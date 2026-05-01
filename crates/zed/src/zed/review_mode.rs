//! Review-mode runtime for `zed --review <repo>`.
//!
//! When the CLI sends a [`cli::CliRequest::Review`], we open a dedicated
//! workspace with the Project Diff multibuffer focused, then hand the
//! IPC response sink to this module. The user leaves line-level
//! comments on the diff using Zed's existing review-comment infra
//! ([`editor::Editor::add_review_comment`]) and clicks "Send Review".
//!
//! On Send Review we walk the saved comments and the working tree's
//! git status, build a JSON payload that matches the
//! `claude-review/docs/SPECIFICATION.md` §6.2 schema, write it to
//! stdout via the response sink, and exit 0. On window close without
//! Send Review we emit a stderr message and exit non-zero.

use anyhow::{Context as _, Result, anyhow};
use cli::{CliResponse, CliResponseSink};
use editor::{DiffHunkKey, Editor, StoredReviewComment, actions::SendReviewToAgent};
use feature_flags::FeatureFlagAppExt as _;
use git_ui::git_panel::GitPanel;
use git_ui::project_diff::ProjectDiff;
use gpui::{App, Entity, Global, ReadGlobal, UpdateGlobal, WeakEntity, Window};
use multi_buffer::{AnchorRangeExt as _, MultiBufferSnapshot};
use parking_lot::Mutex;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use util::ResultExt as _;
use workspace::Workspace;

/// Thread-safe holder for the IPC response sink. The sink is `Send`
/// but not `Sync`; an `Arc<Mutex<Option<Box<dyn _>>>>` lets the global
/// be shared and the sink consumed exactly once when we send the
/// payload (or the cancel notification).
type SinkSlot = Arc<Mutex<Option<Box<dyn CliResponseSink>>>>;

/// Process-wide review-mode state. Set when Zed receives
/// [`cli::CliRequest::Review`]; consumed when the user clicks Send
/// Review or closes the window without sending.
pub struct ReviewMode {
    pub repo_root: PathBuf,
    pub head_sha: Option<String>,
    pub head_branch: Option<String>,
    pub workspace: Option<WeakEntity<Workspace>>,
    pub window: Option<gpui::AnyWindowHandle>,
    pub sink: SinkSlot,
}

impl Global for ReviewMode {}

/// True iff the running Zed instance was launched via `zed --review`.
pub fn is_active(cx: &App) -> bool {
    cx.try_global::<ReviewMode>().is_some()
}

/// Initialize review-mode state. Called once, before the review
/// workspace is opened. Also force-enables the `diff-review` feature
/// flag so the gutter "Add Review" button (the only way to start a
/// drag-select range) renders without the user having to be on the
/// flag's allow list.
pub fn enter(repo_root: PathBuf, sink: Box<dyn CliResponseSink>, cx: &mut App) {
    let head_sha = git_head_sha(&repo_root).ok();
    let head_branch = git_head_branch(&repo_root).ok();
    cx.update_flags(false, vec!["diff-review".to_string()]);
    git_ui::review_mode_marker::activate(cx, std::sync::Arc::new(|cx| run_send_review(cx)));
    cx.set_global(ReviewMode {
        repo_root,
        head_sha,
        head_branch,
        workspace: None,
        window: None,
        sink: Arc::new(Mutex::new(Some(sink))),
    });
    register_global_action(cx);
}

/// Activates the Git panel so the user lands on the file-status
/// sidebar (the "git" tab in Zed's bottom dock) — the analog of
/// SPECIFICATION.md §5.4's "sidebar — file tree". Called once the
/// review workspace has rendered.
pub fn focus_git_panel(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    workspace.focus_panel::<GitPanel>(window, cx);
}

/// Records the active workspace and its window. Called after the
/// review workspace has been opened so the global action handler can
/// route Send Review back to it.
pub fn attach_workspace(
    workspace: WeakEntity<Workspace>,
    window: gpui::AnyWindowHandle,
    cx: &mut App,
) {
    if !is_active(cx) {
        return;
    }
    ReviewMode::update_global(cx, |this, _| {
        this.workspace = Some(workspace);
        this.window = Some(window);
    });
}

/// Registers a global `SendReviewToAgent` action handler that runs at
/// the end of the bubble phase — i.e. only after every element-level
/// handler has had a chance. Workspace-level `register_action`
/// listeners get attached on the next render, which races with the
/// user clicking Send Review immediately after the window opens; a
/// global handler sidesteps that race because [`App::on_action`]
/// installs the listener synchronously.
pub fn register_global_action(cx: &mut App) {
    cx.on_action::<SendReviewToAgent>(|_, cx| {
        run_send_review(cx);
    });
}

fn run_send_review(cx: &mut App) {
    if !is_active(cx) {
        return;
    }
    cx.spawn(async move |cx| {
        let Some(workspace_weak) = cx.update(|cx| {
            if !is_active(cx) {
                return None;
            }
            ReviewMode::global(cx).workspace.clone()
        }) else {
            return;
        };
        let _ = workspace_weak.update_in(cx, |workspace, window, cx| {
            do_send_review(workspace, window, cx);
        });
    })
    .detach();
}

