//! Read-only side-by-side diff view for a Pull Request. Builds an in-memory
//! `MultiBuffer` of all changed files: each file gets a synthetic `Buffer` with
//! the PR's HEAD content, plus a `BufferDiff` whose base text is the file's
//! content at the PR's BASE sha. The whole thing renders as the same kind of
//! multibuffer Zed uses for "Uncommitted Changes" — but the working tree is
//! never touched. Nothing is checked out, nothing is staged.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{
    DiffViewStyle, Editor, EditorEvent, EditorSettings, MultiBuffer, SplittableEditor,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, DisplayRow},
};
use gpui::SharedUri;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString,
    Subscription, Task, WeakEntity, Window,
};
use futures::stream::{FuturesOrdered, StreamExt as _};
use http_client::HttpClient;
use language::{
    Buffer, Capability, DiskState, File as LangFile, LanguageRegistry, LineEnding,
    OffsetRangeExt as _, ReplicaId, Rope, TextBuffer, ToPoint as _,
};
use text::{Bias, Point};
use multi_buffer::PathKey;
use project::{Project, WorktreeId};
use settings::Settings as _;
use ui::{prelude::*};
use util::paths::PathStyle;
use util::rel_path::RelPath;
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::github_api::{GitHubClient, PullRequest, RepoCoords, ReviewComment};

pub struct PrFilesView {
    pr: PullRequest,
    repo: RepoCoords,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    http: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,
    multibuffer: Entity<MultiBuffer>,
    editor: Option<Entity<SplittableEditor>>,
    state: ContentState,
    progress: Option<String>,
    /// Maps PR file paths to their synthetic head buffer, used to anchor
    /// review-comment blocks once all files have loaded.
    file_buffers: collections::HashMap<String, Entity<Buffer>>,
    /// Local IDs of comments already posted to GitHub. Prevents double-post
    /// when the editor's `ReviewCommentsChanged` event fires multiple times.
    posted_comment_ids: collections::HashSet<usize>,
    /// Status banner above the diff (e.g. "Posting comment on src/foo.rs:42…").
    post_status: Option<String>,
    _fetch_task: Option<Task<()>>,
    _editor_subscription: Option<Subscription>,
    _post_tasks: Vec<Task<()>>,
}

enum ContentState {
    Loading,
    Loaded,
    Error(String),
}

impl PrFilesView {
    pub fn new(
        pr: PullRequest,
        repo: RepoCoords,
        project: Entity<Project>,
        workspace: WeakEntity<Workspace>,
        http: Arc<dyn HttpClient>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let multibuffer = cx.new(|cx| {
            let mut mb = MultiBuffer::new(Capability::ReadOnly);
            mb.set_all_diff_hunks_expanded(cx);
            mb
        });

        let mut this = Self {
            pr,
            repo,
            project,
            workspace,
            http,
            focus_handle,
            multibuffer,
            editor: None,
            state: ContentState::Loading,
            progress: None,
            file_buffers: Default::default(),
            posted_comment_ids: Default::default(),
            post_status: None,
            _fetch_task: None,
            _editor_subscription: None,
            _post_tasks: Vec::new(),
        };
        this.fetch(window, cx);
        this
    }

    fn fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let pr = self.pr.clone();
        let http = self.http.clone();
        let language_registry = self.project.read(cx).languages().clone();

        self.state = ContentState::Loading;
        self.progress = Some("Fetching PR file list…".into());
        cx.notify();

