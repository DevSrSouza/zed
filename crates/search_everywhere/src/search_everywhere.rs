//! Search Everywhere — IntelliJ-style unified picker.
//!
//! Combines worktree files and registered actions in a single
//! fuzzy-matched modal. Bound to `shift shift` in the JetBrains
//! keymap (replacing the plain `command_palette::Toggle`).
//!
//! Files are matched via `fuzzy_nucleo::match_path_sets`
//! (path-aware scoring, files-only — same primitive the file
//! finder uses), so leaf-name matches outrank deep-path matches.
//! Actions are matched in parallel with `match_strings_async`.
//!
//! Sort order:
//!   1. Action whose humanized name has an exact case-insensitive
//!      prefix match against the query — highest signal, mirrors
//!      IntelliJ.
//!   2. Files (already path-scored) ahead of remaining actions.
//!   3. Within ties, higher fuzzy score wins.

use fuzzy_nucleo::{PathMatch, StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Global,
    ParentElement, Render, Styled, Subscription, Task, WeakEntity, Window, actions, rems,
};
use picker::{Picker, PickerDelegate};
use project::{Candidates, PathMatchCandidateSet, ProjectPath};
use std::cmp::Ordering;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use ui::{
    Checkbox, CommonAnimationExt as _, HighlightedLabel, Label, ListItem, ListItemSpacing,
    ToggleState, prelude::*, tooltip_container, v_flex,
};
use workspace::{ModalView, Workspace};

actions!(
    zed,
    [
        /// Open the unified Search Everywhere modal (files + actions).
        SearchEverywhere
    ]
);

/// Persists the last query the user typed into the modal so the
/// next `shift+shift` invocation can restore it (matches IntelliJ's
/// Search Everywhere behavior).
#[derive(Default, Clone)]
struct LastQuery(String);

impl Global for LastQuery {}

pub fn init(cx: &mut App) {
    cx.set_global(LastQuery::default());
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(toggle);
    })
    .detach();
}

fn toggle(
    workspace: &mut Workspace,
    _: &SearchEverywhere,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_weak = cx.entity().downgrade();
    let project = workspace.project().clone();
    // Prefill rules — first non-empty wins:
    //   1. Active editor's selection / word-under-cursor (matches
    //      cmd+shift+F's existing behavior via query_suggestion).
    //   2. Last query the user typed in this session.
    let initial_query = workspace
        .active_item(cx)
        .and_then(|item| item.to_searchable_item_handle(cx))
        .map(|handle| handle.query_suggestion(false, window, cx))
        .filter(|q| !q.is_empty())
        .unwrap_or_else(|| cx.global::<LastQuery>().0.clone());
    workspace.toggle_modal(window, cx, |window, cx| {
        SearchEverywhereModal::new(workspace_weak, project, initial_query, window, cx)
    });
}

pub struct SearchEverywhereModal {
    picker: Entity<Picker<SearchEverywhereDelegate>>,
    _worktree_subscription: Subscription,
    _picker_observation: Subscription,
    is_indexing: bool,
    refresh_task: Option<Task<()>>,
    idle_task: Option<Task<()>>,
    cached_width: Option<gpui::Pixels>,
    width_debounce_task: Option<Task<()>>,
}

