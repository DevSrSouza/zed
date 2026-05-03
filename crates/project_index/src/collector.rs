//! Background sweep loop that writes the symbol corpus into
//! `ProjectIndexDB` while LSPs are running. The collector is the
//! "feed" half of Smart Mode caching; the read half is wired into
//! Search Everywhere and the go-to-definition fallback in
//! sibling crates.
//!
//! Strategy:
//!
//! 1. Subscribe to the workspace's project. While it's alive,
//!    every `IDLE_REFRESH_INTERVAL` ask the project for
//!    `workspace/symbol("")`. Empty-query is the documented LSP
//!    way to retrieve "all" symbols; servers that don't honor
//!    that semantic just return nothing and we no-op.
//! 2. Group the response by (worktree, file, language-server) so
//!    each `replace_file_symbols` write is scoped to one source
//!    LSP and one source file. That keeps multiple LSPs writing
//!    to the same project from clobbering each other.
//! 3. The first sweep fires after a short delay
//!    (`STARTUP_GRACE_PERIOD`) so we don't pile work on top of
//!    Zed's own startup; subsequent sweeps run on the idle
//!    interval.
//!
//! The collector is fail-soft. Any DB or LSP error is logged
//! and skipped — the cache is a UX nice-to-have, not the
//! source of truth.

use crate::{CachedSymbol, CachedSymbolKind, ProjectIndexDB};
use collections::HashMap;
use gpui::{App, AppContext, AsyncApp, Entity, Task, WeakEntity};
use language::LanguageServerName;
use project::{Project, Symbol, lsp_store::SymbolLocation};
use std::{sync::Arc, time::Duration};
use util::rel_path::RelPath;
use worktree::WorktreeId;

const STARTUP_GRACE_PERIOD: Duration = Duration::from_secs(45);
const IDLE_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Spawn a per-project collection loop. The returned `Task` must
/// be detached or stored on the workspace; dropping it cancels
/// the loop.
pub fn install(project: Entity<Project>, cx: &mut App) -> Task<()> {
    let weak = project.downgrade();
    cx.spawn(async move |cx: &mut AsyncApp| {
        cx.background_executor().timer(STARTUP_GRACE_PERIOD).await;
        loop {
            sweep(&weak, cx).await;
            cx.background_executor()
                .timer(IDLE_REFRESH_INTERVAL)
                .await;
            if weak.upgrade().is_none() {
                return;
            }
        }
    })
}

async fn sweep(project: &WeakEntity<Project>, cx: &mut AsyncApp) {
    let Some(project_strong) = project.upgrade() else {
        return;
    };

    // Skip the sweep entirely if no language server is running —
    // there's nothing to feed the cache from. Avoids the
    // round-trip cost when Smart Mode is off.
    let any_running = project_strong.update(cx, |p, cx| {
        p.lsp_store()
            .read(cx)
            .language_server_statuses()
            .next()
            .is_some()
    });
    if !any_running {
        return;
    }

    // `Project::symbols("")` fans out to every running LSP that
    // implements `workspace/symbol`. The empty-query semantics
    // vary per server, but the caller already handles "got
    // nothing" gracefully (see search_everywhere fallback).
    let symbols_task = project_strong.update(cx, |p, cx| p.symbols("", cx));
    let symbols = match symbols_task.await {
        Ok(syms) => syms,
        Err(err) => {
            log::debug!("project_index collector: symbols('') failed: {err:#}");
            return;
        }
    };
    if symbols.is_empty() {
        return;
    }

    let groups = group_symbols(symbols);
    if groups.is_empty() {
        return;
    }

    // Resolve each worktree's abs path once. The DB schema is
    // keyed on `repo_path` strings so we use the worktree root.
    let worktree_paths: HashMap<WorktreeId, Arc<std::path::Path>> =
        project_strong.update(cx, |p, cx| {
            let mut map: HashMap<WorktreeId, Arc<std::path::Path>> = HashMap::default();
            for worktree in p.worktree_store().read(cx).visible_worktrees(cx) {
                let snapshot = worktree.read(cx);
                map.insert(snapshot.id(), snapshot.abs_path());
            }
            map
        });

    let db = cx.update(|cx| ProjectIndexDB::global(cx));
    for ((worktree_id, rel_path, server_name), batch) in groups {
        let Some(repo_path) = worktree_paths.get(&worktree_id) else {
            continue;
        };
        let repo_path = repo_path.to_string_lossy().into_owned();
        let rel_path_str = rel_path.as_unix_str().to_string();
        let server = server_name.0.to_string();
        let cached = batch
            .into_iter()
            .map(|s| {
                CachedSymbol {
                    repo_path: repo_path.clone(),
                    rel_path: rel_path_str.clone(),
                    name: s.name,
                    kind: CachedSymbolKind::from_lsp(s.kind),
                    container: s.container_name,
                    range_start_row: s.range.start.0.row,
                    range_start_col: s.range.start.0.column,
                    range_end_row: s.range.end.0.row,
                    range_end_col: s.range.end.0.column,
                    server_name: server.clone(),
                }
            })
            .collect::<Vec<_>>();
        if let Err(err) = db
            .replace_file_symbols(repo_path, rel_path_str, server, cached)
            .await
        {
            log::warn!("project_index collector: replace_file_symbols failed: {err:#}");
        }
    }
}

fn group_symbols(
    symbols: Vec<Symbol>,
) -> HashMap<(WorktreeId, Arc<RelPath>, LanguageServerName), Vec<Symbol>> {
    let mut out: HashMap<(WorktreeId, Arc<RelPath>, LanguageServerName), Vec<Symbol>> =
        HashMap::default();
    for symbol in symbols {
        let SymbolLocation::InProject(path) = &symbol.path else {
            // Dependency-source symbols (Xcode SourcePackages,
            // gradle caches, sourcekit-lsp generated headers)
            // don't belong in the project index.
            continue;
        };
        let key = (
            path.worktree_id,
            path.path.clone(),
            symbol.language_server_name.clone(),
        );
        out.entry(key).or_default().push(symbol);
    }
    out
}
