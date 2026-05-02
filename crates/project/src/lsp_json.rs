//! Claude Code-compatible `.lsp.json` parser.
//!
//! When `<root>/.lsp.json` is present, its content overrides the
//! Zed-built-in language server configuration for that root. Each
//! key is a language-server name, mapped to a config object whose
//! shape matches the spec at
//! <https://code.claude.com/docs/en/plugins-reference#lsp-servers>.
//!
//! Only the fields Zed can directly honor are wired through to
//! `LspSettings`: `command` / `args` / `env` (→ `binary`),
//! `initializationOptions`, and `settings`. Fields that don't have
//! a Zed counterpart yet (`transport`, `startupTimeout`,
//! `shutdownTimeout`, `restartOnCrash`, `maxRestarts`,
//! `workspaceFolder`, `extensionToLanguage`) are parsed but
//! ignored at runtime.

use serde::Deserialize;
use settings::LspSettings;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

pub const LSP_JSON_FILENAME: &str = ".lsp.json";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspJsonServer {
    /// Optional. When omitted, the existing Zed adapter's binary
    /// resolution runs unmodified (we still pick up `args` / `env`
    /// / `initializationOptions` / `settings` overrides if
    /// present). Required when you want to override which binary
    /// runs.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default, alias = "initialization_options")]
    pub initialization_options: Option<serde_json::Value>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    // Parsed but unused — accept the field so JSON containing it
    // doesn't break deserialization.
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default, alias = "extension_to_language")]
    pub extension_to_language: Option<HashMap<String, String>>,
    #[serde(default, alias = "workspace_folder")]
    pub workspace_folder: Option<String>,
    #[serde(default, alias = "startup_timeout")]
    pub startup_timeout: Option<u64>,
    #[serde(default, alias = "shutdown_timeout")]
    pub shutdown_timeout: Option<u64>,
    #[serde(default, alias = "restart_on_crash")]
    pub restart_on_crash: Option<bool>,
    #[serde(default, alias = "max_restarts")]
    pub max_restarts: Option<u32>,
}

pub type LspJsonFile = HashMap<String, LspJsonServer>;

/// Reads `<root>/.lsp.json` from disk synchronously (small file,
/// only invoked on LSP-spawn cold path) and parses it. Returns
/// `None` on missing file or malformed JSON, with a `log::error!`
/// for the parse-failure case so users know their override didn't
/// apply.
pub fn read_lsp_json(root_abs: &Path) -> Option<LspJsonFile> {
    let path = root_abs.join(LSP_JSON_FILENAME);
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice::<LspJsonFile>(&bytes) {
        Ok(file) => Some(file),
        Err(err) => {
            log::error!("claude-review-v2: failed to parse {}: {err}", path.display());
            None
        }
    }
}

/// Folds a parsed `.lsp.json` server entry into the `LspSettings`
/// that will drive `LocalLspStore::start_language_server`. Fields
/// in `LspJsonServer` win over what was already there — the user
/// dropped the file specifically to override defaults.
pub fn apply_to_lsp_settings(server: &LspJsonServer, settings: &mut LspSettings) {
    if server.command.is_some() || server.args.is_some() || server.env.is_some() {
        let mut binary = settings.binary.clone().unwrap_or_default();
        if let Some(cmd) = &server.command {
            binary.path = Some(cmd.clone());
            // Explicit override → don't fall back to the
            // adapter's bundled installer.
            binary.ignore_system_version = Some(true);
        }
        if let Some(args) = &server.args {
            binary.arguments = Some(args.clone());
        }
        if let Some(env) = &server.env {
            binary.env = Some(env.clone());
        }
        settings.binary = Some(binary);
    }
    if let Some(init) = &server.initialization_options {
        settings.initialization_options = Some(init.clone());
    }
    if let Some(s) = &server.settings {
        settings.settings = Some(s.clone());
    }
}
