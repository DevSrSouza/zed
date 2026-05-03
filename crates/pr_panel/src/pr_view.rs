//! Workspace tab for a single Pull Request: overview-style page (header,
//! markdown description, stats, mergeability, comments with author avatars,
//! review composer). The actual side-by-side file diff is opened in a separate
//! tab via the "Files (N)" button, which checks out the PR locally with `gh pr
//! checkout` and then dispatches the existing `git::BranchDiff` action so Zed's
//! built-in branch-diff multibuffer renders the changes.
//!
//! What this view does NOT do (yet): pin inline review comments to specific
//! diff lines, allow replying to threads, or render PR labels / requested
//! reviewers.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use editor::Editor;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, Render, SharedString, Task,
    WeakEntity, Window,
};
use http_client::HttpClient;
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use project::Project;
use ui::{Avatar, Tooltip, prelude::*};
use workspace::{
    Item, Workspace,
    item::{ItemEvent, TabContentParams},
};

use crate::github_api::{
    CheckRun, GitHubClient, IssueComment, PrCommit, PrDetails, PullRequest, RepoCoords,
    ReviewComment, ReviewEvent,
};

pub struct PrView {
    pr: PullRequest,
    repo: RepoCoords,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    http: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,

    description: Option<Entity<Markdown>>,
    issue_comments: Vec<IssueComment>,
    review_comments: Vec<ReviewComment>,
    details: Option<PrDetails>,
    file_count: Option<u32>,
    check_runs: Vec<CheckRun>,
    commits: Vec<PrCommit>,

    state: ContentState,
    fetch_task: Option<Task<()>>,
    submit_task: Option<Task<()>>,
    checkout_task: Option<Task<()>>,
    files_task: Option<Task<()>>,

    composer: Entity<Editor>,
    submit_status: Option<String>,
    checkout_status: Option<String>,
    files_status: Option<String>,
}

enum ContentState {
    Loading,
    Loaded,
    Error(String),
}

impl PrView {
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
        let composer = cx.new(|cx| Editor::auto_height(2, 8, window, cx));

        let description = if pr.body.is_empty() {
            None
        } else {
            Some(cx.new(|cx| Markdown::new(pr.body.clone().into(), None, None, cx)))
        };

