//! claude-review-v2 fork — persistent symbol index for Smart Mode.
//!
//! Goal: when LSPs are running, opportunistically record every
//! workspace symbol we see into a SQLite-backed index. When LSPs
//! are off, queries (Search Everywhere, go-to-definition fallback)
//! consult this index so the user still has basic navigation
//! without paying the LSP startup cost.
//!
//! The index is **NOT** the source of truth — running LSPs always
//! win when available. The cache is a UX bridge for the read-only
//! browsing case.
//!
//! Architecture:
//!
//! - One shared SQLite domain (`ProjectIndexDB`) backed by Zed's
//!   `db` crate domain mechanism (joins the existing
//!   `<data_dir>/db/<channel>/db.sqlite`).
//! - Two tables (`pi_*` prefix to avoid collisions with other
//!   domains): `pi_files` for invalidation metadata,
//!   `pi_symbols` for the symbol corpus. Both rows scoped by
//!   `repo_path` (workspace abs path) so multiple projects
//!   coexist.
//! - One FTS5 virtual table over `pi_symbols(name, container)`
//!   for sub-millisecond fuzzy lookup. Triggers keep it in sync
//!   on inserts / deletes.

pub mod collector;

use gpui::{App, AppContext as _, actions};
use std::time::Duration;

actions!(
    zed,
    [
        /// Drop every Smart Mode symbol cache row for the
        /// current workspace's visible worktrees. Useful
        /// when the cache has gone stale (rare — eviction
        /// is automatic) or when troubleshooting.
        ClearProjectIndexCache
    ]
);

/// Cache-row eviction window. Symbols whose owning file row
/// has not been refreshed in this long are dropped on startup.
/// 30 days lets a developer come back to a project after a
/// while with their cache intact, but stops the DB from growing
/// without bound when projects rotate.
const EVICTION_AGE: Duration = Duration::from_secs(60 * 60 * 24 * 30);

/// Initialize Smart Mode symbol caching. Called once from
/// `zed::main`.
///
/// Three things happen here:
/// 1. Eviction sweep (drop rows whose `last_seen` is older
///    than 30 days).
/// 2. Per-project collector spawned on every workspace open.
/// 3. Cache-backed go-to-definition fallback registered with
///    `project::cache_fallback`. `LspStore::definitions`
///    calls into this when the live LSP returns nothing,
///    converting cached symbol locations into go-to-def
///    results.
///
/// The collector loop self-terminates once the project entity
/// is dropped, so detaching the task is safe — no manual
/// unregister required.
pub fn init(cx: &mut App) {
    let db = ProjectIndexDB::global(cx);
    cx.spawn(async move |_cx| {
        let cutoff_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_secs() as i64 - EVICTION_AGE.as_secs() as i64)
            .unwrap_or(0);
        if let Err(err) = db.evict_older_than(cutoff_secs).await {
            log::warn!("project_index: eviction sweep failed: {err:#}");
        }
    })
    .detach();

    project::cache_fallback::register_definition_lookup(Box::new(
        |cx, identifier, repos| {
            let db = ProjectIndexDB::global(cx);
            let mut hits: Vec<project::cache_fallback::CachedDefinition> = Vec::new();
            for repo in repos {
                let repo_str = repo.to_string_lossy();
                match db.lookup_exact(&repo_str, identifier) {
                    Ok(rows) => {
                        for row in rows {
                            let abs = std::path::PathBuf::from(&row.repo_path)
                                .join(&row.rel_path);
                            let start = language::PointUtf16::new(
                                row.range_start_row,
                                row.range_start_col,
                            );
                            let end = language::PointUtf16::new(
                                row.range_end_row,
                                row.range_end_col,
                            );
                            hits.push((abs, start, end));
                        }
                    }
                    Err(err) => log::debug!(
                        "project_index cache_fallback: lookup_exact {repo_str:?} {identifier:?} failed: {err:#}"
                    ),
                }
            }
            hits
        },
    ));

    cx.observe_new(|workspace: &mut workspace::Workspace, _window, cx| {
        let project = workspace.project().clone();
        collector::install(project.clone(), cx).detach();
        workspace.register_action(
            move |workspace, _: &ClearProjectIndexCache, _window, cx| {
                let repos: Vec<String> = workspace
                    .project()
                    .read(cx)
                    .worktree_store()
                    .read(cx)
                    .visible_worktrees(cx)
                    .map(|w| w.read(cx).abs_path().to_string_lossy().to_string())
                    .collect();
                let db = ProjectIndexDB::global(cx);
                cx.spawn(async move |_, _cx| {
                    for repo in repos {
                        if let Err(err) = db.clear_repo(repo.clone()).await {
                            log::warn!(
                                "project_index: clear_repo {repo:?} failed: {err:#}"
                            );
                        } else {
                            log::info!("project_index: cleared cache for {repo}");
                        }
                    }
                })
                .detach();
            },
        );
    })
    .detach();
}