fn do_send_review(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    auto_save_open_composers(workspace, window, cx);

    let payload = match build_payload(workspace, cx) {
        Ok(payload) => payload,
        Err(err) => {
            send_error(&format!("review: failed to build payload: {err:#}"), 1, cx);
            window.remove_window();
            return;
        }
    };

    let json = match serde_json::to_string_pretty(&payload) {
        Ok(s) => s,
        Err(err) => {
            send_error(&format!("review: failed to serialize payload: {err}"), 1, cx);
            window.remove_window();
            return;
        }
    };

    send_payload(json, cx);
    window.remove_window();
    // If the review window was the only one open, fully quit the app
    // — without this macOS keeps the process alive in the dock even
    // though every window is gone. If the user had other Zed windows
    // open we leave them alone and only close this one.
    if cx.windows().len() <= 1 {
        cx.quit();
    }
}

/// Called from `handle_review_request` if opening the workspace
/// fails. Drains the sink with a stderr message and exits non-zero.
pub fn send_payload_failure(msg: &str, cx: &mut App) {
    send_error(msg, 1, cx);
}

/// Called when the review window is closed without Send Review.
pub fn handle_cancel(cx: &mut App) {
    if !is_active(cx) {
        return;
    }
    if let Some(sink) = take_sink(cx) {
        sink.send(CliResponse::Stderr {
            message: "review cancelled — window closed without 'Send Review'".to_string(),
        })
        .log_err();
        sink.send(CliResponse::Exit { status: 130 }).log_err();
    }
}

fn take_sink(cx: &mut App) -> Option<Box<dyn CliResponseSink>> {
    if !is_active(cx) {
        return None;
    }
    let slot = ReviewMode::global(cx).sink.clone();
    let sink = slot.lock().take();
    sink
}

fn send_error(msg: &str, status: i32, cx: &mut App) {
    if let Some(sink) = take_sink(cx) {
        sink.send(CliResponse::Stderr {
            message: msg.to_string(),
        })
        .log_err();
        sink.send(CliResponse::Exit { status }).log_err();
    }
}

/// Sends the JSON payload and exit-zero to the CLI. Window close is
/// done by the caller via `Window::remove_window` so the user sees an
/// instant visual confirmation; relying on `cx.quit()` defers shutdown
/// to the OS routine, which on macOS gives the appearance of needing
/// to click Send Review twice.
fn send_payload(json: String, cx: &mut App) {
    if let Some(sink) = take_sink(cx) {
        sink.send(CliResponse::Stdout { message: json }).log_err();
        sink.send(CliResponse::Exit { status: 0 }).log_err();
    }
}

#[derive(Debug, Serialize)]
struct ReviewPayload {
    repo_root: String,
    head_sha: Option<String>,
    head_branch: Option<String>,
    files: Vec<FilePayload>,
    comments: Vec<CommentPayload>,
}

#[derive(Debug, Serialize)]
struct FilePayload {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    status: &'static str,
    old_content: FileContent,
    new_content: FileContent,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum FileContent {
    Text { text: String },
    Binary,
    Missing,
}

#[derive(Debug, Serialize)]
struct CommentPayload {
    id: usize,
    file_path: String,
    side: &'static str,
    line_start: u32,
    line_end: u32,
    body: String,
}

fn build_payload(workspace: &Workspace, cx: &App) -> Result<ReviewPayload> {
    let state = ReviewMode::global(cx);
    let repo_root = state.repo_root.clone();
    let head_sha = state.head_sha.clone();
    let head_branch = state.head_branch.clone();

    let files = collect_files(&repo_root)?;
    let comments = collect_comments(workspace, cx);

    Ok(ReviewPayload {
        repo_root: repo_root.to_string_lossy().into_owned(),
        head_sha,
        head_branch,
        files,
        comments,
    })
}

/// Walks `git status --porcelain=v1 -z` for the file list, then reads
/// HEAD blobs and working-tree contents per side.
fn collect_files(repo_root: &Path) -> Result<Vec<FilePayload>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=no",
        ])
        .output()
        .context("git status")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let mut files = parse_porcelain_z(&output.stdout, repo_root)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn parse_porcelain_z(bytes: &[u8], repo_root: &Path) -> Result<Vec<FilePayload>> {
    let mut out = Vec::new();
    let mut iter = bytes.split(|&b| b == 0).peekable();

    while let Some(record) = iter.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 3 {
            continue;
        }
        let xy = &record[..2];
        let path_bytes = &record[3..];
        let xy0 = xy[0] as char;
        let xy1 = xy[1] as char;
        let path = String::from_utf8_lossy(path_bytes).into_owned();

        // Renames carry a second NUL-delimited field (the old path).
        let (status, old_path) = match (xy0, xy1) {
            ('?', '?') => ("untracked", None),
            ('A', _) | (_, 'A') => ("added", None),
            ('D', _) | (_, 'D') => ("deleted", None),
            ('R', _) | (_, 'R') => {
                let old = iter
                    .next()
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .unwrap_or_default();
                ("renamed", Some(old))
            }
            ('C', _) | (_, 'C') => ("modified", None),
            _ => ("modified", None),
        };

        let (old_content, new_content) = match status {
            "untracked" | "added" => (
                FileContent::Missing,
                read_worktree(repo_root, &path).unwrap_or(FileContent::Missing),
            ),
            "deleted" => (
                read_head_blob(repo_root, &path).unwrap_or(FileContent::Missing),
                FileContent::Missing,
            ),
            "renamed" => {
                let from = old_path.as_deref().unwrap_or("");
                (
                    if from.is_empty() {
                        FileContent::Missing
                    } else {
                        read_head_blob(repo_root, from).unwrap_or(FileContent::Missing)
                    },
                    read_worktree(repo_root, &path).unwrap_or(FileContent::Missing),
                )
            }
            _ => (
                read_head_blob(repo_root, &path).unwrap_or(FileContent::Missing),
                read_worktree(repo_root, &path).unwrap_or(FileContent::Missing),
            ),
        };

        out.push(FilePayload {
            path,
            old_path,
            status,
            old_content,
            new_content,
        });
    }

    Ok(out)
}