impl SearchEverywhereModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        project: Entity<project::Project>,
        initial_query: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let previous_focus = window.focused(cx).unwrap_or_else(|| cx.focus_handle());
        let actions = collect_actions(window, cx);
        let modal_handle = cx.entity().downgrade();
        let delegate = SearchEverywhereDelegate {
            workspace,
            project: project.clone(),
            actions,
            matches: Vec::new(),
            selected_ix: 0,
            previous_focus,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            include_ignored: false,
            last_query: String::new(),
            modal: modal_handle,
        };
        let picker = cx.new(|cx| {
            let picker = Picker::uniform_list(delegate, window, cx);
            picker.set_query(initial_query.as_str(), window, cx);
            picker
        });

        // Stream new matches as the worktree scanner discovers more
        // paths in the background. Refresh is debounced (300ms) so a
        // burst of `WorktreeUpdatedEntries` events doesn't reshuffle
        // the list and steal the user's selection on every tick.
        // A separate idle timer flips `is_indexing` back to false
        // 1500ms after the last update — that drives the bottom-bar
        // progress indicator.
        let worktree_store = project.read(cx).worktree_store().clone();
        let _worktree_subscription = cx.subscribe_in(
            &worktree_store,
            window,
            move |modal, _store, event, window, cx| {
                use project::worktree_store::WorktreeStoreEvent;
                if matches!(event, WorktreeStoreEvent::WorktreeUpdatedEntries(_, _)) {
                    modal.note_indexing_event(window, cx);
                }
            },
        );

        // Recompute the cached modal width 500 ms after the last
        // change to the picker's matches list. While the user is
        // actively typing, every keystroke updates `matches` and
        // would otherwise jiggle the modal width on every render.
        let _picker_observation = cx.observe(&picker, |modal, _picker, cx| {
            modal.schedule_width_recompute(cx);
        });

        Self {
            picker,
            _worktree_subscription,
            _picker_observation,
            is_indexing: false,
            refresh_task: None,
            idle_task: None,
            cached_width: None,
            width_debounce_task: None,
        }
    }

    fn schedule_width_recompute(&mut self, cx: &mut Context<Self>) {
        self.width_debounce_task = Some(cx.spawn(async move |modal, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let _ = modal.update(cx, |modal, cx| {
                modal.cached_width = Some(modal.compute_target_width(cx));
                cx.notify();
            });
        }));
    }

    fn compute_target_width(&self, cx: &App) -> gpui::Pixels {
        // Width is the longest of the top-5 matches × estimated
        // char width. The viewport ceiling is applied at render
        // time (see `Render::render`).
        let delegate = &self.picker.read(cx).delegate;
        let longest_chars = delegate
            .matches
            .iter()
            .take(5)
            .map(|hit| match hit {
                Hit::Action { entry, .. } => entry.display.chars().count(),
                Hit::File { path_match } => compose_path_label(path_match).chars().count(),
            })
            .max()
            .unwrap_or(0);
        let estimate = (longest_chars as f32) * 7.5 + 96.0;
        gpui::Pixels::from(estimate)
    }

    fn note_indexing_event(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.is_indexing = true;
        cx.notify();

        // Debounced refresh — replace any pending one.
        let picker = self.picker.downgrade();
        self.refresh_task = Some(cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(300))
                .await;
            let _ = picker.update_in(cx, |picker, window, cx| {
                picker.refresh(window, cx);
            });
        }));

        // Idle timer — drop the indexing flag if no events arrive
        // within the next 1500ms.
        self.idle_task = Some(cx.spawn(async move |modal, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(1500))
                .await;
            let _ = modal.update(cx, |modal, cx| {
                modal.is_indexing = false;
                cx.notify();
            });
        }));
    }
}

impl EventEmitter<DismissEvent> for SearchEverywhereModal {}

impl Focusable for SearchEverywhereModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ModalView for SearchEverywhereModal {}

impl Render for SearchEverywhereModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let include_ignored = self
            .picker
            .read(cx)
            .delegate
            .include_ignored;
        let toggle_state = if include_ignored {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let picker = self.picker.clone();
        // Auto-grow with content: floor at the comfortable
        // 36rem default, ceiling at 75% of the host window's
        // current width. The picker's uniform-list uses the
        // container's measured width, so simply giving the
        // modal a min/max range produces a content-sized width
        // that respects the user's display.
        let viewport_width = window.viewport_size().width;
        let max_modal_width = viewport_width * 0.75;
        let indexing = self.is_indexing;
        // Width is debounced (see `schedule_width_recompute`): only
        // updated 500 ms after the last picker-state change. Avoids
        // the modal jiggling on every keystroke while the user is
        // typing. First render uses an initial measurement.
        let target_width = self
            .cached_width
            .unwrap_or_else(|| self.compute_target_width(cx))
            .min(max_modal_width);
        v_flex()
            .key_context("SearchEverywhere")
            .min_w(rems(36.))
            .w(target_width)
            .max_w(max_modal_width)
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .shadow_md()
            .child(self.picker.clone())
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .gap_2()
                    .justify_between()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().elevated_surface_background)
                    .child(
                        Checkbox::new("include-ignored", toggle_state)
                            .label("Include .gitignored files")
                            .on_click(cx.listener(move |_modal, _, window, cx| {
                                picker.update(cx, |picker, cx| {
                                    picker.delegate.include_ignored =
                                        !picker.delegate.include_ignored;
                                    picker.refresh(window, cx);
                                });
                            })),
                    )
                    .when(indexing, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Icon::new(IconName::ArrowCircle)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted)
                                        .with_rotate_animation(2)
                                        .into_any_element(),
                                )
                                .child(
                                    Label::new("Indexing…")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                ),
                        )
                    }),
            )
    }
}

struct ActionEntry {
    name: String,
    display: String,
    action: Box<dyn Action>,
}

impl Clone for ActionEntry {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            display: self.display.clone(),
            action: self.action.boxed_clone(),
        }
    }
}

#[derive(Clone)]
enum Hit {
    Action {
        entry: ActionEntry,
        positions: Vec<usize>,
    },
    File {
        path_match: PathMatch,
    },
}