        let mut this = Self {
            pr,
            repo,
            project,
            workspace,
            http,
            focus_handle,
            description,
            issue_comments: Vec::new(),
            review_comments: Vec::new(),
            details: None,
            file_count: None,
            check_runs: Vec::new(),
            commits: Vec::new(),
            state: ContentState::Loading,
            fetch_task: None,
            submit_task: None,
            checkout_task: None,
            files_task: None,
            composer,
            submit_status: None,
            checkout_status: None,
            files_status: None,
        };
        this.fetch_all(window, cx);
        this
    }

    fn fetch_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.repo.clone();
        let pr_number = self.pr.number;
        let head_sha = self.pr.head_sha.clone();
        let http = self.http.clone();

        self.state = ContentState::Loading;
        cx.notify();

        self.fetch_task = Some(cx.spawn_in(window, async move |this, cx| {
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

            // Fan out every GitHub endpoint concurrently so the overview
            // tab paints in roughly the time of the slowest call instead of
            // their sum (used to be sequential; cold loads felt sluggish).
            let (
                issue_comments,
                review_comments,
                details,
                files,
                commits,
                check_runs,
            ) = futures::join!(
                client.get_issue_comments(&repo, pr_number),
                client.get_review_comments(&repo, pr_number),
                client.get_pr_details(&repo, pr_number),
                client.get_pr_files(&repo, pr_number),
                client.get_pr_commits(&repo, pr_number),
                client.get_check_runs(&repo, &head_sha),
            );

            this.update(cx, |this, cx| {
                this.issue_comments = issue_comments.unwrap_or_default();
                this.review_comments = review_comments.unwrap_or_default();
                this.details = details.ok();
                this.file_count = files.ok().map(|v| v.len() as u32);
                this.commits = commits.unwrap_or_default();
                this.check_runs = check_runs.unwrap_or_default();
                this.state = ContentState::Loaded;
                cx.notify();
            })
            .ok();
        }));
    }

    fn submit_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_review_or_comment(None, window, cx);
    }

    fn submit_review(&mut self, event: ReviewEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.submit_review_or_comment(Some(event), window, cx);
    }

    fn submit_review_or_comment(
        &mut self,
        event: Option<ReviewEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = self.composer.read(cx).text(cx);
        if body.trim().is_empty() && event.is_none() {
            self.submit_status = Some("Type a comment first.".into());
            cx.notify();
            return;
        }
        let repo = self.repo.clone();
        let pr_number = self.pr.number;
        let http = self.http.clone();
        let composer = self.composer.clone();

        self.submit_status = Some("Submitting…".into());
        cx.notify();

        self.submit_task = Some(cx.spawn_in(window, async move |this, cx| {
            let token = match util::github_auth::github_token().await {
                Some(t) => t.to_string(),
                None => {
                    this.update(cx, |this, cx| {
                        this.submit_status = Some("No GitHub token.".into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let client = GitHubClient::new(http, token);

            let result = match event {
                Some(ev) => client.post_review(&repo, pr_number, ev, &body).await,
                None => client.post_issue_comment(&repo, pr_number, &body).await,
            };

            this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(_) => {
                        composer.update(cx, |editor, cx| editor.clear(window, cx));
                        this.submit_status = Some("Submitted ✓".into());
                        this.fetch_all(window, cx);
                    }
                    Err(err) => {
                        log::error!("pr_view: submit failed: {err:?}");
                        this.submit_status = Some(format!("Error: {err:#}"));
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
    }

    fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pr = self.pr.clone();
        let repo = self.repo.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let http = self.http.clone();

        self.files_status = Some("Opening PR diff…".into());
        cx.notify();

        self.files_task = Some(cx.spawn_in(window, async move |_this, cx| {
            workspace
                .update_in(cx, move |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        crate::pr_files_view::PrFilesView::new(
                            pr,
                            repo,
                            project,
                            workspace.weak_handle(),
                            http,
                            window,
                            cx,
                        )
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .ok();
        }));
    }

    fn checkout(&mut self, cx: &mut Context<Self>) {
        let pr_number = self.pr.number;
        let project = self.project.clone();
        let cwd = project
            .read(cx)
            .visible_worktrees(cx)
            .next()
            .map(|wt| wt.read(cx).abs_path().to_path_buf());

        self.checkout_status = Some("Running `gh pr checkout`…".into());
        cx.notify();

        self.checkout_task = Some(cx.spawn(async move |this, cx| {
            let result = run_gh_pr_checkout(pr_number, cwd).await;
            this.update(cx, |this, cx| {
                this.checkout_status = Some(match result {
                    Ok(()) => format!("Checked out PR #{pr_number}"),
                    Err(err) => {
                        log::error!("pr_view: checkout failed: {err:?}");
                        format!("Checkout failed: {err:#}")
                    }
                });
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_header(&self, cx: &Context<Self>) -> AnyElement {
        let pr = &self.pr;
        let url = pr.html_url.clone();

        // Mergeability badge: pill-shaped status pill with icon + text + bg
        // tinted by status. Reads at a glance instead of being a tiny gray
        // word in the corner.
        let status_badge: Option<gpui::AnyElement> = self.details.as_ref().map(|d| {
            let (icon, label, color) = match d.mergeable {
                Some(true) => (IconName::Check, "Mergeable", Color::Success),
                Some(false) => (IconName::Warning, "Conflicts", Color::Error),
                None => (IconName::ArrowCircle, "Checking", Color::Muted),
            };
            h_flex()
                .gap_1()
                .px_2()
                .py_0p5()
                .rounded_full()
                .bg(cx.theme().colors().element_background)
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .child(Icon::new(icon).size(IconSize::XSmall).color(color))
                .child(
                    Label::new(label)
                        .size(LabelSize::XSmall)
                        .color(color)
                        .weight(gpui::FontWeight::SEMIBOLD),
                )
                .into_any_element()
        });

        let stats_label = self.details.as_ref().map(|d| {
            format!(
                "+{} −{} · {} files",
                d.additions, d.deletions, d.changed_files
            )
        });

        let files_label = match self.file_count {
            Some(n) => format!("Files ({n})"),
            None => "Files".to_string(),
        };

        v_flex()
            .px_4()
            .py_3()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when_some(pr.user_avatar_url.clone(), |row, src| {
                        row.child(Avatar::new(src).size(px(20.)))
                    })
                    .child(
                        Icon::new(IconName::PullRequest)
                            .color(if pr.draft { Color::Muted } else { Color::Success }),
                    )
                    .child(
                        Label::new(pr.title.clone())
                            .size(LabelSize::Large)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .child(
                        Label::new(format!("#{}", pr.number))
                            .size(LabelSize::Default)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1())
                    .when_some(status_badge, |row, badge| row.child(badge))
                    .child(
                        Button::new("pr-view-files", files_label)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_files(window, cx)
                            })),
                    )
                    .child(
                        Button::new("pr-view-checkout", "Checkout")
                            .style(ButtonStyle::Outlined)
                            .on_click(cx.listener(|this, _, _, cx| this.checkout(cx))),
                    )
                    .child(
                        IconButton::new("pr-view-open-browser", IconName::Link)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Open on GitHub"))
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Label::new(format!(
                            "by {} · {} → {}",
                            pr.user_login, pr.head_ref, pr.base_ref
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .when_some(stats_label, |row, label| {
                        row.child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .when_some(self.checkout_status.clone(), |row, msg| {
                        row.child(Label::new(msg).size(LabelSize::XSmall).color(Color::Muted))
                    })
                    .when_some(self.files_status.clone(), |row, msg| {
                        row.child(Label::new(msg).size(LabelSize::XSmall).color(Color::Muted))
                    }),
            )
            .into_any_element()
    }

    fn render_check_runs(&self, cx: &Context<Self>) -> AnyElement {
        if self.check_runs.is_empty() {
            return gpui::Empty.into_any_element();
        }
        let header = h_flex()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(format!("CI checks ({})", self.check_runs.len()))
                    .size(LabelSize::Default)
                    .weight(gpui::FontWeight::SEMIBOLD),
            );

        let rows = self.check_runs.iter().enumerate().map(|(i, r)| {
            let (icon, color, status_text): (IconName, Color, SharedString) =
                match (r.status.as_ref(), r.conclusion.as_deref()) {
                    (_, Some("success")) => (IconName::Check, Color::Success, "success".into()),
                    (_, Some("failure")) => (IconName::Close, Color::Error, "failure".into()),
                    (_, Some("cancelled")) => {
                        (IconName::Close, Color::Muted, "cancelled".into())
                    }
                    (_, Some("skipped")) => {
                        (IconName::ArrowRight, Color::Muted, "skipped".into())
                    }
                    (_, Some("neutral")) => (IconName::Dash, Color::Muted, "neutral".into()),
                    (_, Some("timed_out")) => {
                        (IconName::Warning, Color::Error, "timed out".into())
                    }
                    (_, Some("action_required")) => {
                        (IconName::Warning, Color::Warning, "action required".into())
                    }
                    ("queued", _) => (IconName::ArrowCircle, Color::Muted, "queued".into()),
                    ("in_progress", _) => {
                        (IconName::ArrowCircle, Color::Info, "running".into())
                    }
                    _ => (IconName::Dash, Color::Muted, r.status.clone()),
                };
            let url = r.html_url.clone();
            h_flex()
                .id(("ci-row", i))
                .px_4()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .when(url.is_some(), |row| row.cursor_pointer())
                .child(Icon::new(icon).size(IconSize::Small).color(color))
                .child(
                    Label::new(r.name.clone())
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::MEDIUM),
                )
                .child(div().flex_1())
                .child(
                    Label::new(status_text)
                        .size(LabelSize::XSmall)
                        .color(color),
                )
                .on_click(move |_, _, cx| {
                    if let Some(u) = url.clone() {
                        cx.open_url(&u);
                    }
                })
        });

        v_flex()
            .w_full()
            .child(header)
            .children(rows)
            .into_any_element()
    }

    fn render_review_comments_summary(&self, cx: &Context<Self>) -> AnyElement {
        let count = self.review_comments.len();
        if count == 0 {
            return gpui::Empty.into_any_element();
        }
        h_flex()
            .px_4()
            .py_2()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Icon::new(IconName::FileDiff)
                    .size(IconSize::Small)
                    .color(Color::Info),
            )
            .child(
                Label::new(format!("{count} file review comment{}", if count == 1 { "" } else { "s" }))
                    .size(LabelSize::Small)
                    .weight(gpui::FontWeight::SEMIBOLD),
            )
            .child(
                Label::new("— open the Files tab to read and reply inline.")
                    .size(LabelSize::XSmall)
                    .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_commits(&self, cx: &Context<Self>) -> AnyElement {
        if self.commits.is_empty() {
            return gpui::Empty.into_any_element();
        }
        let header = h_flex()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(format!("Commits ({})", self.commits.len()))
                    .size(LabelSize::Default)
                    .weight(gpui::FontWeight::SEMIBOLD),
            );

        let rows = self.commits.iter().enumerate().map(|(i, c)| {
            let url = c.html_url.clone();
            h_flex()
                .id(("commit-row", i))
                .px_4()
                .py_1p5()
                .gap_2()
                .items_center()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .cursor_pointer()
                .child(if let Some(src) = c.avatar_url.clone() {
                    Avatar::new(src).size(px(16.)).into_any_element()
                } else {
                    Icon::new(IconName::Person)
                        .size(IconSize::XSmall)
                        .color(Color::Muted)
                        .into_any_element()
                })
                .child(
                    Label::new(c.short_sha.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .buffer_font(cx),
                )
                .child(
                    Label::new(c.message.clone())
                        .size(LabelSize::Small)
                        .truncate(),
                )
                .child(div().flex_1())
                .child(
                    Label::new(c.author.clone())
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .on_click(move |_, _, cx| cx.open_url(&url))
        });

        v_flex()
            .w_full()
            .child(header)
            .children(rows)
            .into_any_element()
    }

    fn render_description(&self, _cx: &Context<Self>) -> AnyElement {
        match self.description.as_ref() {
            Some(md) => v_flex()
                .px_4()
                .py_3()
                .gap_1()
                .child(
                    Label::new("Description")
                        .size(LabelSize::Small)
                        .weight(gpui::FontWeight::SEMIBOLD)
                        .color(Color::Muted),
                )
                .child(MarkdownElement::new(md.clone(), MarkdownStyle::default()))
                .into_any_element(),
            None => v_flex()
                .px_4()
                .py_3()
                .child(
                    Label::new("(no description)")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
        }
    }

    fn render_comments(&self, cx: &Context<Self>) -> AnyElement {
        // Overview only shows the PR's conversation comments (issue API).
        // Line-anchored review comments live inside the Files tab's diff
        // overlay — duplicating them here just clutters the conversation.
        let total = self.issue_comments.len();
        let header = h_flex()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().colors().border_variant)
            .child(
                Label::new(format!("Conversation ({total})"))
                    .size(LabelSize::Default)
                    .weight(gpui::FontWeight::SEMIBOLD),
            );

        let mut entries: Vec<AnyElement> = Vec::new();
        for (i, c) in self.issue_comments.iter().enumerate() {
            entries.push(self.render_comment_block(
                ("issue-comment", i),
                c.author.clone(),
                c.avatar_url.clone(),
                None,
                false,
                c.body.clone(),
                cx,
            ));
        }

        v_flex().w_full().child(header).children(entries).into_any_element()
    }

    fn render_comment_block(
        &self,
        id: (&'static str, usize),
        author: SharedString,
        avatar_url: Option<String>,
        context: Option<String>,
        outdated: bool,
        body: String,
        cx: &Context<Self>,
    ) -> AnyElement {
        // Mirror the inline diff-overlay comment row: avatar on the left,
        // surface background, author header above body, soft-wrapping text.
        let colors = cx.theme().colors();
        h_flex()
            .id(id)
            .mx_4()
            .my_1p5()
            .px_3()
            .py_2()
            .gap_2()
            .items_start()
            .rounded_md()
            .bg(colors.surface_background)
            .child(
                div()
                    .size(px(20.))
                    .flex_shrink_0()
                    .rounded_full()
                    .overflow_hidden()
                    .child(if let Some(src) = avatar_url {
                        Avatar::new(src).size(px(20.)).into_any_element()
                    } else {
                        Icon::new(IconName::Person)
                            .size(IconSize::Small)
                            .color(Color::Muted)
                            .into_any_element()
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Label::new(author)
                                    .size(LabelSize::Small)
                                    .weight(gpui::FontWeight::SEMIBOLD),
                            )
                            .when_some(context, |row, ctx| {
                                row.child(
                                    Label::new(ctx)
                                        .size(LabelSize::XSmall)
                                        .color(if outdated {
                                            Color::Muted
                                        } else {
                                            Color::Info
                                        }),
                                )
                            })
                            .when(outdated, |row| {
                                row.child(
                                    Label::new("outdated")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w_full()
                            .text_sm()
                            .text_color(colors.text)
                            .whitespace_normal()
                            .child(body),
                    ),
            )
            .into_any_element()
    }

    fn render_composer(&self, cx: &Context<Self>) -> AnyElement {
        v_flex()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .px_4()
            .py_3()
            .gap_2()
            .child(
                Label::new("Leave a comment or review")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                div()
                    .min_h(px(80.))
                    .border_1()
                    .border_color(cx.theme().colors().border_variant)
                    .rounded_md()
                    .p_2()
                    .child(self.composer.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("pr-comment", "Comment")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_comment(window, cx)
                            })),
                    )
                    .child(
                        Button::new("pr-approve", "Approve")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_review(ReviewEvent::Approve, window, cx)
                            })),
                    )
                    .child(
                        Button::new("pr-request-changes", "Request changes")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_review(ReviewEvent::RequestChanges, window, cx)
                            })),
                    )
                    .when_some(self.submit_status.clone(), |row, msg| {
                        row.child(div().flex_1())
                            .child(Label::new(msg).size(LabelSize::XSmall).color(Color::Muted))
                    }),
            )
            .into_any_element()
    }
}

async fn run_gh_pr_checkout(pr_number: u32, cwd: Option<PathBuf>) -> Result<()> {
    if which::which("gh").is_err() {
        return Err(anyhow!("gh CLI is not on PATH; install GitHub CLI to use checkout"));
    }
    let mut cmd = util::command::new_command("gh");
    cmd.args(["pr", "checkout", &pr_number.to_string()])
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::piped())
        .stderr(util::command::Stdio::piped());
    if let Some(dir) = cwd {
        cmd.current_dir(&dir);
    }
    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        return Err(anyhow!(stderr.trim().to_string()));
    }
    Ok(())
}

impl EventEmitter<()> for PrView {}

impl Focusable for PrView {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for PrView {
    type Event = ();

    fn to_item_events(_: &Self::Event, _: &mut dyn FnMut(ItemEvent)) {}

    fn tab_content_text(&self, _detail: usize, _cx: &gpui::App) -> SharedString {
        format!("PR #{}: {}", self.pr.number, self.pr.title).into()
    }

    fn tab_icon(&self, _window: &Window, _cx: &gpui::App) -> Option<Icon> {
        Some(Icon::new(IconName::PullRequest).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &gpui::App) -> Option<SharedString> {
        Some(format!("{} · #{}", self.pr.title, self.pr.number).into())
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("PR View Opened")
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

impl Render for PrView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.state {
            ContentState::Loading => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(Label::new("Loading PR…").color(Color::Muted))
                .into_any_element(),
            ContentState::Error(msg) => v_flex()
                .size_full()
                .p_4()
                .gap_2()
                .child(Label::new("Failed to load PR.").color(Color::Error))
                .child(
                    Label::new(msg.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            ContentState::Loaded => v_flex()
                .id("pr-view-scroll")
                .size_full()
                .min_h_0()
                .overflow_y_scroll()
                .child(self.render_description(cx))
                .child(self.render_check_runs(cx))
                .child(self.render_review_comments_summary(cx))
                .child(self.render_commits(cx))
                .child(self.render_comments(cx))
                .into_any_element(),
        };

        v_flex()
            .key_context("PrView")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(self.render_header(cx))
            .child(div().flex_1().min_h_0().child(body))
            .child(self.render_composer(cx))
    }
}