        let task = cx.spawn_in(window, async move |this, cx| {
            let token = match util::github_auth::github_token().await {
                Some(t) => t.to_string(),
                None => {
                    this.update(cx, |this, cx| {
                        this.state = ContentState::Error(
                            "No GitHub token. Run `gh auth login` and reopen.".into(),
                        );
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let client = GitHubClient::new(http, token);

            let files = match client.get_pr_files(&repo, pr.number).await {
                Ok(f) => f,
                Err(err) => {
                    this.update(cx, |this, cx| {
                        this.state = ContentState::Error(format!("Failed to list files: {err:#}"));
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };

            let total = files.len();
            let pr_number = pr.number;

            // Phase 1: fan out base+head fetches with bounded concurrency so
            // big PRs don't load file-by-file. Order is preserved via
            // FuturesOrdered so excerpts still appear in PR order.
            const CONCURRENCY: usize = 6;
            let mut fetched_count = 0usize;
            let mut futures = FuturesOrdered::new();
            for (i, file) in files.iter().cloned().enumerate() {
                let client = client.clone();
                let repo = repo.clone();
                let base_ref = pr.base_ref.to_string();
                let head_sha = pr.head_sha.clone();
                futures.push_back(async move {
                    let base_text: String = if file.status == "added" {
                        String::new()
                    } else {
                        client
                            .get_blob_content(&repo, &base_ref, &file.filename)
                            .await
                            .unwrap_or_default()
                    };
                    let head_text: String = if file.status == "removed" {
                        String::new()
                    } else {
                        client
                            .get_blob_content(&repo, &head_sha, &file.filename)
                            .await
                            .unwrap_or_default()
                    };
                    (i, file.filename, base_text, head_text)
                });
                if futures.len() >= CONCURRENCY {
                    if let Some((i, path, base_text, head_text)) = futures.next().await {
                        fetched_count += 1;
                        let _ = this.update(cx, |this, cx| {
                            this.progress =
                                Some(format!("Fetching {fetched_count} of {total}: {path}"));
                            cx.notify();
                        });
                        if let Err(err) = add_file_to_multibuffer(
                            this.clone(),
                            cx,
                            pr_number,
                            i as u64,
                            path,
                            base_text,
                            head_text,
                            language_registry.clone(),
                        )
                        .await
                        {
                            log::error!("pr_files_view: failed to add file: {err:?}");
                        }
                    }
                }
            }
            // Drain remaining.
            while let Some((i, path, base_text, head_text)) = futures.next().await {
                fetched_count += 1;
                let _ = this.update(cx, |this, cx| {
                    this.progress = Some(format!("Fetching {fetched_count} of {total}: {path}"));
                    cx.notify();
                });
                if let Err(err) = add_file_to_multibuffer(
                    this.clone(),
                    cx,
                    pr_number,
                    i as u64,
                    path,
                    base_text,
                    head_text,
                    language_registry.clone(),
                )
                .await
                {
                    log::error!("pr_files_view: failed to add file: {err:?}");
                }
            }

            // Build the editor now that all files are in the multibuffer.
            let editor_handle = this
                .update_in(cx, |this, window, cx| {
                    let mb = this.multibuffer.clone();
                    let project = this.project.clone();
                    let workspace_entity = match this.workspace.upgrade() {
                        Some(w) => w,
                        None => {
                            this.state = ContentState::Error(
                                "Workspace closed before files finished loading.".into(),
                            );
                            cx.notify();
                            return None;
                        }
                    };
                    let style = EditorSettings::get_global(cx).diff_view_style;
                    let editor = cx.new(|cx| {
                        let splittable = SplittableEditor::new(
                            style,
                            mb,
                            project,
                            workspace_entity,
                            window,
                            cx,
                        );
                        // Match claude-review's review-mode UX: per-line "+"
                        // button on the head editor, drag-to-multiselect, and
                        // an inline composer overlay below the selected hunk.
                        splittable.update_editors(cx, |editor, cx| {
                            editor.set_read_only(true);
                            // Hide breakpoint dots / diagnostics / runnable
                            // indicators — none of them mean anything for a
                            // PR diff that isn't in the local working tree.
                            editor.set_show_breakpoints(false, cx);
                            editor.disable_inline_diagnostics();
                        });
                        let rhs = splittable.rhs_editor().clone();
                        rhs.update(cx, |editor, cx| {
                            editor.set_show_diff_review_button(true, cx);
                        });
                        splittable
                    });
                    // Subscribe to the head editor's review-comment events so
                    // every locally-stored comment fans out as a GitHub line
                    // comment without the user touching a separate composer.
                    let rhs = editor.read(cx).rhs_editor().clone();
                    let sub = cx.subscribe(&rhs, |this, editor, event: &EditorEvent, cx| {
                        if matches!(event, EditorEvent::ReviewCommentsChanged { .. }) {
                            this.flush_new_review_comments(&editor, cx);
                        }
                    });
                    this._editor_subscription = Some(sub);
                    this.editor = Some(editor.clone());
                    this.state = ContentState::Loaded;
                    this.progress = None;
                    cx.notify();
                    Some(editor)
                })
                .ok()
                .flatten();

            // Fetch existing review comments and inject them into the head
            // editor's claude-review storage so they render in the EXACT same
            // inline overlay UI as locally authored comments — same border,
            // bg, avatar layout. Each commented line gets an auto-opened
            // overlay; the user can dismiss it or use the prompt editor to
            // reply (which posts a new review comment to GitHub).
            let review_comments = client.get_review_comments(&repo, pr_number).await;
            if let (Some(editor), Ok(comments)) = (editor_handle, review_comments) {
                let _ = this.update_in(cx, |this, window, cx| {
                    install_remote_comments(this, &editor, &comments, window, cx);
                });
            }
        });
        self._fetch_task = Some(task);
    }

    /// Walks the head editor's locally-stored review comments, finds the ones
    /// we haven't shipped to GitHub yet, and POSTs each as a one-comment
    /// review against the PR's head sha.
    fn flush_new_review_comments(
        &mut self,
        rhs_editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) {
        let mb = rhs_editor.read(cx).buffer().clone();
        let mb_snapshot = mb.read(cx).snapshot(cx);

        let mut to_post: Vec<(usize, String, u32, String)> = Vec::new();
        for (hunk_key, comments) in rhs_editor.read(cx).all_review_comments() {
            for comment in comments {
                if self.posted_comment_ids.contains(&comment.id) {
                    continue;
                }
                let Some((text_anchor, buffer_snapshot)) =
                    mb_snapshot.anchor_to_buffer_anchor(comment.range.start)
                else {
                    continue;
                };
                let line = text_anchor.to_point(buffer_snapshot).row + 1;
                let path = hunk_key
                    .file_path
                    .display(util::paths::PathStyle::local())
                    .to_string();
                to_post.push((comment.id, path, line, comment.comment.clone()));
            }
        }

        if to_post.is_empty() {
            return;
        }

        for (id, path, line, body) in to_post {
            self.posted_comment_ids.insert(id);
            self.post_status = Some(format!("Posting comment on {path}:L{line}…"));
            cx.notify();

            let repo = self.repo.clone();
            let pr_number = self.pr.number;
            let head_sha = self.pr.head_sha.clone();
            let http = self.http.clone();
            let task = cx.spawn(async move |this, cx| {
                let token = match util::github_auth::github_token().await {
                    Some(t) => t.to_string(),
                    None => {
                        let _ = this.update(cx, |this, cx| {
                            this.post_status = Some("No GitHub token.".into());
                            cx.notify();
                        });
                        return;
                    }
                };
                let client = GitHubClient::new(http, token);
                let result = client
                    .post_review_line_comment(&repo, pr_number, &head_sha, &path, line, &body)
                    .await;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            this.post_status =
                                Some(format!("Posted comment on {path}:L{line} ✓"));
                        }
                        Err(err) => {
                            log::error!("pr_files_view: post line comment failed: {err:?}");
                            this.post_status = Some(format!("Error posting: {err:#}"));
                        }
                    }
                    cx.notify();
                });
            });
            self._post_tasks.push(task);
        }
    }
}

#[allow(dead_code)]
fn _legacy_cursor_line(
    editor: &Entity<SplittableEditor>,
    file_buffers: &collections::HashMap<String, Entity<Buffer>>,
    cx: &gpui::App,
) -> Option<(String, u32)> {
    let inner_editor = editor.read(cx).focused_editor().clone();
    let inner = inner_editor.read(cx);
    let cursor = inner.selections.newest_anchor().head();
    let mb_handle = inner.buffer().clone();
    let mb_snapshot = mb_handle.read(cx).snapshot(cx);
    let (text_anchor, buffer_snapshot) = mb_snapshot.anchor_to_buffer_anchor(cursor)?;
    let buffer_id = buffer_snapshot.remote_id();
    let point = text_anchor.to_point(buffer_snapshot);
    let line = point.row + 1;
    for (path, buffer) in file_buffers.iter() {
        if buffer.read(cx).remote_id() == buffer_id {
            return Some((path.clone(), line));
        }
    }
    None
}

/// Pushes existing GitHub review comments into the head editor's
/// claude-review storage. Each commented line gets a `show_diff_review_overlay`
/// call so the same overlay UI used for new comments hosts the remote thread.
/// Comments are stored via `add_remote_review_comment`, so the overlay's
/// per-comment render path picks up author + avatar.
fn install_remote_comments(
    this: &mut PrFilesView,
    editor: &Entity<SplittableEditor>,
    comments: &[ReviewComment],
    window: &mut Window,
    cx: &mut Context<PrFilesView>,
) {
    use std::collections::HashMap as StdHashMap;

    let rhs = editor.read(cx).rhs_editor().clone();

    let mut grouped: StdHashMap<(String, u32), Vec<&ReviewComment>> = StdHashMap::new();
    for c in comments {
        let Some(line) = c.line else { continue };
        grouped.entry((c.path.to_string(), line)).or_default().push(c);
    }

    for ((path, line), thread) in grouped {
        let Some(buffer) = this.file_buffers.get(&path) else { continue };
        let buffer_snapshot = buffer.read(cx).snapshot();
        let row = line.saturating_sub(1);
        let buffer_point = Point::new(row, 0);
        let text_anchor = buffer_snapshot.anchor_at(buffer_point, Bias::Left);

        let mb_handle = rhs.read(cx).buffer().clone();
        let mb_snapshot = mb_handle.read(cx).snapshot(cx);
        let Some(mb_anchor) = mb_snapshot.anchor_in_excerpt(text_anchor) else {
            continue;
        };

        let mb_point = editor::ToPoint::to_point(&mb_anchor, &mb_snapshot);
        let editor_snapshot = rhs.update(cx, |e, cx| e.snapshot(window, cx));
        let display_point = editor_snapshot
            .display_snapshot
            .point_to_display_point(mb_point, Bias::Left);
        let display_row = display_point.row();

        for c in thread {
            let avatar_uri: Option<SharedUri> = c
                .avatar_url
                .as_deref()
                .map(|s| SharedUri::from(SharedString::from(s.to_owned())));
            let author = SharedString::from(c.author.to_string());
            let body = c.body.clone();
            let id = rhs.update(cx, |editor, cx| {
                editor.inject_remote_review_comment(
                    display_row,
                    body,
                    author,
                    avatar_uri,
                    window,
                    cx,
                )
            });
            if let Some(id) = id {
                // Mark as already-posted so the ReviewCommentsChanged
                // subscription doesn't fan it back out to GitHub.
                this.posted_comment_ids.insert(id);
            }
        }
    }
    cx.notify();
}

async fn add_file_to_multibuffer(
    this: WeakEntity<PrFilesView>,
    cx: &mut gpui::AsyncWindowContext,
    pr_number: u32,
    sort_index: u64,
    path: String,
    base_text: String,
    head_text: String,
    language_registry: Arc<LanguageRegistry>,
) -> Result<()> {
    use gpui::AppContext as _;
    use gpui::Entity;

    let language = language_registry
        .load_language_for_file_path(Path::new(&path))
        .await
        .ok();

    // Synthesize a File for the buffer so the multibuffer's header banner
    // shows the file path instead of "untitled".
    let rel_path: Arc<RelPath> = {
        let fallback = format!("pr-{pr_number}/{path}");
        let rel = RelPath::unix(&path).or_else(|_| RelPath::unix(&fallback));
        rel.map_err(|e| anyhow::anyhow!("invalid path {path}: {e}"))?
            .into_arc()
    };
    let display_name = Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&path)
        .to_string();
    let file: Arc<dyn LangFile> = Arc::new(PrFile {
        path: rel_path.clone(),
        display_name,
        full_path: PathBuf::from(&path),
    });

    // Build the head Buffer with the synthetic File attached. Mirrors the
    // pattern used by git_ui::commit_view::build_buffer for read-only blobs.
    let mut head_normalized = head_text.clone();
    LineEnding::normalize(&mut head_normalized);
    let line_ending = LineEnding::detect(&head_normalized);
    let rope = Rope::from(head_normalized);
    let file_for_buffer = file.clone();
    let buffer: Entity<Buffer> = cx.new(|cx| {
        let text_buffer = TextBuffer::new_normalized(
            ReplicaId::LOCAL,
            cx.entity_id().as_non_zero_u64().into(),
            line_ending,
            rope,
        );
        let mut buffer =
            Buffer::build(text_buffer, Some(file_for_buffer), Capability::ReadOnly);
        if let Some(language) = language.clone() {
            buffer.set_language_async(Some(language), cx);
        }
        buffer
    });

    let buffer_snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let text_snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());

