//! claude-review-v2 fork: Swift→Kotlin go-to-definition bridge for KMP projects.
//!
//! sourcekit-lsp resolves cross-module Swift go-to-def to a synthesized
//! `<Module>.swiftinterface` file in
//! `/var/folders/.../sourcekit-lsp/GeneratedInterfaces/`. For frameworks
//! exported by Kotlin Multiplatform (Shared, Lemonade, …) those interfaces
//! are stub bridges — the real source is the `.kt` file the framework was
//! compiled from.
//!
//! When that happens, this module:
//!
//! 1. Detects target URIs that look like sourcekit-lsp's GeneratedInterfaces.
//! 2. Reads the `.swiftinterface` content and extracts the identifier at the
//!    target position (sourcekit-lsp returns zero-width ranges, so we have
//!    to slice the line ourselves).
//! 3. Asks the kotlin-lsp running in the same workspace for
//!    `workspace/symbol` matches.
//! 4. Returns the best Kotlin source location to swap into the definition
//!    response, preferring `commonMain` over platform-specific source sets.
//! 5. Returns `None` on any miss (no kotlin-lsp running, no exact-name
//!    match, fs/lsp error) so the caller falls back to the original
//!    swiftinterface location.
//!
//! Renames via `@ObjCName` and generic type erasure across the ObjC bridge
//! aren't handled here — those would need a build-time Kotlin↔ObjC index.
//! Out of scope for v1; the fallback path keeps current behavior.

use crate::lsp_store::{LanguageServerState, LspStore};
use anyhow::Result;
use gpui::{AsyncApp, WeakEntity};
use lsp::{OneOf, Range as LspRange, SymbolInformation, Uri, WorkspaceSymbolResponse};
use std::time::Duration;

const KOTLIN_LSP_NAME: &str = "kotlin-lsp";

/// Default timeout for the `workspace/symbol` round-trip we tack on after
/// definition resolution. Kept tight — definition latency to the user is
/// already gated by sourcekit-lsp; we shouldn't double the wait if
/// kotlin-lsp is slow or wedged.
const KOTLIN_SYMBOL_TIMEOUT: Duration = Duration::from_millis(2_000);

pub fn is_generated_swiftinterface(uri: &Uri) -> bool {
    if uri.scheme() != "file" {
        return false;
    }
    let path = uri.path();
    path.contains("/sourcekit-lsp/GeneratedInterfaces/") && path.ends_with(".swiftinterface")
}

/// When sourcekit-lsp goes-to-def on an ObjC-imported KMP-exported
/// type, the target lands in `<DerivedData>/Build/Products/<config>/
/// <Module>.framework/Headers/<Module>.h`. KMP mangles every Kotlin
/// type with the framework name as a C-style prefix
/// (`SharedBarPresenter` for Kotlin `BarPresenter` exported via the
/// `Shared` framework). We detect this layout so we can strip the
/// prefix before querying kotlin-lsp.
///
/// Returns the framework's module name (e.g. `"Shared"`) on match.
/// Excludes Apple SDK frameworks by requiring the
/// `<DerivedData>/Build/Products/` segment, so UIKit / SwiftUI
/// headers fall through to the original behavior.
pub fn kmp_framework_module_for(uri: &Uri) -> Option<String> {
    if uri.scheme() != "file" {
        return None;
    }
    let path = uri.path();
    if !(path.contains("/DerivedData/") && path.contains("/Build/Products/")) {
        return None;
    }
    if !path.ends_with(".h") {
        return None;
    }
    // Expect: …/<Module>.framework/Headers/<Module>.h
    let mut iter = path.rsplit('/');
    let _filename = iter.next()?;
    if iter.next()? != "Headers" {
        return None;
    }
    let framework = iter.next()?;
    framework.strip_suffix(".framework").map(str::to_string)
}

