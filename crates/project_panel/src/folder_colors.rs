//! Rule-based folder background colors for the project panel.
//!
//! Loaded from `project_panel.folder_colors` in settings.json (with
//! the standard global → per-project cascade Zed already applies).
//! Each rule matches a folder by any combination of name pattern,
//! path glob, sibling-file existence, and direct-child existence;
//! all matching rules are applied in declaration order, last
//! match's color winning.
//!
//! Colors are applied as a low-opacity background tint on the
//! folder row. Color values can be a hex string (`#ff8800`) or one
//! of a fixed set of theme-aware tokens that adapt to light/dark.

use gpui::{App, Hsla};
use settings::{RegisterSetting, Settings};
use settings_content::{FolderColorRuleContent, SettingsContent};
use std::sync::Arc;
use util::paths::{PathMatcher, PathStyle};

#[derive(Clone, Debug, PartialEq, RegisterSetting)]
pub struct FolderColorsSettings {
    pub rules: Arc<Vec<FolderColorRule>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FolderColorRule {
    pub name: String,
    pub name_pattern: Option<PathMatcher>,
    pub path_glob: Option<PathMatcher>,
    pub parent_has_files: Vec<String>,
    pub contains_files: Vec<String>,
    pub descendant_has_files: Vec<String>,
    pub is_ignored: Option<bool>,
    pub propagate_to_children: bool,
    pub background_color: ColorSpec,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ColorSpec {
    /// Pre-parsed RGBA in [0, 1].
    Hex(f32, f32, f32, f32),
    /// Theme-aware token. Resolved at render time via the active
    /// theme so light / dark themes get the right shade.
    Token(ColorToken),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorToken {
    Blue,
    Green,
    Orange,
    Red,
    Purple,
    Cyan,
    Magenta,
    Yellow,
    Accent,
    Created,
    Modified,
    Deleted,
    Conflict,
}

impl ColorToken {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "blue" => Self::Blue,
            "green" => Self::Green,
            "orange" => Self::Orange,
            "red" => Self::Red,
            "purple" => Self::Purple,
            "cyan" => Self::Cyan,
            "magenta" => Self::Magenta,
            "yellow" => Self::Yellow,
            "accent" => Self::Accent,
            "created" => Self::Created,
            "modified" => Self::Modified,
            "deleted" => Self::Deleted,
            "conflict" => Self::Conflict,
            _ => return None,
        })
    }
}

impl ColorSpec {
    /// Resolve to a render-time HSLA. Tokens map to the active
    /// theme; hex is returned as-is. Background opacity is reduced
    /// by the caller (see `tinted_background`) — this fn returns
    /// the source color at full alpha.
    pub fn resolve(&self, cx: &App) -> Hsla {
        match self {
            ColorSpec::Hex(r, g, b, a) => gpui::rgba(rgba_u32(*r, *g, *b, *a)).into(),
            ColorSpec::Token(token) => token_to_hsla(*token, cx),
        }
    }
}

/// Returns the folder-row background as an OPAQUE color: the
/// resolved tint blended over the panel's canvas background. This
/// matters because the project panel's sticky-scroll header layer
/// floats over the rest of the panel — a translucent tint there
/// shows through the rows behind it, which looks broken. Blending
/// to opaque makes the result identical visually but renders
/// correctly when stacked.
pub fn tinted_background(spec: &ColorSpec, cx: &App) -> Hsla {
    use theme::ActiveTheme as _;
    let tint = spec.resolve(cx);
    let canvas = cx.theme().colors().panel_background;
    blend_over(tint, canvas, 0.18)
}

fn blend_over(top: Hsla, bottom: Hsla, top_alpha: f32) -> Hsla {
    let (tr, tg, tb) = hsla_to_rgb(top);
    let (br, bg, bb) = hsla_to_rgb(bottom);
    let a = top_alpha.clamp(0.0, 1.0);
    let r = tr * a + br * (1.0 - a);
    let g = tg * a + bg * (1.0 - a);
    let b = tb * a + bb * (1.0 - a);
    rgb_to_hsla(r, g, b)
}

fn hsla_to_rgb(c: Hsla) -> (f32, f32, f32) {
    // gpui::Hsla is HSL with alpha; convert to sRGB triplet.
    let h = c.h;
    let s = c.s;
    let l = c.l;
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);
    (r, g, b)
}

fn hue_to_rgb(p: f32, q: f32, mut t: f32) -> f32 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        p + (q - p) * 6.0 * t
    } else if t < 0.5 {
        q
    } else if t < 2.0 / 3.0 {
        p + (q - p) * (2.0 / 3.0 - t) * 6.0
    } else {
        p
    }
}