    // Build the diff entity, with secondary diff (so the "Uncommitted Changes"
    // style decoration works correctly).
    let secondary_diff = cx.new(|cx| BufferDiff::new(&buffer_snapshot, cx));
    let secondary_update_task = secondary_diff.update(cx, |d, cx| {
        d.update_diff(
            text_snapshot.clone(),
            Some(Arc::from(base_text.clone())),
            Some(false),
            language.clone(),
            cx,
        )
    });
    let secondary_state = secondary_update_task.await;
    let snapshot_for_secondary = buffer_snapshot.clone();
    let secondary_state_for_set = secondary_state.clone();
    let set_task = secondary_diff.update(cx, move |d, cx| {
        d.set_snapshot(secondary_state_for_set, &snapshot_for_secondary, cx)
    });
    set_task.await;

    let diff = cx.new(|cx| BufferDiff::new(&buffer_snapshot, cx));
    let snapshot_for_primary = buffer_snapshot.clone();
    let lang_for_diff = language.clone();
    let registry_for_diff = language_registry.clone();
    let primary_set_task = diff.update(cx, move |d, cx| {
        d.language_changed(lang_for_diff, Some(registry_for_diff), cx);
        d.set_secondary_diff(secondary_diff);
        d.set_snapshot(secondary_state, &snapshot_for_primary, cx)
    });
    primary_set_task.await;

