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
use collections::{HashMap, HashSet};
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
        log::debug!("project_index collector: no LSPs running, skipping sweep");
        return;
    }

    log::info!("project_index collector: starting sweep");

    // `Project::symbols("")` fans out to every running LSP that
    // implements `workspace/symbol`. The empty-query semantics
    // vary per server, but the caller already handles "got
    // nothing" gracefully (see search_everywhere fallback).
    let symbols_task = project_strong.update(cx, |p, cx| p.symbols("", cx));
    let symbols = match symbols_task.await {
        Ok(syms) => syms,
        Err(err) => {
            log::warn!("project_index collector: symbols('') failed: {err:#}");
            return;
        }
    };
    log::info!(
        "project_index collector: workspace/symbol('') returned {} symbols",
        symbols.len()
    );
    if symbols.is_empty() {
        return;
    }

    let groups = group_symbols(symbols);
    log::info!(
        "project_index collector: grouped into {} (file, server) batches",
        groups.len()
    );
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
    // Track which language servers actually wrote rows from
    // the workspace/symbol main sweep. Empty-set members
    // need the per-file documentSymbol fallback below.
    let mut contributing_servers: HashSet<LanguageServerName> = HashSet::default();
    for ((worktree_id, rel_path, server_name), batch) in groups {
        if !batch.is_empty() {
            contributing_servers.insert(server_name.clone());
        }
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
        let n = cached.len();
        match db
            .replace_file_symbols(repo_path.clone(), rel_path_str.clone(), server.clone(), cached)
            .await
        {
            Ok(()) => log::debug!(
                "project_index collector: wrote {n} symbols for {server} {rel_path_str}"
            ),
            Err(err) => log::warn!(
                "project_index collector: replace_file_symbols failed for {rel_path_str}: {err:#}"
            ),
        }
    }

    // claude-review-v2 fork — per-file `documentSymbol`
    // fallback for servers that returned ZERO rows from the
    // main `workspace/symbol("")` sweep above. kotlin-lsp
    // returns the whole project so we skip it; sourcekit-lsp
    // ignores empty queries entirely so it lands here.
    //
    // The sweep walks every visible worktree on disk, sends
    // `textDocument/{didOpen,documentSymbol,didClose}`
    // directly via the running `LanguageServer` handle —
    // bypassing Buffer entities. That keeps the cost bounded
    // by the LSP round-trip; no syntax tree, no tree-sitter
    // grammar load for the file.
    let contributed: HashSet<LanguageServerName> = contributing_servers;
    if !contributed.is_empty() {
        let names: Vec<String> = contributed.iter().map(|n| n.0.to_string()).collect();
        log::info!(
            "project_index collector: skipping per-file disk-walk for servers that already contributed via workspace/symbol: {}",
            names.join(", ")
        );
    }
    let skip_servers: HashSet<LanguageServerName> = contributed;
    sweep_files_via_lsp(&project_strong, &worktree_paths, &db, &skip_servers, cx).await;
}