fn rgb_to_hsla(r: f32, g: f32, b: f32) -> Hsla {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f32::EPSILON {
        return Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        };
    }
    let d = max - min;
    let s = if l > 0.5 { d / (2.0 - max - min) } else { d / (max + min) };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / d) + if g < b { 6.0 } else { 0.0 }
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    } / 6.0;
    Hsla { h, s, l, a: 1.0 }
}

fn token_to_hsla(token: ColorToken, cx: &App) -> Hsla {
    use theme::ActiveTheme as _;
    let theme = cx.theme();
    let status = theme.status();
    match token {
        ColorToken::Accent => theme.colors().text_accent,
        ColorToken::Created => status.created,
        ColorToken::Modified => status.modified,
        ColorToken::Deleted => status.deleted,
        ColorToken::Conflict => status.conflict,
        // Named hues — picked to look roughly the same across the
        // shipped light & dark themes. We bake H/S/L directly so the
        // tint reads consistently regardless of which theme is
        // active; rendering uses the alpha reduction above.
        ColorToken::Blue => Hsla {
            h: 211. / 360.,
            s: 0.70,
            l: 0.55,
            a: 1.0,
        },
        ColorToken::Green => Hsla {
            h: 145. / 360.,
            s: 0.55,
            l: 0.50,
            a: 1.0,
        },
        ColorToken::Orange => Hsla {
            h: 30. / 360.,
            s: 0.85,
            l: 0.55,
            a: 1.0,
        },
        ColorToken::Red => Hsla {
            h: 0. / 360.,
            s: 0.70,
            l: 0.55,
            a: 1.0,
        },
        ColorToken::Purple => Hsla {
            h: 270. / 360.,
            s: 0.55,
            l: 0.60,
            a: 1.0,
        },
        ColorToken::Cyan => Hsla {
            h: 190. / 360.,
            s: 0.65,
            l: 0.55,
            a: 1.0,
        },
        ColorToken::Magenta => Hsla {
            h: 320. / 360.,
            s: 0.65,
            l: 0.60,
            a: 1.0,
        },
        ColorToken::Yellow => Hsla {
            h: 50. / 360.,
            s: 0.85,
            l: 0.55,
            a: 1.0,
        },
    }
}

fn rgba_u32(r: f32, g: f32, b: f32, a: f32) -> u32 {
    let r = (r.clamp(0., 1.) * 255.0) as u32;
    let g = (g.clamp(0., 1.) * 255.0) as u32;
    let b = (b.clamp(0., 1.) * 255.0) as u32;
    let a = (a.clamp(0., 1.) * 255.0) as u32;
    (r << 24) | (g << 16) | (b << 8) | a
}

fn parse_color(s: &str) -> Option<ColorSpec> {
    if let Some(token) = ColorToken::parse(s) {
        return Some(ColorSpec::Token(token));
    }
    let hex = s.trim().trim_start_matches('#');
    let (r, g, b, a) = match hex.len() {
        6 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            255u8,
        ),
        8 => (
            u8::from_str_radix(&hex[0..2], 16).ok()?,
            u8::from_str_radix(&hex[2..4], 16).ok()?,
            u8::from_str_radix(&hex[4..6], 16).ok()?,
            u8::from_str_radix(&hex[6..8], 16).ok()?,
        ),
        _ => return None,
    };
    Some(ColorSpec::Hex(
        r as f32 / 255.0,
        g as f32 / 255.0,
        b as f32 / 255.0,
        a as f32 / 255.0,
    ))
}

fn build_matcher(pattern: &str) -> Option<PathMatcher> {
    PathMatcher::new([pattern.to_string()], PathStyle::Posix).ok()
}

fn rule_from_content(label: &str, raw: &FolderColorRuleContent) -> Option<FolderColorRule> {
    let bg_str = raw.background_color.as_deref()?;
    let bg = parse_color(bg_str)?;
    let name_pattern = raw.name_pattern.as_deref().and_then(build_matcher);
    let path_glob = raw.path_glob.as_deref().and_then(build_matcher);
    if name_pattern.is_none()
        && path_glob.is_none()
        && raw.parent_has_files.as_ref().map_or(true, |v| v.is_empty())
        && raw.contains_files.as_ref().map_or(true, |v| v.is_empty())
        && raw
            .descendant_has_files
            .as_ref()
            .map_or(true, |v| v.is_empty())
        && raw.is_ignored.is_none()
    {
        // A rule with no match dimensions would fire on every
        // folder — almost certainly a config error. Skip it.
        return None;
    }
    Some(FolderColorRule {
        name: raw.name.clone().unwrap_or_else(|| label.to_string()),
        name_pattern,
        path_glob,
        parent_has_files: raw.parent_has_files.clone().unwrap_or_default(),
        contains_files: raw.contains_files.clone().unwrap_or_default(),
        descendant_has_files: raw.descendant_has_files.clone().unwrap_or_default(),
        is_ignored: raw.is_ignored,
        propagate_to_children: raw.propagate_to_children.unwrap_or(false),
        background_color: bg,
    })
}

