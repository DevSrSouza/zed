//! Rule-based exceptions to the project panel's auto-fold-dirs
//! logic. Folders matched by any of these rules will NOT be
//! collapsed into a single-line ancestor chain — they always
//! render on their own row. Lets the panel keep IntelliJ-like
//! "module / source-set / language root" boundaries visible
//! even when intermediate folders have a single child.

use settings::{RegisterSetting, Settings};
use settings_content::{FoldExceptionRuleContent, SettingsContent};
use std::sync::Arc;
use util::paths::{PathMatcher, PathStyle};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct FoldExceptionsSettings {
    pub rules: Arc<Vec<FoldExceptionRule>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FoldExceptionRule {
    pub name: String,
    pub name_pattern: Option<PathMatcher>,
    pub path_glob: Option<PathMatcher>,
    pub parent_has_files: Vec<String>,
    pub is_ignored: Option<bool>,
}

const DEFAULTS_JSON: &str = include_str!("../default_fold_exceptions.json");

fn build_matcher(pattern: &str) -> Option<PathMatcher> {
    PathMatcher::new([pattern.to_string()], PathStyle::Posix).ok()
}

fn rule_from_content(label: &str, raw: &FoldExceptionRuleContent) -> Option<FoldExceptionRule> {
    let name_pattern = raw.name_pattern.as_deref().and_then(build_matcher);
    let path_glob = raw.path_glob.as_deref().and_then(build_matcher);
    let parent_has_files = raw.parent_has_files.clone().unwrap_or_default();
    if name_pattern.is_none()
        && path_glob.is_none()
        && parent_has_files.is_empty()
        && raw.is_ignored.is_none()
    {
        return None;
    }
    Some(FoldExceptionRule {
        name: raw.name.clone().unwrap_or_else(|| label.to_string()),
        name_pattern,
        path_glob,
        parent_has_files,
        is_ignored: raw.is_ignored,
    })
}

fn default_rules() -> Vec<FoldExceptionRule> {
    let raw: Vec<FoldExceptionRuleContent> = serde_json::from_str(DEFAULTS_JSON)
        .unwrap_or_else(|err| {
            log::error!("malformed default_fold_exceptions.json: {err}");
            Vec::new()
        });
    raw.iter()
        .enumerate()
        .filter_map(|(i, raw)| rule_from_content(&format!("default-{i}"), raw))
        .collect()
}

impl Settings for FoldExceptionsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let rules = match content
            .project_panel
            .as_ref()
            .and_then(|p| p.fold_exceptions.as_ref())
        {
            None => default_rules(),
            Some(list) if list.is_empty() => Vec::new(),
            Some(list) => list
                .iter()
                .enumerate()
                .filter_map(|(i, raw)| rule_from_content(&format!("rule-{i}"), raw))
                .collect(),
        };
        Self {
            rules: Arc::new(rules),
        }
    }
}

/// Returns true when any rule says this folder must not be auto-folded.
pub fn matches(
    rules: &[FoldExceptionRule],
    folder_name: &str,
    relative_path: &str,
    parent_filenames: &[&str],
    is_ignored: bool,
) -> bool {
    for rule in rules {
        if let Some(required) = rule.is_ignored {
            if required != is_ignored {
                continue;
            }
        }
        if let Some(matcher) = &rule.name_pattern {
            if !matcher.is_match_std_path(std::path::Path::new(folder_name)) {
                continue;
            }
        }
        if let Some(matcher) = &rule.path_glob {
            if !matcher.is_match_std_path(std::path::Path::new(relative_path)) {
                continue;
            }
        }
        if !rule.parent_has_files.is_empty()
            && !rule
                .parent_has_files
                .iter()
                .any(|name| parent_filenames.iter().any(|p| *p == name))
        {
            continue;
        }
        return true;
    }
    false
}