pub struct SearchEverywhereDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<project::Project>,
    actions: Vec<ActionEntry>,
    matches: Vec<Hit>,
    selected_ix: usize,
    previous_focus: FocusHandle,
    cancel_flag: Arc<AtomicBool>,
    include_ignored: bool,
    last_query: String,
    modal: WeakEntity<SearchEverywhereModal>,
}

impl PickerDelegate for SearchEverywhereDelegate {
    type ListItem = ListItem;

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_ix = ix;
        cx.notify();
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        Arc::from("Files, actions…")
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        self.last_query = query.clone();
        if query.is_empty() {
            self.matches.clear();
            self.selected_ix = 0;
            cx.notify();
            return Task::ready(());
        }

        // Cancel any in-flight path search and start a fresh flag for
        // this round; `match_path_sets` polls this to bail out
        // gracefully when the query changes.
        self.cancel_flag
            .store(true, std::sync::atomic::Ordering::Release);
        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel_flag = self.cancel_flag.clone();

        let actions = self.actions.clone();
        let executor = cx.background_executor().clone();
        let project = self.project.clone();
        let include_ignored = self.include_ignored;
        let worktree_snapshots: Vec<_> = project
            .read(cx)
            .worktree_store()
            .read(cx)
            .visible_worktrees_and_single_files(cx)
            .map(|worktree| worktree.read(cx).snapshot())
            .collect();
        let file_sets: Vec<PathMatchCandidateSet> = worktree_snapshots
            .iter()
            .cloned()
            .map(|snapshot| PathMatchCandidateSet {
                snapshot,
                include_ignored,
                include_root_name: true,
                candidates: Candidates::Files,
            })
            .collect();
        let dir_sets: Vec<PathMatchCandidateSet> = worktree_snapshots
            .into_iter()
            .map(|snapshot| PathMatchCandidateSet {
                snapshot,
                include_ignored,
                include_root_name: true,
                candidates: Candidates::Directories,
            })
            .collect();