fn read_head_blob(repo_root: &Path, path: &str) -> Result<FileContent> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show", &format!("HEAD:{path}")])
        .output()
        .context("git show HEAD:<path>")?;
    if !output.status.success() {
        return Ok(FileContent::Missing);
    }
    Ok(bytes_to_file_content(output.stdout))
}

fn read_worktree(repo_root: &Path, path: &str) -> Result<FileContent> {
    let abs = repo_root.join(path);
    let bytes = std::fs::read(&abs).context("read worktree file")?;
    Ok(bytes_to_file_content(bytes))
}

fn bytes_to_file_content(bytes: Vec<u8>) -> FileContent {
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return FileContent::Binary;
    }
    match String::from_utf8(bytes) {
        Ok(text) => FileContent::Text { text },
        Err(_) => FileContent::Binary,
    }
}

fn git_head_sha(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("git rev-parse HEAD")?;
    if !output.status.success() {
        return Err(anyhow!("no HEAD"));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn git_head_branch(repo_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .context("git symbolic-ref")?;
    if !output.status.success() {
        return Err(anyhow!("detached HEAD"));
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

/// Drains every open diff-review composer with a non-empty draft
/// body into `Editor::stored_review_comments` so they appear in the
/// payload. Empty composers are left as-is and discarded silently
/// when the window closes (per SPECIFICATION.md §4.6).
fn auto_save_open_composers(
    workspace: &Workspace,
    window: &mut Window,
    cx: &mut gpui::Context<Workspace>,
) {
    let project_diffs: Vec<Entity<ProjectDiff>> =
        workspace.items_of_type::<ProjectDiff>(cx).collect();
    for project_diff in project_diffs {
        let split = project_diff.read(cx).editor().clone();
        let mut editors: Vec<Entity<Editor>> = vec![split.read(cx).rhs_editor().clone()];
        if let Some(lhs) = split.read(cx).lhs_editor() {
            editors.push(lhs.clone());
        }
        for editor_entity in editors {
            editor_entity.update(cx, |editor, cx| {
                editor.submit_all_diff_review_comments(window, cx);
            });
        }
    }
}

/// Walks every Project Diff in the workspace and harvests its
/// review comments. Comments are stored on the Editor instance
/// inside `ProjectDiff`'s [`editor::SplittableEditor`], on both
/// the LHS (old side) and RHS (new side) when the split is open.
fn collect_comments(workspace: &Workspace, cx: &App) -> Vec<CommentPayload> {
    let mut out = Vec::new();

    for project_diff in workspace.items_of_type::<ProjectDiff>(cx) {
        let pd = project_diff.read(cx);
        let split = pd.editor().read(cx);
        let mut editors: Vec<Entity<Editor>> = vec![split.rhs_editor().clone()];
        if let Some(lhs) = split.lhs_editor() {
            editors.push(lhs.clone());
        }
        for editor_entity in editors {
            let editor = editor_entity.read(cx);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            for (hunk_key, comments) in editor.all_review_comments() {
                for comment in comments {
                    let Some(payload) = comment_to_payload(hunk_key, comment, &snapshot) else {
                        continue;
                    };
                    out.push(payload);
                }
            }
        }
    }
    out
}

fn comment_to_payload(
    hunk_key: &DiffHunkKey,
    comment: &StoredReviewComment,
    snapshot: &MultiBufferSnapshot,
) -> Option<CommentPayload> {
    let range = comment.range.to_point(snapshot);
    let line_start = range.start.row + 1;
    let line_end = range.end.row.saturating_add(1).max(line_start);
    Some(CommentPayload {
        id: comment.id,
        file_path: hunk_key.file_path.as_unix_str().to_string(),
        // TODO Phase 2b — derive side from anchor position vs. diff
        // hunk's deletion phantom rows. v0 hardcodes "new" since the
        // overwhelming majority of comments target added/changed code
        // (see SPECIFICATION.md §6.2 note).
        side: "new",
        line_start,
        line_end,
        body: comment.comment.trim().to_string(),
    })
}