/// JSON file that ships the fork's default folder-color rules.
/// Edit the file (rebuild required) to change defaults — user
/// overrides via `settings.json -> project_panel.folder_colors`
/// are independent and replace this list entirely when present.
const DEFAULT_RULES_JSON: &str = include_str!("../default_folder_colors.json");

fn default_rules() -> Vec<FolderColorRule> {
    let raw: Vec<FolderColorRuleContent> =
        serde_json::from_str(DEFAULT_RULES_JSON).unwrap_or_else(|err| {
            log::error!(
                "claude-review-v2 fork: malformed default_folder_colors.json: {err}"
            );
            Vec::new()
        });
    raw.iter()
        .enumerate()
        .filter_map(|(i, raw)| rule_from_content(&format!("default-{i}"), raw))
        .collect()
}

impl Settings for FolderColorsSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let rules = match content
            .project_panel
            .as_ref()
            .and_then(|p| p.folder_colors.as_ref())
        {
            None => default_rules(),
            // An explicit empty list opts out of defaults entirely.
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

/// Match check used by the project panel to decide a folder row's
/// background color. Returns the *last* matching rule's color
/// (declaration order). `has_descendant_with_name` is invoked at
/// most once per rule and only when needed — the caller can scan
/// the worktree subtree there.
/// Light-weight match input. The render path constructs one of
/// these per visited path (the entry itself + each ancestor when a
/// propagate rule is active).
pub struct EntryCtx<'a> {
    pub name: &'a str,
    pub relative_path: &'a str,
    pub parent_filenames: &'a [&'a str],
    pub direct_child_filenames: &'a [&'a str],
    pub is_ignored: bool,
}

fn rule_matches_self(
    rule: &FolderColorRule,
    ctx: &EntryCtx,
    has_descendant_with_name: &mut dyn FnMut(&str) -> bool,
) -> bool {
    if let Some(required) = rule.is_ignored {
        if required != ctx.is_ignored {
            return false;
        }
    }
    if let Some(matcher) = &rule.name_pattern {
        if !matcher.is_match_std_path(std::path::Path::new(ctx.name)) {
            return false;
        }
    }
    if let Some(matcher) = &rule.path_glob {
        if !matcher.is_match_std_path(std::path::Path::new(ctx.relative_path)) {
            return false;
        }
    }
    if !rule.parent_has_files.is_empty()
        && !rule
            .parent_has_files
            .iter()
            .any(|name| ctx.parent_filenames.iter().any(|p| *p == name))
    {
        return false;
    }
    if !rule.contains_files.is_empty()
        && !rule
            .contains_files
            .iter()
            .any(|name| ctx.direct_child_filenames.iter().any(|c| *c == name))
    {
        return false;
    }
    if !rule.descendant_has_files.is_empty()
        && !rule
            .descendant_has_files
            .iter()
            .any(|name| has_descendant_with_name(name))
    {
        return false;
    }
    true
}

/// Match check used by the project panel to decide an entry's
/// background color.
///
/// Behavior per rule (in JSON declaration order; last hit wins):
///   - `propagate_to_children: false` (default) — matches only when
///     the entry itself satisfies every dimension.
///   - `propagate_to_children: true` — matches when the entry OR
///     any of its ancestors (closest first) satisfies the rule.
///     For propagate rules, file-existence dimensions
///     (`parent_has_files`, `contains_files`, `descendant_has_files`)
///     are intentionally *not* re-checked against ancestors —
///     they would be expensive and rarely make sense in propagate
///     mode. `name_pattern` / `path_glob` / `is_ignored` are.
pub fn match_color(
    rules: &[FolderColorRule],
    self_ctx: &EntryCtx,
    ancestors: &[EntryCtx],
    mut has_descendant_with_name: impl FnMut(&str) -> bool,
) -> Option<ColorSpec> {
    let mut hit: Option<&FolderColorRule> = None;
    for rule in rules {
        let mut matched_self =
            rule_matches_self(rule, self_ctx, &mut has_descendant_with_name);
        if !matched_self && rule.propagate_to_children {
            for ancestor in ancestors {
                let pattern_only_rule = FolderColorRule {
                    name: rule.name.clone(),
                    name_pattern: rule.name_pattern.clone(),
                    path_glob: rule.path_glob.clone(),
                    parent_has_files: Vec::new(),
                    contains_files: Vec::new(),
                    descendant_has_files: Vec::new(),
                    is_ignored: rule.is_ignored,
                    propagate_to_children: rule.propagate_to_children,
                    background_color: rule.background_color.clone(),
                };
                if rule_matches_self(&pattern_only_rule, ancestor, &mut |_| false) {
                    matched_self = true;
                    break;
                }
            }
        }
        if matched_self {
            hit = Some(rule);
        }
    }
    hit.map(|r| r.background_color.clone())
}