        cx.spawn(async move |picker, cx| {
            let action_candidates: Vec<StringMatchCandidate> = actions
                .iter()
                .enumerate()
                .map(|(i, a)| StringMatchCandidate::new(i, &a.display))
                .collect();
            let action_query = query.clone();

            let actions_task = {
                let executor = executor.clone();
                async move {
                    fuzzy_nucleo::match_strings_async(
                        &action_candidates,
                        &action_query,
                        fuzzy_nucleo::Case::Smart,
                        fuzzy_nucleo::LengthPenalty::On,
                        100,
                        &Default::default(),
                        executor,
                    )
                    .await
                }
            };
            let file_query = query.clone();
            let dir_query = query.clone();
            let exec_for_files = executor.clone();
            let exec_for_dirs = executor.clone();
            let cancel_for_files = cancel_flag.clone();
            let cancel_for_dirs = cancel_flag.clone();
            let files_task = async move {
                fuzzy_nucleo::match_path_sets(
                    file_sets.as_slice(),
                    &file_query,
                    &None,
                    fuzzy_nucleo::Case::Ignore,
                    5000,
                    &cancel_for_files,
                    exec_for_files,
                )
                .await
            };
            let dirs_task = async move {
                fuzzy_nucleo::match_path_sets(
                    dir_sets.as_slice(),
                    &dir_query,
                    &None,
                    fuzzy_nucleo::Case::Ignore,
                    100,
                    &cancel_for_dirs,
                    exec_for_dirs,
                )
                .await
            };

            let (action_matches, file_matches, dir_matches) =
                futures::join!(actions_task, files_task, dirs_task);

            let lowered_query = query.to_lowercase();
            let mut hits: Vec<(u8, f64, Hit)> = Vec::new();
            for m in action_matches {
                let StringMatch {
                    candidate_id,
                    score,
                    positions,
                    ..
                } = m;
                let entry = actions[candidate_id].clone();
                let exact_prefix = entry.display.to_lowercase().starts_with(&lowered_query);
                // priority bucket — lower wins:
                //   0: action exact-prefix
                //   1: file
                //   2: other action
                let priority: u8 = if exact_prefix { 0 } else { 2 };
                hits.push((priority, score, Hit::Action { entry, positions }));
            }
            let lowered = query.to_lowercase();
            for path_match in file_matches {
                // Boost: when the filename (last path segment)
                // contains the query as a contiguous substring,
                // promote that hit hard. nucleo's path-aware score
                // does prefer leaf matches but isn't strong enough
                // to outrank a 5-component path with the query
                // letters scattered across components — e.g. for
                // the query `FooPresenter`, a deep path like
                // `repo/.config/skills/presenter-patterns/SKILL.md`
                // was outranking the actual `…/FooPresenter.kt`.
                let filename = path_match
                    .path
                    .as_unix_str()
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .to_lowercase();
                let mut score = path_match.score;
                if filename.starts_with(&lowered) {
                    score += 2000.0;
                } else if filename.contains(&lowered) {
                    score += 1000.0;
                }
                hits.push((1, score, Hit::File { path_match }));
            }
            for path_match in dir_matches {
                let score = path_match.score;
                // Directories rank below remaining actions/files
                // (priority 3) so files always lead but folders
                // still surface when the query targets one.
                hits.push((3, score, Hit::File { path_match }));
            }
            hits.sort_by(|a, b| {
                a.0.cmp(&b.0)
                    .then(b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal))
            });
            let merged: Vec<Hit> = hits.into_iter().take(100).map(|(_, _, h)| h).collect();

            picker
                .update(cx, |picker, cx| {
                    picker.delegate.matches = merged;
                    if picker.delegate.selected_ix >= picker.delegate.matches.len() {
                        picker.delegate.selected_ix = 0;
                    }
                    cx.notify();
                })
                .ok();
        })
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.set_global(LastQuery(self.last_query.clone()));
        let Some(hit) = self.matches.get(self.selected_ix).cloned() else {
            return;
        };
        match hit {
            Hit::Action { entry, .. } => {
                let action = entry.action.boxed_clone();
                self.previous_focus.focus(window, cx);
                cx.defer_in(window, move |_, window, cx| {
                    window.dispatch_action(action, cx);
                });
                self.modal
                    .update(cx, |_, cx| cx.emit(DismissEvent))
                    .ok();
            }
            Hit::File { path_match } => {
                let project_path = ProjectPath {
                    worktree_id: project::WorktreeId::from_usize(path_match.worktree_id),
                    path: path_match.path.clone(),
                };
                let workspace = self.workspace.clone();
                cx.spawn_in(window, async move |_, cx| {
                    let _ = workspace
                        .update_in(cx, |workspace, window, cx| {
                            workspace
                                .open_path(project_path.clone(), None, true, window, cx)
                        })?
                        .await;
                    Ok::<_, anyhow::Error>(())
                })
                .detach_and_log_err(cx);
                self.modal
                    .update(cx, |_, cx| cx.emit(DismissEvent))
                    .ok();
            }
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.set_global(LastQuery(self.last_query.clone()));
        self.modal
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let hit = self.matches.get(ix)?;
        let (label_text, positions, badge) = match hit {
            Hit::Action { entry, positions } => {
                (entry.display.clone(), positions.clone(), "ACT")
            }
            Hit::File { path_match } => {
                // Reconstruct the exact byte string the matcher saw,
                // otherwise `path_match.positions` (byte offsets) can
                // land mid-codepoint and `HighlightedLabel` panics.
                let label = compose_path_label(path_match);
                (label, path_match.positions.clone(), "FILE")
            }
        };
        let tooltip_text: SharedString = label_text.clone().into();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .tooltip(move |_, cx| {
                    cx.new(|_| PathTooltip {
                        text: tooltip_text.clone(),
                    })
                    .into()
                })
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Label::new(badge)
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            HighlightedLabel::new(label_text, positions)
                                .truncate_start(),
                        ),
                ),
        )
    }
}

/// Wider tooltip view used to render full path strings without
/// wrapping. The default `Tooltip` widget caps its title at
/// `max_w_72` (~288px) which forces multi-line wrap on long paths;
/// here we let the host window decide the max width and force a
/// single line so the tooltip just gets longer instead of taller.
struct PathTooltip {
    text: SharedString,
}

impl Render for PathTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        tooltip_container(cx, |el, _| {
            el.child(
                div()
                    .max_w(rems(80.))
                    .child(Label::new(self.text.clone()).single_line()),
            )
        })
    }
}

fn collect_actions(window: &mut Window, cx: &App) -> Vec<ActionEntry> {
    let mut out: Vec<ActionEntry> = window
        .available_actions(cx)
        .into_iter()
        .map(|action| {
            let raw = action.name();
            let display = humanize_action(raw);
            ActionEntry {
                name: raw.to_string(),
                display,
                action,
            }
        })
        .collect();
    out.sort_by(|a, b| a.display.cmp(&b.display));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

fn compose_path_label(path_match: &PathMatch) -> String {
    if path_match.path_prefix.is_empty() {
        path_match.path.as_unix_str().to_string()
    } else {
        format!(
            "{}/{}",
            path_match.path_prefix.as_unix_str(),
            path_match.path.as_unix_str()
        )
    }
}

fn humanize_action(name: &str) -> String {
    let mut out = String::new();
    let mut prev_lower = false;
    let trimmed = name.replace("::", " > ");
    for ch in trimmed.chars() {
        if ch.is_uppercase() && prev_lower {
            out.push(' ');
        }
        out.push(ch);
        prev_lower = ch.is_lowercase();
    }
    out
}