async fn sweep_files_via_lsp(
    project: &Entity<Project>,
    worktree_paths: &HashMap<WorktreeId, Arc<std::path::Path>>,
    db: &ProjectIndexDB,
    skip_servers: &HashSet<LanguageServerName>,
    cx: &mut AsyncApp,
) {
    // claude-review-v2 fork — detection-based mapping. For
    // every running server we DIDN'T see contribute, walk
    // the language registry to figure out which languages
    // claim this server as an adapter, then collect each of
    // those languages' file extensions + LSP languageId.
    // Strictly data-driven, so a future user installing a
    // new LSP-aware extension gets covered automatically
    // without code changes here.
    struct ServerSpec {
        server: Arc<lsp::LanguageServer>,
        name: LanguageServerName,
        // (extension without leading dot, languageId) pairs.
        // A server may serve multiple extensions across
        // multiple languages (e.g. tsserver: js / jsx / ts /
        // tsx with respective languageIds).
        targets: Vec<(String, String)>,
    }
    let server_specs: Vec<ServerSpec> = project.update(cx, |p, cx| {
        let lsp_store = p.lsp_store();
        let lsp_store_read = lsp_store.read(cx);
        let registry = lsp_store_read.languages.clone();
        let mut specs = Vec::new();
        let entries: Vec<(lsp::LanguageServerId, LanguageServerName)> = lsp_store_read
            .language_server_statuses()
            .map(|(id, status)| (id, status.name.clone()))
            .collect();

        // Build a map from server name → Vec<(ext, langId)>
        // by inverting the language registry's adapter list.
        let mut by_server: HashMap<LanguageServerName, Vec<(String, String)>> =
            HashMap::default();
        for language_name in registry.language_names() {
            let Some(available) = registry.available_language_for_name(language_name.as_ref())
            else {
                continue;
            };
            let extensions: Vec<String> = available.matcher().path_suffixes.clone();
            if extensions.is_empty() {
                continue;
            }
            for adapter in registry.lsp_adapters(&language_name) {
                let bucket = by_server.entry(adapter.name()).or_default();
                let language_id = adapter.language_id(&language_name);
                for ext in &extensions {
                    bucket.push((ext.clone(), language_id.clone()));
                }
            }
        }

        for (id, name) in entries {
            if skip_servers.contains(&name) {
                continue;
            }
            let Some(server) = lsp_store_read.language_server_for_id(id) else {
                continue;
            };
            let Some(targets) = by_server.get(&name).cloned() else {
                // No language in the registry maps to this
                // adapter (yet). Either the extension hasn't
                // loaded its language definition yet, or
                // it's a pure-runtime LSP without an
                // associated language config. Skip.
                continue;
            };
            specs.push(ServerSpec {
                server,
                name,
                targets,
            });
        }
        specs
    });

    if server_specs.is_empty() {
        return;
    }

    // Walk every visible worktree once and bucket paths by
    // extension. Both indices are tiny — `Vec<PathBuf>` per
    // extension — and we hand them to whichever server
    // claimed the extension.
    let mut by_extension: HashMap<String, Vec<(WorktreeId, Arc<RelPath>, std::path::PathBuf)>> =
        HashMap::default();
    let interesting_exts: HashSet<String> = server_specs
        .iter()
        .flat_map(|s| s.targets.iter().map(|(e, _)| e.clone()))
        .collect();
    let snapshots: Vec<(WorktreeId, std::path::PathBuf, worktree::Snapshot)> = project
        .update(cx, |p, cx| {
            p.worktree_store()
                .read(cx)
                .visible_worktrees(cx)
                .map(|worktree| {
                    let read = worktree.read(cx);
                    (read.id(), read.abs_path().to_path_buf(), read.snapshot())
                })
                .collect()
        });
    for (worktree_id, root_abs, snapshot) in snapshots {
        for entry in snapshot.entries(false, 0) {
            if entry.is_dir() {
                continue;
            }
            let path = entry.path.clone();
            let path_str = path.as_unix_str();
            // claude-review-v2 fork — skip third-party
            // pre-built frameworks. `.xcframework/` bundles
            // ship one `.swiftinterface` file per architecture
            // × per slice; indexing all of them is wasted
            // work (every symbol is duplicated 4-6 times) and
            // floods the cache with dependency internals
            // users almost never go-to-def into.
            if path_str.contains(".xcframework/") {
                continue;
            }
            let Some(filename) = path_str.rsplit('/').next() else {
                continue;
            };
            let Some(ext) = filename.rsplit('.').next() else {
                continue;
            };
            let Some(matched_ext) = interesting_exts.iter().find(|e| e.as_str() == ext) else {
                continue;
            };
            let mut abs = root_abs.clone();
            let rel_unix = path.as_unix_str();
            if !rel_unix.is_empty() {
                abs.push(rel_unix);
            }
            by_extension
                .entry(matched_ext.clone())
                .or_default()
                .push((worktree_id, path, abs));
        }
        let _ = worktree_id;
    }
    let total: usize = by_extension.values().map(Vec::len).sum();
    if total == 0 {
        return;
    }
    log::info!(
        "project_index collector: documentSymbol disk-walk over {total} files for {} servers",
        server_specs.len()
    );

    // claude-review-v2 fork — parallelism. Per (server, file)
    // we do disk-read + 3 LSP RPCs + 1 SQLite write. The
    // serial form was bottlenecked on LSP round-trips for
    // hundreds of swift files. Process N files concurrently
    // per server via `buffer_unordered`. Background-executor
    // spawn keeps the work off the foreground and away from
    // the user's typing path — Zed's UI stays responsive
    // even during a multi-minute initial sweep.
    const PARALLEL_FILES_PER_SERVER: usize = 8;

    for spec in &server_specs {
        // Bucket files for this server by their language_id.
        // One server may serve multiple languages (e.g.
        // tsserver — js, jsx, ts, tsx). didOpen needs the
        // matching languageId per file.
        let mut entries: Vec<(WorktreeId, Arc<RelPath>, std::path::PathBuf, String)> = Vec::new();
        for (ext, language_id) in &spec.targets {
            if let Some(files) = by_extension.get(ext) {
                for (worktree_id, rel_path, abs_path) in files {
                    entries.push((
                        *worktree_id,
                        rel_path.clone(),
                        abs_path.clone(),
                        language_id.clone(),
                    ));
                }
            }
        }
        if entries.is_empty() {
            continue;
        }
        let file_count = entries.len();
        let server = spec.server.clone();
        let server_name = spec.name.0.to_string();
        let db_handle = db.clone();
        let worktree_paths_owned: HashMap<WorktreeId, Arc<std::path::Path>> =
            worktree_paths.clone();

        let wrote = cx
            .background_executor()
            .spawn(async move {
                use futures::stream::StreamExt as _;
                futures::stream::iter(entries.into_iter())
                    .map(|(worktree_id, rel_path, abs_path, language_id)| {
                        let server = server.clone();
                        let server_name = server_name.clone();
                        let db = db_handle.clone();
                        let repo_root = worktree_paths_owned.get(&worktree_id).cloned();
                        async move {
                            process_one_file(
                                server,
                                &server_name,
                                &language_id,
                                repo_root,
                                rel_path,
                                abs_path,
                                &db,
                            )
                            .await
                        }
                    })
                    .buffer_unordered(PARALLEL_FILES_PER_SERVER)
                    .fold(0usize, |acc, n| async move { acc + n })
                    .await
            })
            .await;
        log::info!(
            "project_index collector: {} wrote {wrote} symbols across {file_count} files (parallel={PARALLEL_FILES_PER_SERVER})",
            spec.name.0
        );
    }
}

