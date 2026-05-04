//! claude-review-v2 fork — definition fallback hook for the
//! Smart Mode persistent symbol cache.
//!
//! `project` and `project_index` can't have a circular cargo
//! dependency, so the cache lookup is registered through a
//! type-erased callback. `project_index::init` installs the
//! closure at startup; `LspStore::definitions` calls it when
//! the live LSP can't satisfy a go-to-def. If no callback is
//! installed (e.g. tests without project_index), the call site
//! degrades to "no fallback" — same as before this hook
//! existed.

use language::PointUtf16;
use std::path::PathBuf;
use std::sync::OnceLock;

/// One cached symbol declaration. Anonymous tuple so the
/// caller (project_index) doesn't have to expose its
/// `CachedSymbol` type to the project crate.
pub type CachedDefinition = (PathBuf, PointUtf16, PointUtf16);

pub type DefinitionLookup = Box<
    dyn for<'a> Fn(&'a gpui::App, &'a str, &'a [PathBuf]) -> Vec<CachedDefinition>
        + Send
        + Sync
        + 'static,
>;

static REGISTRY: OnceLock<DefinitionLookup> = OnceLock::new();

/// Install the cache-backed definition lookup. Called once
/// from `project_index::init`. Subsequent calls are ignored.
pub fn register_definition_lookup(f: DefinitionLookup) {
    let _ = REGISTRY.set(f);
}

/// Look up cached definitions for `identifier` within the
/// given repos. Empty `Vec` if no callback is registered or
/// the cache has no matches.
pub fn lookup(
    cx: &gpui::App,
    identifier: &str,
    repos: &[PathBuf],
) -> Vec<CachedDefinition> {
    let Some(f) = REGISTRY.get() else {
        return Vec::new();
    };
    f(cx, identifier, repos)
}
