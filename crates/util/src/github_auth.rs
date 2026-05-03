//! Resolves a GitHub API token for unauthenticated, non-interactive contexts.
//!
//! Resolution order, evaluated once and cached:
//! 1. `GITHUB_TOKEN` environment variable (existing behavior).
//! 2. `gh auth token`, when the GitHub CLI is on `PATH` and already authenticated.
//!
//! The result is memoized for the process lifetime. If the user authenticates `gh`
//! after Zed starts, they need to restart Zed to pick it up — this matches the
//! existing `GITHUB_TOKEN` env-var contract.

use std::sync::OnceLock;

static RESOLVED_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Returns a GitHub API bearer token if one is available, preferring `GITHUB_TOKEN`
/// and falling back to `gh auth token`.
///
/// Cached after the first successful resolution. Spawning `gh` only happens when
/// the env var is missing and `gh` is on `PATH`; on subsequent calls the cached
/// value is returned without I/O.
pub async fn github_token() -> Option<&'static str> {
    if let Some(cached) = RESOLVED_TOKEN.get() {
        return cached.as_deref();
    }

    let resolved = resolve().await;
    // Multiple concurrent first-callers may race; whichever wins, the value is
    // identical, so dropping the loser's result is fine.
    let _ = RESOLVED_TOKEN.set(resolved);
    RESOLVED_TOKEN.get().and_then(|t| t.as_deref())
}

async fn resolve() -> Option<String> {
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        let token = token.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
    }

    resolve_from_gh_cli().await
}

#[cfg(not(target_family = "wasm"))]
async fn resolve_from_gh_cli() -> Option<String> {
    use crate::command::{Stdio, new_command};

    // Confirm the binary is on PATH before spawning so we don't pay the cost of
    // a missing-PATH failure on every Zed start when the CLI isn't installed.
    if which::which("gh").is_err() {
        return None;
    }

    let output = new_command("gh")
        .args(["auth", "token"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if token.is_empty() {
        log::debug!("`gh auth token` returned empty output; ignoring");
        None
    } else {
        log::info!("resolved GitHub token from `gh auth token`");
        Some(token)
    }
}

#[cfg(target_family = "wasm")]
async fn resolve_from_gh_cli() -> Option<String> {
    None
}