    // Compute the hunk ranges so the multibuffer expands them as excerpts.
    let path_key = PathKey::with_sort_prefix(sort_index, rel_path);

    let path_for_map = path.clone();
    let buffer_for_map = buffer.clone();
    this.update(cx, |this, cx| {
        this.multibuffer.update(cx, |mb, cx| {
            let buffer_for_ranges = buffer.clone();
            let hunk_ranges = {
                let buffer_read = buffer_for_ranges.read(cx);
                diff.read(cx)
                    .snapshot(cx)
                    .hunks_intersecting_range(
                        text::Anchor::min_for_buffer(buffer_read.remote_id())
                            ..text::Anchor::max_for_buffer(buffer_read.remote_id()),
                        buffer_read,
                    )
                    .map(|h| h.buffer_range.to_point(buffer_read))
                    .collect::<Vec<_>>()
            };
            mb.set_excerpts_for_path(
                path_key,
                buffer.clone(),
                hunk_ranges,
                multi_buffer::excerpt_context_lines(cx),
                cx,
            );
            mb.add_diff(diff, cx);
        });
        this.file_buffers.insert(path_for_map, buffer_for_map);
        cx.notify();
    })?;
    Ok(())
}

/// Synthetic `language::File` for PR-fetched buffers. Provides a path so the
/// multibuffer renders a file-name header instead of "untitled". Marks itself
/// as `Historic` so Zed treats it as read-only / VCS-derived (matches how
/// `commit_view::GitBlob` handles past commits).
struct PrFile {
    path: Arc<RelPath>,
    display_name: String,
    full_path: PathBuf,
}

