# Review Mode (`zed --review`)

Centralizes how Claude Code sessions get human review before commit / PR. Opens a dedicated GUI showing the full diff (HEAD vs working tree) of every changed file in one stacked view, lets the user drop comments on multiple files / line ranges in one pass, then ships them all back to Claude Code at once via "Send Review to Agent". Claude Code then walks each comment as a discrete instruction.

Use:

1. Build + install fork: `script/bundle-mac -i`
2. Symlink CLI to PATH: `ln -sf "/Applications/Zed.app/Contents/MacOS/cli" /usr/local/bin/zed`
3. Install Claude Code skill: `zed --install-review-skill` (writes `~/.claude/skills/start-review/SKILL.md`)
4. In any Claude Code session, type `/start-review` (or `/start-review /path/to/repo`)

Closing the review window without sending exits non-zero — skill reports "review cancelled".

# Search Everywhere (`shift shift`)

Inspired by IntelliJ IDEs' Search Everywhere. Unified fuzzy picker — files + actions in one modal. Bound in JetBrains keymap.

Use: press `shift shift`. Type. Filename-substring match wins for files; exact-prefix match wins for actions. ENTER opens / dispatches. ESC closes. Hover any row for full-path tooltip. Toggle "Include .gitignored files" at bottom. Last query restored on reopen; active editor's selection prefills on open.

# Auto-run `cmd+shift+F` from Selection

When `cmd+shift+F` opens with the active editor's selection / word-under-cursor as the prefilled query, search runs immediately. No Enter needed.

Use: select text → `cmd+shift+F` → results appear right away.

# Project Panel — Rule-Based Folder & File Colors

Tints rows in the project panel with a subtle theme-aware background. Inspired by IntelliJ's "Sources Root" / "Test Sources Root" / "Excluded" coloring. Defaults cover the Kotlin / Gradle layout (KMP `*Main` / `*Test`, plain `src/main` / `src/test`, `build.gradle.kts`, gradle module roots, `build/` output, gitignored folders).

Configure: add `folder_colors` under `project_panel` in `~/Library/Application Support/Zed/settings.json` (global) or `.zed/settings.json` (per-project). Rules cascade and merge with global settings using Zed's standard rules.

```json
{
  "project_panel": {
    "folder_colors": [
      {
        "name": "my-rule",
        "name_pattern": "*Main",
        "path_glob": "**/src/*Main",
        "parent_has_files": ["build.gradle.kts"],
        "contains_files": ["package.json"],
        "descendant_has_files": ["build.gradle.kts"],
        "is_ignored": true,
        "propagate_to_children": true,
        "background_color": "blue"
      }
    ]
  }
}
```

Each rule's match dimensions are AND'd. Multiple matching rules apply in declaration order; the last match's color wins.

Match dimensions:

- `name_pattern` — glob against the row's name (last path segment), e.g. `"*Main"`, `"build.gradle.kts"`.
- `path_glob` — glob against the worktree-relative path, e.g. `"**/src/*Main/kotlin"`.
- `parent_has_files` — fires only when the row's parent directory contains at least one of these filenames.
- `contains_files` — fires only when the row (folder) directly contains at least one of these filenames.
- `descendant_has_files` — fires when the row's subtree contains any of these filenames anywhere. Skips gitignored subtrees for performance.
- `is_ignored` — `true` to fire only on gitignored rows; `false` to fire only on non-ignored. Omit to ignore status.
- `propagate_to_children` — when `true`, the rule's color is also applied to every descendant row.

Color values:

- Hex strings: `"#ff8800"`, `"#ff8800cc"` (with alpha).
- Theme-aware tokens (recommended; adapt to light/dark): `blue`, `green`, `orange`, `red`, `purple`, `cyan`, `magenta`, `yellow`, `accent`, `created`, `modified`, `deleted`, `conflict`.

Replace defaults / opt out:

- Don't define `folder_colors` at all → defaults from `crates/project_panel/default_folder_colors.json` are used.
- Define `folder_colors: []` → no rules at all (defaults disabled, nothing gets tinted).
- Define `folder_colors: [...]` with your own rules → fully replaces defaults; no merging with the built-in list.

Performance:

- Match results are cached per `(worktree_id, path)`. The cache is cleared whenever the worktree emits an entries-update event so changes to marker files (e.g. a new `build.gradle.kts`) take effect on the next paint.
- Each match-dimension scan only runs when the active ruleset references it. With no rules, the lookup is a no-op.

# Project Panel — Tree-ASCII Indent Guides

Adds a per-row horizontal elbow that meets the existing vertical indent guide so the project tree reads more like a `tree`-style outline instead of disconnected dashes. No configuration — automatic at depth > 0.

# Project Panel — Fold-Chain Exceptions

Zed already merges single-child folder chains into one row when `auto_fold_dirs` is enabled (e.g. `src/commonMain/kotlin/co/foo/bar`). This feature adds **rules that break that chain** so logical boundaries (source-set roots, language roots, gradle modules) always render on their own line.

Configure via `project_panel.fold_exceptions` in `settings.json` (global or per-project `.zed/settings.json`). Defaults cover the Kotlin / Gradle layout: `kotlin/` inside `src/*Main/`, `src/*Test/`, `src/main/`, `src/test/`; the `*Main` / `*Test` / `main` / `test` source-set folders themselves; and `src/` whose parent has a `build.gradle.kts`.

```json
{
  "project_panel": {
    "fold_exceptions": [
      {
        "name": "my-rule",
        "name_pattern": "kotlin",
        "path_glob": "**/src/*Main/kotlin",
        "parent_has_files": ["build.gradle.kts"],
        "is_ignored": false
      }
    ]
  }
}
```

Match dimensions are AND'd. A folder matched by ANY rule breaks the chain. Available dimensions:

- `name_pattern` — glob against the folder name.
- `path_glob` — glob against the worktree-relative path.
- `parent_has_files` — fires only when the parent dir contains any of the listed filenames.
- `is_ignored` — restrict to gitignored / non-ignored folders.

Replace defaults / opt out:

- Omit `fold_exceptions` → defaults from `crates/project_panel/default_fold_exceptions.json` apply.
- `fold_exceptions: []` → all rules disabled, full upstream collapse behavior.
- `fold_exceptions: [...]` → fully replaces defaults.

Result: a tree like `src/commonMain/kotlin/co/foo/bar` renders as

```
src / commonMain / kotlin /
   co / foo / bar /
      …files…
```

instead of one giant collapsed row.
