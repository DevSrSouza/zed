//! Pull Request panel for GitHub repos. MVP: lists open PRs from the current
//! project's GitHub remote and lets the user open changed files at the PR's
//! HEAD revision as read-only buffers in Zed.
//!
//! Auth uses `util::github_auth::github_token()` which prefers `GITHUB_TOKEN`
//! and falls back to `gh auth token`. Panel only registers when a token is
//! available AND the active repo has a `github.com` remote.

mod github_api;
mod pr_files_view;
mod pr_view;

use std::sync::Arc;

use anyhow::Result;
use gpui::{
    Action, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, Pixels,
    Subscription, Task, WeakEntity, actions, px,
};
use http_client::HttpClient;
use project::{Project, git_store::GitStoreEvent};
use ui::{Divider, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::github_api::{GitHubClient, PullRequest, RepoCoords};
use crate::pr_view::PrView;

const PR_PANEL_KEY: &str = "PrPanel";

actions!(
    pr_panel,
    [
        /// Toggles focus on the Pull Requests panel.
        ToggleFocus,
        /// Refreshes the list of open pull requests.
        Refresh,
    ]
);

pub fn init(_cx: &mut App) {
    // Workspace-level action handlers are registered in `crates/zed/src/zed.rs`
    // alongside other panel toggles. Nothing global needed here yet.
}

pub struct PrPanel {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    http_client: Arc<dyn HttpClient>,
    focus_handle: FocusHandle,
    width: Option<Pixels>,

    state: PanelState,
    refresh_task: Option<Task<()>>,
    open_view_task: Option<Task<()>>,

    _project_subscription: Subscription,
    _git_store_subscription: Subscription,
}

enum PanelState {
    Loading,
    NoToken,
    NoRepo,
    Ready { repo: RepoCoords, prs: Vec<PullRequest> },
    Error(String),
}

impl PrPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    fn new(workspace: &Workspace, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let http_client = project.read(cx).client().http_client();
        let focus_handle = cx.focus_handle();

        let project_subscription = cx.observe(&project, |this, _, cx| {
            // Repo set may have changed (e.g. user added a worktree).
            if matches!(this.state, PanelState::NoRepo | PanelState::Error(_)) {
                this.refresh(cx);
            }
        });

        // Watch the project's GitStore for repository discovery events. On a
        // cold workspace open, repositories are added asynchronously after
        // the panel mounts and their `remote_origin_url` is populated even
        // later (a separate `RepositoryUpdated` once the snapshot syncs).
        // Without watching both phases, the panel would stick in `NoRepo`.
        let git_store = project.read(cx).git_store().clone();
        let git_store_subscription =
            cx.subscribe(&git_store, |this, _store, event: &GitStoreEvent, cx| {
                let should_refresh = matches!(
                    event,
                    GitStoreEvent::RepositoryAdded
                        | GitStoreEvent::RepositoryRemoved(_)
                        | GitStoreEvent::ActiveRepositoryChanged(_)
                        | GitStoreEvent::RepositoryUpdated(_, _, _)
                );
                if should_refresh
                    && matches!(this.state, PanelState::NoRepo | PanelState::Error(_))
                {
                    this.silent_refresh(cx);
                }
            });

        let mut this = Self {
            workspace: workspace.weak_handle(),
            project,
            http_client,
            focus_handle,
            width: None,
            state: PanelState::Loading,
            refresh_task: None,
            open_view_task: None,
            _project_subscription: project_subscription,
            _git_store_subscription: git_store_subscription,
        };
        this.refresh(cx);
        this
    }

    /// Background-poll variant of refresh: doesn't flash the panel to the
    /// Loading state; just swaps the PR list when the new fetch returns. If
    /// the request fails, the previously-displayed list stays put. When
    /// detect_repo fails (no GitHub remote yet, e.g. mid-discovery), we
    /// transition to `NoRepo` so subsequent GitStore events can still gate
    /// retry attempts on `state == NoRepo`.
    fn silent_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = Self::detect_repo(&self.project, cx) else {
            if !matches!(self.state, PanelState::NoRepo) {
                self.state = PanelState::NoRepo;
                cx.notify();
            }
            return;
        };

        let http_client = self.http_client.clone();
        let repo_for_task = repo.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let Some(token) = util::github_auth::github_token().await else {
                return;
            };
            let client = GitHubClient::new(http_client, token.to_string());
            if let Ok(prs) = client.list_open_prs(&repo_for_task).await {
                this.update(cx, |this, cx| {
                    this.state = PanelState::Ready {
                        repo: repo_for_task,
                        prs,
                    };
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn detect_repo(project: &Entity<Project>, cx: &App) -> Option<RepoCoords> {
        let project = project.read(cx);
        for repo in project.repositories(cx).values() {
            let repo = repo.read(cx);
            let url = repo.remote_origin_url.as_deref()?;
            if let Some(coords) = RepoCoords::parse_github_url(url) {
                return Some(coords);
            }
        }
        None
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = Self::detect_repo(&self.project, cx) else {
            self.state = PanelState::NoRepo;
            cx.notify();
            return;
        };
        self.state = PanelState::Loading;
        cx.notify();

        let http_client = self.http_client.clone();
        let repo_for_task = repo.clone();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let token = util::github_auth::github_token().await;
            let Some(token) = token else {
                this.update(cx, |this, cx| {
                    this.state = PanelState::NoToken;
                    cx.notify();
                })
                .ok();
                return;
            };

            let client = GitHubClient::new(http_client, token.to_string());
            let result = client.list_open_prs(&repo_for_task).await;

            this.update(cx, |this, cx| {
                match result {
                    Ok(prs) => {
                        this.state = PanelState::Ready {
                            repo: repo_for_task,
                            prs,
                        };
                    }
                    Err(err) => {
                        log::error!("pr_panel: list PRs failed: {err:?}");
                        this.state = PanelState::Error(format!("{err:#}"));
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn open_pr(&mut self, pr: PullRequest, window: &mut Window, cx: &mut Context<Self>) {
        let PanelState::Ready { repo, .. } = &self.state else {
            return;
        };
        let repo = repo.clone();
        let project = self.project.clone();
        let workspace = self.workspace.clone();
        let http = self.http_client.clone();
        self.open_view_task = Some(cx.spawn_in(window, async move |_this, cx| {
            workspace
                .update_in(cx, move |workspace, window, cx| {
                    let view = cx.new(|cx| {
                        PrView::new(pr, repo, project, workspace.weak_handle(), http, window, cx)
                    });
                    workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
                })
                .ok();
        }));
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let repo_label = match &self.state {
            PanelState::Ready { repo, .. } => Some(format!("{}/{}", repo.owner, repo.repo)),
            _ => None,
        };
        h_flex()
            .px_2()
            .py_1()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Icon::new(IconName::PullRequest)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("Pull Requests")
                            .size(LabelSize::Small)
                            .weight(gpui::FontWeight::SEMIBOLD),
                    )
                    .when_some(repo_label, |this, label| {
                        this.child(Divider::vertical()).child(
                            Label::new(label)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                    }),
            )
            .child(
                IconButton::new("pr-refresh", IconName::ArrowCircle)
                    .icon_size(IconSize::Small)
                    .tooltip(Tooltip::text("Refresh"))
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            )
    }

    fn render_body(&self, cx: &Context<Self>) -> AnyElement {
        match &self.state {
            PanelState::Loading => v_flex()
                .p_4()
                .gap_2()
                .items_center()
                .child(Label::new("Loading…").color(Color::Muted))
                .into_any_element(),
            PanelState::NoToken => v_flex()
                .p_4()
                .gap_2()
                .child(
                    Label::new("GitHub token unavailable.")
                        .color(Color::Muted),
                )
                .child(
                    Label::new("Run `gh auth login` or set GITHUB_TOKEN, then click Refresh.")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            PanelState::NoRepo => v_flex()
                .p_4()
                .gap_2()
                .child(
                    Label::new("No GitHub remote detected in this project.")
                        .color(Color::Muted),
                )
                .into_any_element(),
            PanelState::Error(msg) => v_flex()
                .p_4()
                .gap_2()
                .child(Label::new("Failed to load PRs.").color(Color::Error))
                .child(
                    Label::new(msg.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .into_any_element(),
            PanelState::Ready { prs, .. } if prs.is_empty() => v_flex()
                .p_4()
                .gap_2()
                .child(Label::new("No open pull requests.").color(Color::Muted))
                .into_any_element(),
            PanelState::Ready { prs, .. } => v_flex()
                .id("pr-list")
                .size_full()
                .overflow_y_scroll()
                .children(prs.iter().map(|pr| self.render_pr_row(pr, cx)))
                .into_any_element(),
        }
    }

    fn render_pr_row(&self, pr: &PullRequest, cx: &Context<Self>) -> AnyElement {
        let pr_number = pr.number;
        let url = pr.html_url.clone();
        let pr_for_open = pr.clone();

        h_flex()
            .id(("pr-row", pr_number as u64))
            .px_2()
            .py_1()
            .gap_1p5()
            .w_full()
            .hover(|s| s.bg(cx.theme().colors().element_hover))
            .child(
                Icon::new(IconName::PullRequest)
                    .size(IconSize::XSmall)
                    .color(if pr.draft {
                        Color::Muted
                    } else {
                        Color::Success
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        Label::new(pr.title.clone())
                            .size(LabelSize::Small)
                            .truncate(),
                    )
                    .child(
                        Label::new(format!(
                            "#{} · {} · {}",
                            pr.number, pr.user_login, pr.head_ref
                        ))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .truncate(),
                    ),
            )
            .child(
                IconButton::new(("pr-open-browser", pr_number as u64), IconName::Link)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text("Open in browser"))
                    .on_click(move |_, _, cx| cx.open_url(&url)),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_pr(pr_for_open.clone(), window, cx);
            }))
            .into_any_element()
    }
}

impl Render for PrPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PrPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(self.render_header(cx))
            .child(self.render_body(cx))
    }
}

impl Focusable for PrPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for PrPanel {}

impl Panel for PrPanel {
    fn persistent_name() -> &'static str {
        "PrPanel"
    }

    fn panel_key() -> &'static str {
        PR_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Left
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, _position: DockPosition, _window: &mut Window, _cx: &mut Context<Self>) {}

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.width.unwrap_or(px(280.))
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<IconName> {
        Some(IconName::PullRequest)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Pull Requests")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        // Sits between git_panel (3) and collab_panel (5).
        4
    }

    /// Fires whenever the dock makes this panel the visible one (initial
    /// reveal, switching back from another panel, restoring a saved layout
    /// where this panel was open). Use it to silently refresh the PR list
    /// instead of polling on a timer.
    fn set_active(&mut self, active: bool, _window: &mut Window, cx: &mut Context<Self>) {
        if active {
            self.silent_refresh(cx);
        }
    }
}