impl LangFile for PrFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic { was_deleted: false }
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.full_path.clone()
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::local()
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        &self.display_name
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        WorktreeId::from_proto(0)
    }

    fn to_proto(&self, _cx: &App) -> language::proto::File {
        unimplemented!("PrFile::to_proto: PR diff buffers are local-only")
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        false
    }
}

impl EventEmitter<()> for PrFilesView {}

impl Focusable for PrFilesView {
    fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        if let Some(editor) = &self.editor {
            editor.read(cx).focused_editor().focus_handle(cx)
        } else {
            self.focus_handle.clone()
        }
    }
}

impl Item for PrFilesView {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content_text(&self, _: usize, _: &gpui::App) -> SharedString {
        format!("PR #{} files", self.pr.number).into()
    }

    fn tab_icon(&self, _: &Window, _: &gpui::App) -> Option<Icon> {
        Some(Icon::new(IconName::FileDiff).color(Color::Muted))
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("PR Files Opened")
    }

    fn can_split(&self) -> bool {
        false
    }

    fn tab_content(
        &self,
        params: TabContentParams,
        _window: &Window,
        cx: &gpui::App,
    ) -> AnyElement {
        Label::new(self.tab_content_text(0, cx))
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }
}

impl PrFilesView {
    fn render_status_banner(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let msg = self.post_status.clone()?;
        Some(
            h_flex()
                .flex_shrink_0()
                .px_3()
                .py_1()
                .gap_2()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().panel_background)
                .child(
                    Icon::new(IconName::Chat)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(msg).size(LabelSize::XSmall).color(Color::Muted))
                .into_any_element(),
        )
    }
}

impl Render for PrFilesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match (&self.state, self.editor.clone()) {
            (ContentState::Loaded, Some(editor)) => v_flex()
                .size_full()
                .min_h_0()
                .when_some(self.render_status_banner(cx), |col, banner| col.child(banner))
                .child(div().flex_1().min_h_0().child(editor))
                .into_any_element(),
            (ContentState::Error(msg), _) => v_flex()
                .size_full()
                .p_4()
                .gap_2()
                .child(Label::new("Failed to load PR files.").color(Color::Error))
                .child(Label::new(msg.clone()).size(LabelSize::Small).color(Color::Muted))
                .into_any_element(),
            _ => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(
                    Label::new(format!(
                        "Loading PR #{} files…",
                        self.pr.number
                    ))
                    .color(Color::Muted),
                )
                .when_some(self.progress.clone(), |col, msg| {
                    col.child(Label::new(msg).size(LabelSize::Small).color(Color::Muted))
                })
                .into_any_element(),
        };

        v_flex()
            .key_context("PrFilesView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(body)
    }
}
