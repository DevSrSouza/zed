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
use editor::Editor;
use gpui::{
    Action, Anchor, App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    MouseButton, Pixels, SharedString, Subscription, Task, WeakEntity, actions, anchored, deferred,
    px,
};
use http_client::HttpClient;
use project::{Project, git_store::GitStoreEvent};
use ui::{Avatar, Divider, TintColor, Tooltip, prelude::*};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::github_api::{GitHubClient, PrListState, PullRequest, RepoCoords};
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

    state_filter: PrListState,
    selected_author: Option<SharedString>,
    author_picker_open: bool,
    search_editor: Entity<Editor>,
    author_search_editor: Entity<Editor>,
    search_subscription: Subscription,
    author_subscription: Subscription,

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

    fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
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

        // Title-bar search box: matches title / author / branch / #N.
        let search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Filter PRs by title, author, or branch", window, cx);
            editor
        });
        let search_subscription =
            cx.subscribe(&search_editor, |_, _, _: &editor::EditorEvent, cx| cx.notify());

        // Search box inside the author-picker dropdown.
        let author_search_editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Search authors", window, cx);
            editor
        });
        let author_subscription =
            cx.subscribe(&author_search_editor, |_, _, _: &editor::EditorEvent, cx| {
                cx.notify();
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
            state_filter: PrListState::Open,
            selected_author: None,
            author_picker_open: false,
            search_editor,
            author_search_editor,
            search_subscription,
            author_subscription,
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
        let state_filter = self.state_filter;
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let Some(token) = util::github_auth::github_token().await else {
                return;
            };
            let client = GitHubClient::new(http_client, token.to_string());
            if let Ok(prs) = client.list_prs(&repo_for_task, state_filter).await {
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
        let state_filter = self.state_filter;
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
            let result = client.list_prs(&repo_for_task, state_filter).await;

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
            PanelState::Ready { prs, .. } => {
                let query = self.search_editor.read(cx).text(cx).to_lowercase();
                let q = query.trim().to_string();
                let selected_author = self.selected_author.clone();

                let filtered: Vec<&PullRequest> = prs
                    .iter()
                    .filter(|pr| {
                        if let Some(author) = &selected_author
                            && pr.user_login.as_ref() != author.as_ref()
                        {
                            return false;
                        }
                        if q.is_empty() {
                            return true;
                        }
                        pr.title.to_lowercase().contains(&q)
                            || pr.user_login.to_lowercase().contains(&q)
                            || pr.head_ref.to_lowercase().contains(&q)
                            || pr.number.to_string().contains(&q)
                    })
                    .collect();

                if filtered.is_empty() {
                    let empty_msg = if !q.is_empty() && selected_author.is_some() {
                        format!(
                            "No PRs match \"{q}\" by {}.",
                            selected_author.as_ref().unwrap()
                        )
                    } else if !q.is_empty() {
                        format!("No PRs match \"{q}\".")
                    } else if let Some(author) = &selected_author {
                        format!("No PRs by {author}.")
                    } else {
                        format!("No {} pull requests.", self.state_filter.label().to_lowercase())
                    };
                    v_flex()
                        .p_4()
                        .gap_2()
                        .child(Label::new(empty_msg).color(Color::Muted))
                        .into_any_element()
                } else {
                    v_flex()
                        .id("pr-list")
                        .size_full()
                        .overflow_y_scroll()
                        .children(filtered.iter().map(|pr| self.render_pr_row(pr, cx)))
                        .into_any_element()
                }
            }
        }
    }

    fn set_state_filter(&mut self, filter: PrListState, cx: &mut Context<Self>) {
        if self.state_filter == filter {
            return;
        }
        self.state_filter = filter;
        self.refresh(cx);
    }

    fn render_filter_bar(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let segment = |state: PrListState| {
            let active = self.state_filter == state;
            let id: gpui::ElementId =
                gpui::ElementId::Name(format!("pr-filter-{}", state.label()).into());
            Button::new(id, state.label())
                .style(if active {
                    ButtonStyle::Tinted(TintColor::Accent)
                } else {
                    ButtonStyle::Subtle
                })
                .label_size(LabelSize::Small)
                .on_click(cx.listener(move |this, _, _, cx| this.set_state_filter(state, cx)))
        };

        v_flex()
            .px_2()
            .pt_1p5()
            .pb_2()
            .gap_1p5()
            .border_b_1()
            .border_color(colors.border_variant)
            .child(
                div()
                    .border_1()
                    .border_color(colors.border_variant)
                    .rounded_md()
                    .px_2()
                    .py_1()
                    .child(self.search_editor.clone()),
            )
            .child(self.render_author_dropdown(cx))
            .child(
                h_flex()
                    .gap_1()
                    .child(segment(PrListState::Open))
                    .child(segment(PrListState::Closed))
                    .child(segment(PrListState::All)),
            )
            .into_any_element()
    }

    fn render_author_dropdown(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();

        // Find the avatar URL for the selected author from any cached PR by
        // that user — saves us a per-author API call.
        let (selected_label, selected_avatar): (SharedString, Option<String>) =
            match self.selected_author.clone() {
                Some(login) => {
                    let avatar = if let PanelState::Ready { prs, .. } = &self.state {
                        prs.iter()
                            .find(|p| p.user_login.as_ref() == login.as_ref())
                            .and_then(|p| p.user_avatar_url.clone())
                    } else {
                        None
                    };
                    (login, avatar)
                }
                None => ("Any author".into(), None),
            };

        let trigger = h_flex()
            .id("pr-author-trigger")
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .border_1()
            .border_color(colors.border_variant)
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(colors.element_hover))
            .child(if let Some(src) = selected_avatar {
                Avatar::new(src).size(px(16.)).into_any_element()
            } else {
                Icon::new(IconName::Person)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .into_any_element()
            })
            .child(
                Label::new(selected_label)
                    .size(LabelSize::Small)
                    .truncate(),
            )
            .child(div().flex_1())
            .when_some(self.selected_author.clone(), |row, _| {
                row.child(
                    IconButton::new("pr-author-clear", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Clear author filter"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.selected_author = None;
                            cx.notify();
                        })),
                )
            })
            .child(Icon::new(IconName::ChevronDown).size(IconSize::XSmall).color(Color::Muted))
            .on_click(cx.listener(|this, _, _, cx| {
                this.author_picker_open = !this.author_picker_open;
                cx.notify();
            }));

        let popover = if self.author_picker_open {
            Some(self.render_author_popover(cx))
        } else {
            None
        };

        v_flex()
            .child(trigger)
            .when_some(popover, |col, popover| {
                col.child(
                    deferred(anchored().anchor(Anchor::TopLeft).child(popover))
                        .with_priority(1),
                )
            })
            .into_any_element()
    }

    fn render_author_popover(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();

        // Distinct (login, avatar, count) entries from currently-loaded PRs.
        // Sorted by count desc — most active authors at the top — with
        // alphabetical tiebreak.
        let mut by_login: collections::HashMap<String, (SharedString, Option<String>, u32)> =
            Default::default();
        if let PanelState::Ready { prs, .. } = &self.state {
            for pr in prs {
                let login_key = pr.user_login.to_string();
                let entry = by_login.entry(login_key).or_insert_with(|| {
                    (pr.user_login.clone(), pr.user_avatar_url.clone(), 0)
                });
                entry.2 += 1;
            }
        }
        let mut authors: Vec<(SharedString, Option<String>, u32)> = by_login.into_values().collect();
        authors.sort_by(|a, b| {
            b.2.cmp(&a.2).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });

        let query = self
            .author_search_editor
            .read(cx)
            .text(cx)
            .to_lowercase();
        let filtered: Vec<(SharedString, Option<String>, u32)> = authors
            .into_iter()
            .filter(|(login, _, _)| {
                query.trim().is_empty() || login.to_lowercase().contains(query.trim())
            })
            .collect();

        v_flex()
            .id("pr-author-popover")
            .mt_1()
            .w(px(280.))
            .max_h(px(360.))
            .min_h_0()
            .bg(colors.elevated_surface_background)
            .border_1()
            .border_color(colors.border)
            .rounded_md()
            .shadow_md()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.author_picker_open = false;
                cx.notify();
            }))
            .child(
                div()
                    .px_2()
                    .py_1p5()
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(self.author_search_editor.clone()),
            )
            .child(
                v_flex()
                    .id("pr-author-list")
                    .flex_1()
                    .min_h_0()
                    .max_h(px(300.))
                    .overflow_y_scroll()
                    .child(
                        h_flex()
                            .id("pr-author-any")
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(colors.element_hover))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.selected_author = None;
                                this.author_picker_open = false;
                                cx.notify();
                            }))
                            .child(
                                Icon::new(IconName::Person)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new("Any author").size(LabelSize::Small)),
                    )
                    .children(filtered.into_iter().enumerate().map(|(i, (login, avatar, count))| {
                        let login_for_click = login.clone();
                        h_flex()
                            .id(("pr-author-item", i))
                            .px_2()
                            .py_1p5()
                            .gap_2()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.bg(colors.element_hover))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_author = Some(login_for_click.clone());
                                this.author_picker_open = false;
                                cx.notify();
                            }))
                            .child(if let Some(src) = avatar {
                                Avatar::new(src).size(px(20.)).into_any_element()
                            } else {
                                Icon::new(IconName::Person)
                                    .size(IconSize::Small)
                                    .color(Color::Muted)
                                    .into_any_element()
                            })
                            .child(
                                Label::new(login)
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .child(div().flex_1())
                            .child(
                                Label::new(count.to_string())
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted),
                            )
                    })),
            )
            .into_any_element()
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
            .child(self.render_filter_bar(cx))
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