use anyhow::{Context as _, Result};
use db::{
    query,
    sqlez::{
        bindable::{Bind, Column},
        domain::Domain,
        statement::Statement,
        thread_safe_connection::ThreadSafeConnection,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachedSymbolKind {
    Class,
    Interface,
    Struct,
    Enum,
    Function,
    Method,
    Constant,
    Variable,
    Property,
    Module,
    Trait,
    TypeParameter,
    Other,
}

impl CachedSymbolKind {
    /// Map LSP symbol kind constants to our persisted variant.
    /// `lsp::SymbolKind` is a newtype with a private inner i32,
    /// so we match against the exposed `SymbolKind::*` consts.
    pub fn from_lsp(kind: lsp::SymbolKind) -> Self {
        match kind {
            lsp::SymbolKind::CLASS => Self::Class,
            lsp::SymbolKind::INTERFACE => Self::Interface,
            lsp::SymbolKind::STRUCT => Self::Struct,
            lsp::SymbolKind::ENUM => Self::Enum,
            lsp::SymbolKind::FUNCTION => Self::Function,
            lsp::SymbolKind::METHOD => Self::Method,
            lsp::SymbolKind::CONSTANT => Self::Constant,
            lsp::SymbolKind::VARIABLE => Self::Variable,
            lsp::SymbolKind::PROPERTY => Self::Property,
            lsp::SymbolKind::MODULE => Self::Module,
            lsp::SymbolKind::TYPE_PARAMETER => Self::TypeParameter,
            _ => Self::Other,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Function => "function",
            Self::Method => "method",
            Self::Constant => "constant",
            Self::Variable => "variable",
            Self::Property => "property",
            Self::Module => "module",
            Self::Trait => "trait",
            Self::TypeParameter => "type-parameter",
            Self::Other => "other",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "class" => Self::Class,
            "interface" => Self::Interface,
            "struct" => Self::Struct,
            "enum" => Self::Enum,
            "function" => Self::Function,
            "method" => Self::Method,
            "constant" => Self::Constant,
            "variable" => Self::Variable,
            "property" => Self::Property,
            "module" => Self::Module,
            "trait" => Self::Trait,
            "type-parameter" => Self::TypeParameter,
            _ => Self::Other,
        }
    }
}

impl Bind for CachedSymbolKind {
    fn bind(&self, statement: &Statement, start_index: i32) -> Result<i32> {
        self.as_str().bind(statement, start_index)
    }
}

impl Column for CachedSymbolKind {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (s, next): (String, i32) = Column::column(statement, start_index)?;
        Ok((Self::from_str(&s), next))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CachedSymbol {
    pub repo_path: String,
    pub rel_path: String,
    pub name: String,
    pub kind: CachedSymbolKind,
    pub container: Option<String>,
    pub range_start_row: u32,
    pub range_start_col: u32,
    pub range_end_row: u32,
    pub range_end_col: u32,
    pub server_name: String,
}

impl Column for CachedSymbol {
    fn column(statement: &mut Statement, start_index: i32) -> Result<(Self, i32)> {
        let (repo_path, i): (String, i32) = Column::column(statement, start_index)?;
        let (rel_path, i): (String, i32) = Column::column(statement, i)?;
        let (name, i): (String, i32) = Column::column(statement, i)?;
        let (kind, i): (CachedSymbolKind, i32) = Column::column(statement, i)?;
        let (container, i): (Option<String>, i32) = Column::column(statement, i)?;
        let (range_start_row, i): (i64, i32) = Column::column(statement, i)?;
        let (range_start_col, i): (i64, i32) = Column::column(statement, i)?;
        let (range_end_row, i): (i64, i32) = Column::column(statement, i)?;
        let (range_end_col, i): (i64, i32) = Column::column(statement, i)?;
        let (server_name, i): (String, i32) = Column::column(statement, i)?;
        Ok((
            CachedSymbol {
                repo_path,
                rel_path,
                name,
                kind,
                container,
                range_start_row: range_start_row as u32,
                range_start_col: range_start_col as u32,
                range_end_row: range_end_row as u32,
                range_end_col: range_end_col as u32,
                server_name,
            },
            i,
        ))
    }
}

pub struct ProjectIndexDB(ThreadSafeConnection);

impl Domain for ProjectIndexDB {
    const NAME: &str = stringify!(ProjectIndexDB);
    /// Raw-string migration. The SQL contains FTS5 syntax
    /// (`content='pi_symbols'`, `'delete'` argument) that the
    /// `sql!` macro can't tokenize because Rust parses single-
    /// quoted strings as `char` literals first.
    const MIGRATIONS: &[&str] = &[r#"
        CREATE TABLE IF NOT EXISTS pi_files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            repo_path TEXT NOT NULL,
            rel_path TEXT NOT NULL,
            last_seen INTEGER NOT NULL DEFAULT (unixepoch()),
            UNIQUE(repo_path, rel_path)
        ) STRICT;

        CREATE INDEX IF NOT EXISTS pi_files_repo_idx ON pi_files(repo_path);
        CREATE INDEX IF NOT EXISTS pi_files_last_seen_idx ON pi_files(last_seen);

        CREATE TABLE IF NOT EXISTS pi_symbols (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            file_id INTEGER NOT NULL REFERENCES pi_files(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            container TEXT,
            range_start_row INTEGER NOT NULL,
            range_start_col INTEGER NOT NULL,
            range_end_row INTEGER NOT NULL,
            range_end_col INTEGER NOT NULL,
            server_name TEXT NOT NULL
        ) STRICT;

        CREATE INDEX IF NOT EXISTS pi_symbols_file_idx ON pi_symbols(file_id);
        CREATE INDEX IF NOT EXISTS pi_symbols_name_idx ON pi_symbols(name);

        CREATE VIRTUAL TABLE IF NOT EXISTS pi_symbols_fts USING fts5(
            name, container,
            content='pi_symbols',
            content_rowid='id',
            tokenize='unicode61'
        );

        CREATE TRIGGER IF NOT EXISTS pi_symbols_ai AFTER INSERT ON pi_symbols BEGIN
            INSERT INTO pi_symbols_fts(rowid, name, container)
            VALUES (new.id, new.name, COALESCE(new.container, ''));
        END;

        CREATE TRIGGER IF NOT EXISTS pi_symbols_ad AFTER DELETE ON pi_symbols BEGIN
            INSERT INTO pi_symbols_fts(pi_symbols_fts, rowid, name, container)
            VALUES ('delete', old.id, old.name, COALESCE(old.container, ''));
        END;
    "#];
}

db::static_connection!(ProjectIndexDB, []);

impl ProjectIndexDB {
    /// Replace all symbols recorded for one (repo, file, server)
    /// triple with `symbols`. Atomic — either all the new rows
    /// land or none. `last_seen` is bumped to the current time.
    ///
    /// Hand-rolled (rather than `query!` macro) because we need
    /// per-row inserts in a single savepoint, and the macro
    /// doesn't support Vec parameter expansion.
    pub async fn replace_file_symbols(
        &self,
        repo_path: String,
        rel_path: String,
        server_name: String,
        symbols: Vec<CachedSymbol>,
    ) -> Result<()> {
        self.0
            .write(move |connection| -> Result<()> {
                connection.with_savepoint("replace_file_symbols", || {
                    let conn = connection;
                    let mut upsert_file = conn.exec_bound::<(&str, &str)>(
                        "INSERT INTO pi_files (repo_path, rel_path, last_seen) \
                         VALUES (?1, ?2, unixepoch()) \
                         ON CONFLICT(repo_path, rel_path) \
                            DO UPDATE SET last_seen = unixepoch()",
                    )?;
                    upsert_file((repo_path.as_str(), rel_path.as_str()))
                        .context("upserting pi_files row")?;

                    let mut delete_prior = conn.exec_bound::<(&str, &str, &str)>(
                        "DELETE FROM pi_symbols \
                         WHERE file_id = (SELECT id FROM pi_files \
                                          WHERE repo_path = ?1 AND rel_path = ?2) \
                         AND server_name = ?3",
                    )?;
                    delete_prior((
                        repo_path.as_str(),
                        rel_path.as_str(),
                        server_name.as_str(),
                    ))
                    .context("deleting prior pi_symbols rows for server")?;

                    let mut insert = conn.exec_bound::<(
                        &str,
                        &str,
                        &str,
                        CachedSymbolKind,
                        Option<&str>,
                        i64,
                        i64,
                        i64,
                        i64,
                        &str,
                    )>(
                        "INSERT INTO pi_symbols ( \
                            file_id, name, kind, container, \
                            range_start_row, range_start_col, \
                            range_end_row, range_end_col, server_name \
                        ) VALUES ( \
                            (SELECT id FROM pi_files WHERE repo_path = ?1 AND rel_path = ?2), \
                            ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10 \
                        )",
                    )?;
                    for symbol in &symbols {
                        insert((
                            repo_path.as_str(),
                            rel_path.as_str(),
                            symbol.name.as_str(),
                            symbol.kind,
                            symbol.container.as_deref(),
                            symbol.range_start_row as i64,
                            symbol.range_start_col as i64,
                            symbol.range_end_row as i64,
                            symbol.range_end_col as i64,
                            // Bind the call-site `server_name`,
                            // not `symbol.server_name`. The
                            // symbol struct's field is what gets
                            // RETURNED on lookup; the WRITE
                            // identity is the LSP that produced
                            // this batch (the function arg).
                            server_name.as_str(),
                        ))
                        .context("inserting pi_symbols row")?;
                    }
                    Ok(())
                })
            })
            .await
    }

    query! {
        pub fn lookup_exact(repo_path: &str, name: &str) -> Result<Vec<CachedSymbol>> {
            SELECT
                f.repo_path, f.rel_path, s.name, s.kind, s.container,
                s.range_start_row, s.range_start_col,
                s.range_end_row, s.range_end_col,
                s.server_name
            FROM pi_symbols s
            JOIN pi_files f ON f.id = s.file_id
            WHERE f.repo_path = (?1) AND s.name = (?2)
        }
    }

    query! {
        pub fn search_fts(repo_path: &str, pattern: &str, limit: i64) -> Result<Vec<CachedSymbol>> {
            SELECT
                f.repo_path, f.rel_path, s.name, s.kind, s.container,
                s.range_start_row, s.range_start_col,
                s.range_end_row, s.range_end_col,
                s.server_name
            FROM pi_symbols_fts fts
            JOIN pi_symbols s ON s.id = fts.rowid
            JOIN pi_files f ON f.id = s.file_id
            WHERE f.repo_path = (?1) AND pi_symbols_fts MATCH (?2)
            ORDER BY rank
            LIMIT (?3)
        }
    }

    query! {
        pub fn count_symbols(repo_path: &str) -> Result<Option<i64>> {
            SELECT COUNT(*)
            FROM pi_symbols s
            JOIN pi_files f ON f.id = s.file_id
            WHERE f.repo_path = (?1)
        }
    }

    query! {
        pub async fn evict_older_than(cutoff_unix_seconds: i64) -> Result<()> {
            DELETE FROM pi_files WHERE last_seen < (?1)
        }
    }

    query! {
        pub async fn clear_repo(repo_path: String) -> Result<()> {
            DELETE FROM pi_files WHERE repo_path = (?1)
        }
    }
}

/// Escape `query` so it can be passed to FTS5 `MATCH`. We wrap
/// in double quotes (FTS5's "phrase" form, which sidesteps
/// every operator the user might accidentally type) and append
/// `*` so any prefix match fires. For the typical use case
/// (search-everywhere fallback) this is the right semantic.
pub fn fts_prefix_query(query: &str) -> String {
    // FTS5 phrase syntax escapes embedded double-quotes by
    // doubling them.
    let escaped = query.replace('"', "\"\"");
    format!("\"{escaped}\"*")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, rel: &str, repo: &str) -> CachedSymbol {
        CachedSymbol {
            repo_path: repo.to_string(),
            rel_path: rel.to_string(),
            name: name.to_string(),
            kind: CachedSymbolKind::Class,
            container: None,
            range_start_row: 1,
            range_start_col: 0,
            range_end_row: 1,
            range_end_col: name.len() as u32,
            server_name: "kotlin-lsp".to_string(),
        }
    }

    #[gpui::test]
    async fn round_trips_one_file() {
        let db = ProjectIndexDB::open_test_db("project_index_round_trip").await;
        let repo = "/repo".to_string();
        let file = "src/Foo.kt".to_string();
        let server = "kotlin-lsp".to_string();
        db.replace_file_symbols(
            repo.clone(),
            file.clone(),
            server.clone(),
            vec![sym("FooClass", &file, &repo), sym("BarClass", &file, &repo)],
        )
        .await
        .unwrap();

        let count = db.count_symbols(&repo).unwrap().unwrap_or(0);
        assert_eq!(count, 2);

        let exact = db.lookup_exact(&repo, "FooClass").unwrap();
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].name, "FooClass");
        assert_eq!(exact[0].rel_path, "src/Foo.kt");
    }

    #[gpui::test]
    async fn replace_drops_only_same_server() {
        let db = ProjectIndexDB::open_test_db("project_index_replace_scoped").await;
        let repo = "/r".to_string();
        let file = "a.kt".to_string();

        db.replace_file_symbols(
            repo.clone(),
            file.clone(),
            "kotlin-lsp".to_string(),
            vec![sym("Alpha", &file, &repo)],
        )
        .await
        .unwrap();
        db.replace_file_symbols(
            repo.clone(),
            file.clone(),
            "other-server".to_string(),
            vec![sym("Beta", &file, &repo)],
        )
        .await
        .unwrap();

        assert_eq!(db.count_symbols(&repo).unwrap().unwrap_or(0), 2);

        db.replace_file_symbols(
            repo.clone(),
            file,
            "kotlin-lsp".to_string(),
            vec![sym("Gamma", "a.kt", "/r")],
        )
        .await
        .unwrap();

        assert_eq!(db.count_symbols(&repo).unwrap().unwrap_or(0), 2);
        assert!(db.lookup_exact(&repo, "Alpha").unwrap().is_empty());
        assert_eq!(db.lookup_exact(&repo, "Beta").unwrap().len(), 1);
        assert_eq!(db.lookup_exact(&repo, "Gamma").unwrap().len(), 1);
    }

    #[gpui::test]
    async fn fts_prefix_search() {
        let db = ProjectIndexDB::open_test_db("project_index_fts_prefix").await;
        let repo = "/r".to_string();
        db.replace_file_symbols(
            repo.clone(),
            "a.kt".into(),
            "kotlin-lsp".into(),
            vec![
                sym("launchSubscriberAwareMolecule", "a.kt", &repo),
                sym("launcher", "a.kt", &repo),
                sym("Unrelated", "a.kt", &repo),
            ],
        )
        .await
        .unwrap();

        let hits = db.search_fts(&repo, &fts_prefix_query("launch"), 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|h| h.name == "launchSubscriberAwareMolecule"));
        assert!(hits.iter().any(|h| h.name == "launcher"));
    }
}