/// One file's worth of work: didOpen + documentSymbol +
/// didClose + DB write. Runs on the background executor.
/// Returns the number of cached symbols written. Errors
/// are logged and yield 0.
async fn process_one_file(
    server: Arc<lsp::LanguageServer>,
    server_name: &str,
    language_id: &str,
    repo_root: Option<Arc<std::path::Path>>,
    rel_path: Arc<RelPath>,
    abs_path: std::path::PathBuf,
    db: &ProjectIndexDB,
) -> usize {
    let Some(repo_root) = repo_root else {
        return 0;
    };
    let repo_path_str = repo_root.to_string_lossy().into_owned();
    let rel_path_str = rel_path.as_unix_str().to_string();

    let text = match smol::fs::read_to_string(&abs_path).await {
        Ok(t) => t,
        Err(err) => {
            log::debug!("project_index collector: read {abs_path:?} failed: {err:#}");
            return 0;
        }
    };
    let Ok(uri) = lsp::Uri::from_file_path(&abs_path) else {
        return 0;
    };

    let _ = server.notify::<lsp::notification::DidOpenTextDocument>(
        lsp::DidOpenTextDocumentParams {
            text_document: lsp::TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.to_string(),
                version: 0,
                text,
            },
        },
    );

    // claude-review-v2 fork — settle delay. sourcekit-lsp
    // (and other servers) need a beat after `didOpen` to
    // parse + analyze the file before `documentSymbol`
    // returns anything useful. Without this delay we get
    // `Some([])` back instantly because the server hasn't
    // finished its first-pass analysis. 800 ms is a
    // conservative number that's still fast enough — with
    // `buffer_unordered(8)` parallelism, 800 ms × ceil(N/8)
    // is the floor on total sweep time.
    smol::Timer::after(Duration::from_millis(800)).await;

    let response = server
        .request::<lsp::request::DocumentSymbolRequest>(
            lsp::DocumentSymbolParams {
                text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            },
            Duration::from_secs(10),
        )
        .await
        .into_response();

    let _ = server.notify::<lsp::notification::DidCloseTextDocument>(
        lsp::DidCloseTextDocumentParams {
            text_document: lsp::TextDocumentIdentifier { uri },
        },
    );

    let mut symbols: Vec<CachedSymbol> = match response {
        Ok(Some(lsp::DocumentSymbolResponse::Nested(n))) => {
            let mut out = Vec::new();
            flatten_lsp_document_symbols(&n, None, &mut out);
            out
        }
        Ok(Some(lsp::DocumentSymbolResponse::Flat(flat))) => flat
            .into_iter()
            .map(|s| {
                #[allow(deprecated)]
                CachedSymbol {
                    repo_path: String::new(),
                    rel_path: String::new(),
                    name: s.name,
                    kind: CachedSymbolKind::from_lsp(s.kind),
                    container: s.container_name,
                    range_start_row: s.location.range.start.line,
                    range_start_col: s.location.range.start.character,
                    range_end_row: s.location.range.end.line,
                    range_end_col: s.location.range.end.character,
                    server_name: String::new(),
                }
            })
            .collect(),
        Ok(None) => Vec::new(),
        Err(err) => {
            log::debug!(
                "project_index collector: documentSymbol {rel_path_str} failed: {err:#}"
            );
            return 0;
        }
    };
    if symbols.is_empty() {
        return 0;
    }
    for s in symbols.iter_mut() {
        s.repo_path = repo_path_str.clone();
        s.rel_path = rel_path_str.clone();
        s.server_name = server_name.to_string();
    }
    let n = symbols.len();
    if let Err(err) = db
        .replace_file_symbols(
            repo_path_str,
            rel_path_str.clone(),
            server_name.to_string(),
            symbols,
        )
        .await
    {
        log::warn!(
            "project_index collector: replace_file_symbols (LSP-direct) failed for {rel_path_str}: {err:#}"
        );
        0
    } else {
        log::info!(
            "project_index collector: {server_name} indexed {n} symbols from {rel_path_str}"
        );
        n
    }
}

/// Helper struct passed up from `flatten_lsp_document_symbols`.
fn flatten_lsp_document_symbols(
    nodes: &[lsp::DocumentSymbol],
    container: Option<&str>,
    out: &mut Vec<CachedSymbol>,
) {
    for node in nodes {
        // Most fields are filled in by the caller after
        // flatten — we leave repo/rel/server as empty here
        // so the cycle in `sweep_files_via_lsp` can stamp
        // them with the right context.
        #[allow(deprecated)]
        out.push(CachedSymbol {
            repo_path: String::new(),
            rel_path: String::new(),
            name: node.name.clone(),
            kind: CachedSymbolKind::from_lsp(node.kind),
            container: container.map(str::to_string),
            range_start_row: node.range.start.line,
            range_start_col: node.range.start.character,
            range_end_row: node.range.end.line,
            range_end_col: node.range.end.character,
            server_name: String::new(),
        });
        if let Some(children) = &node.children {
            flatten_lsp_document_symbols(children, Some(&node.name), out);
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