/// Extract the identifier at the given (line, character) UTF-16 position
/// from `content`. Returns `None` if the line/character is out of range or
/// no identifier overlaps the cursor.
///
/// We treat `[A-Za-z0-9_]` as identifier characters. Good enough for Swift
/// declarations in synthesized `.swiftinterface` files (we never see
/// operators or backtick-escaped names there).
pub fn extract_symbol_at_position(content: &str, line: u32, character: u32) -> Option<String> {
    let line_str = content.lines().nth(line as usize)?;
    let bytes = line_str.as_bytes();
    let pos = (character as usize).min(bytes.len());

    let is_id_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';

    let mut start = pos;
    while start > 0 && is_id_byte(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = pos;
    while end < bytes.len() && is_id_byte(bytes[end]) {
        end += 1;
    }
    if start == end {
        return None;
    }
    Some(line_str[start..end].to_string())
}

/// Try to redirect a single (target_uri, target_range) pair from
/// sourcekit-lsp's generated-interface area to the originating Kotlin
/// source via kotlin-lsp.
///
/// Returns the new (uri, range) on success, `None` otherwise (URI isn't a
/// generated interface, kotlin-lsp not present, no good match, etc.).
pub async fn redirect_swiftinterface_to_kotlin(
    target_uri: &Uri,
    target_range: LspRange,
    lsp_store: &WeakEntity<LspStore>,
    cx: &mut AsyncApp,
) -> Option<(Uri, LspRange)> {
    match try_redirect(target_uri, target_range, lsp_store, cx).await {
        Ok(found) => found,
        Err(err) => {
            log::debug!("kmp_swift_to_kotlin: redirect failed: {err:#}");
            None
        }
    }
}

async fn try_redirect(
    target_uri: &Uri,
    target_range: LspRange,
    lsp_store: &WeakEntity<LspStore>,
    cx: &mut AsyncApp,
) -> Result<Option<(Uri, LspRange)>> {
    let kmp_module = if is_generated_swiftinterface(target_uri) {
        None
    } else if let Some(module) = kmp_framework_module_for(target_uri) {
        Some(module)
    } else {
        return Ok(None);
    };

    let Ok(path) = target_uri.to_file_path() else {
        return Ok(None);
    };
    let content = smol::fs::read_to_string(&path).await?;
    let Some(raw_symbol) =
        extract_symbol_at_position(&content, target_range.start.line, target_range.start.character)
    else {
        return Ok(None);
    };

    // Strip the KMP framework prefix when present
    // (`SharedBarPresenter` -> `BarPresenter`). Swiftinterface
    // hits don't have the prefix, so we only attempt this for
    // ObjC-header hits.
    let symbol = match &kmp_module {
        Some(module) => raw_symbol
            .strip_prefix(module.as_str())
            .filter(|stripped| {
                // Avoid stripping when the prefix happens to equal
                // the whole identifier (`Shared` itself), or when
                // what's left starts with a lowercase letter — the
                // KMP convention is PascalCase types prefixed with
                // PascalCase module name.
                !stripped.is_empty()
                    && stripped.chars().next().map_or(false, |c| c.is_ascii_uppercase())
            })
            .unwrap_or(raw_symbol.as_str())
            .to_string(),
        None => raw_symbol.clone(),
    };

    let Some(symbols) = query_kotlin_workspace_symbol(lsp_store, &symbol, cx).await? else {
        return Ok(None);
    };
    let Some(best) = pick_best_kotlin_match(&symbols, &symbol) else {
        return Ok(None);
    };

    log::debug!(
        "kmp_swift_to_kotlin: redirecting {} ({} -> {}) -> {}",
        target_uri,
        raw_symbol,
        symbol,
        best.location.uri
    );

    Ok(Some((best.location.uri.clone(), best.location.range)))
}

async fn query_kotlin_workspace_symbol(
    lsp_store: &WeakEntity<LspStore>,
    query: &str,
    cx: &mut AsyncApp,
) -> Result<Option<Vec<SymbolInformation>>> {
    let Some(server) = lsp_store.update(cx, |this, _| find_kotlin_server(this))? else {
        return Ok(None);
    };

    let response = server
        .request::<lsp::request::WorkspaceSymbolRequest>(
            lsp::WorkspaceSymbolParams {
                query: query.to_string(),
                ..Default::default()
            },
            KOTLIN_SYMBOL_TIMEOUT,
        )
        .await
        .into_response();

    match response {
        Ok(opt) => Ok(opt.map(flatten_symbols)),
        Err(err) => {
            log::debug!("kmp_swift_to_kotlin: kotlin-lsp workspace/symbol failed: {err:#}");
            Ok(None)
        }
    }
}

fn find_kotlin_server(lsp_store: &LspStore) -> Option<std::sync::Arc<lsp::LanguageServer>> {
    let local = lsp_store.as_local()?;
    local.language_servers.values().find_map(|state| match state {
        LanguageServerState::Running {
            adapter, server, ..
        } => {
            if adapter.name.0.as_ref() == KOTLIN_LSP_NAME {
                Some(server.clone())
            } else {
                None
            }
        }
        LanguageServerState::Starting { .. } => None,
    })
}

fn flatten_symbols(response: WorkspaceSymbolResponse) -> Vec<SymbolInformation> {
    match response {
        WorkspaceSymbolResponse::Flat(flat) => flat,
        WorkspaceSymbolResponse::Nested(nested) => nested
            .into_iter()
            .filter_map(|s| {
                let location = match s.location {
                    OneOf::Left(loc) => loc,
                    OneOf::Right(_) => return None,
                };
                #[allow(deprecated)]
                Some(SymbolInformation {
                    name: s.name,
                    kind: s.kind,
                    location,
                    container_name: s.container_name,
                    tags: s.tags,
                    deprecated: None,
                })
            })
            .collect(),
    }
}

/// Among the symbols kotlin-lsp returned, pick the one that most plausibly
/// originated the swift symbol we landed on. Heuristic:
///
/// 1. Drop anything whose `name` doesn't match `query` exactly (case-
///    sensitive). If nothing exact, give up — we don't want to
///    accidentally redirect to an unrelated symbol that happens to fuzz-
///    match.
/// 2. Among exact matches, prefer source-set boundaries common to KMP:
///    `commonMain` > `iosMain` > `appleMain` > anything else. Same KMP
///    class can technically have multiple actuals; the commonMain side is
///    the canonical declaration.
/// 3. Tie-break on first stable iteration order.
fn pick_best_kotlin_match<'a>(
    symbols: &'a [SymbolInformation],
    query: &str,
) -> Option<&'a SymbolInformation> {
    let exact: Vec<&SymbolInformation> = symbols.iter().filter(|s| s.name == query).collect();
    if exact.is_empty() {
        return None;
    }
    const PRIORITY: &[&str] = &[
        "/commonMain/",
        "/iosMain/",
        "/appleMain/",
        "/nativeMain/",
        "/jvmMain/",
        "/androidMain/",
    ];
    for needle in PRIORITY {
        if let Some(found) = exact.iter().find(|s| s.location.uri.path().contains(needle)) {
            return Some(*found);
        }
    }
    exact.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::str::FromStr;

    #[test]
    fn detects_generated_swiftinterface() {
        let uri = Uri::from_str(
            "file:///var/folders/23/x/T/sourcekit-lsp/GeneratedInterfaces/abc/Shared.swiftinterface",
        )
        .unwrap();
        assert!(is_generated_swiftinterface(&uri));
    }

    #[test]
    fn ignores_user_swift_files() {
        let uri = Uri::from_str("file:///Users/dev/repo/iosApp/Foo.swift").unwrap();
        assert!(!is_generated_swiftinterface(&uri));
    }

    #[test]
    fn extracts_symbol_at_zero_width_range() {
        // sourcekit-lsp typically returns range with start == end at the
        // beginning of the identifier.
        let line = "public class BarPresenter {";
        // line index 0, identifier starts at col 13.
        let got = extract_symbol_at_position(line, 0, 13);
        assert_eq!(got.as_deref(), Some("BarPresenter"));
    }

    #[test]
    fn extracts_symbol_with_cursor_inside() {
        let line = "  func registerSomething(in registry: RouteRegistry)";
        let got = extract_symbol_at_position(line, 0, 10);
        assert_eq!(got.as_deref(), Some("registerSomething"));
    }

    #[test]
    fn returns_none_when_cursor_in_whitespace() {
        let line = "    public  class  Foo";
        let got = extract_symbol_at_position(line, 0, 11);
        assert_eq!(got, None);
    }

    #[test]
    fn picks_common_main_when_available() {
        let mk = |path: &str| {
            #[allow(deprecated)]
            SymbolInformation {
                name: "Foo".into(),
                kind: lsp::SymbolKind::CLASS,
                tags: None,
                deprecated: None,
                location: lsp::Location {
                    uri: Uri::from_str(&format!("file://{path}")).unwrap(),
                    range: lsp::Range::default(),
                },
                container_name: None,
            }
        };
        let symbols = vec![
            mk("/repo/feature/ui/src/iosMain/kotlin/com/foo/Foo.kt"),
            mk("/repo/feature/ui/src/commonMain/kotlin/com/foo/Foo.kt"),
            mk("/repo/feature/ui/src/androidMain/kotlin/com/foo/Foo.kt"),
        ];
        let best = pick_best_kotlin_match(&symbols, "Foo").unwrap();
        assert!(best.location.uri.path().contains("/commonMain/"));
    }

    #[test]
    fn detects_kmp_framework_header() {
        let uri = Uri::from_str(
            "file:///Users/x/Library/Developer/Xcode/DerivedData/iosApp-abc/Build/Products/Debug-iphonesimulator/Shared.framework/Headers/Shared.h",
        ).unwrap();
        assert_eq!(kmp_framework_module_for(&uri).as_deref(), Some("Shared"));
    }

    #[test]
    fn ignores_apple_sdk_frameworks() {
        let uri = Uri::from_str(
            "file:///Applications/Xcode.app/Contents/Developer/Platforms/iPhoneSimulator.platform/Developer/SDKs/iPhoneSimulator26.4.sdk/System/Library/Frameworks/UIKit.framework/Headers/UIKit.h",
        ).unwrap();
        assert!(kmp_framework_module_for(&uri).is_none());
    }

    #[test]
    fn rejects_when_no_exact_match() {
        let mk = |name: &str| {
            #[allow(deprecated)]
            SymbolInformation {
                name: name.into(),
                kind: lsp::SymbolKind::CLASS,
                tags: None,
                deprecated: None,
                location: lsp::Location {
                    uri: Uri::from_str("file:///x.kt").unwrap(),
                    range: lsp::Range::default(),
                },
                container_name: None,
            }
        };
        let symbols = vec![mk("FooBar"), mk("FooBaz")];
        assert!(pick_best_kotlin_match(&symbols, "Foo").is_none());
    }
}
