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

# `.lsp.json` — Claude Code-Compatible LSP Manifest

Per-root override file for language server configuration. Drop a `.lsp.json` at the root of any worktree to replace Zed's built-in LSP adapter resolution for the named server. Schema matches Claude Code's [plugin LSP servers spec](https://code.claude.com/docs/en/plugins-reference#lsp-servers), so the same file works in both tools.

## Schema

```json
{
  "<language-server-name>": {
    "command": "/abs/path/to/lsp-binary",
    "args": ["--option", "value"],
    "env": { "RUST_LOG": "debug" },
    "initializationOptions": { "...": "..." },
    "settings": { "...": "..." },
    "workspaceFolder": "subdir",
    "extensionToLanguage": { ".swift": "swift" }
  }
}
```

Fields wired through to Zed:

- `command` / `args` / `env` — overrides the binary Zed would otherwise resolve. When `command` is set, the adapter's bundled installer is skipped (treated as `ignore_system_version`).
- `initializationOptions` — folded into the LSP `initialize` params (deep-merged with adapter defaults; user values win).
- `settings` — folded into `workspace/didChangeConfiguration`.
- `workspaceFolder` — retargets the OS cwd, the LSP `rootUri`, and the entry in `workspaceFolders` to that subdirectory of the worktree. Useful when the project's build context lives in a subfolder (see "xcodeproj in a subdirectory" below). Both the cwd and the workspace-folder URI are retargeted, so build-server discovery (e.g. `buildServer.json`) and per-target source scope agree.

Fields parsed but not yet wired (Zed has no direct counterpart): `transport`, `startupTimeout`, `shutdownTimeout`, `restartOnCrash`, `maxRestarts`, `extensionToLanguage`.

## How it interacts with Zed's built-in adapters

By default the file augments — `command`/`args`/`env` are overrides, but `initializationOptions` / `settings` deep-merge with the adapter's existing values. The seed key for the language server is derived from the merged settings, so two `.lsp.json` files in different worktrees of the same project produce two independent OS processes.

To **disable** Zed's built-in manifest providers entirely (so the file becomes the single source of truth for LSP root detection), set:

```json
{
  "lsp_root": {
    "marker_files": [".lsp.json"],
    "disable_builtin_manifest_providers": true
  }
}
```

in `settings.json`. With this on, Zed only spawns servers for worktrees containing one of the marker files. Useful when you want LSP launches to be opt-in per repository.

# Swift / sourcekit-lsp — Setup Guide

This section walks through getting Swift code intelligence (go-to-def, references, completions) working in Zed for a project that uses Xcode (`.xcodeproj` / `.xcworkspace`) as its build system.

## What to install

1. **sourcekit-lsp** — ships with Xcode. Either:
   - `/usr/bin/sourcekit-lsp` (Xcode Command Line Tools — install via `xcode-select --install`).
   - `xcrun sourcekit-lsp` (current Xcode toolchain).
2. **xcode-build-server** — bridge that translates Xcode's per-file compile commands into the Build Server Protocol (BSP) format sourcekit-lsp speaks. Install via Homebrew: `brew install xcode-build-server`.

That's all the host tooling. No Zed extension required — the Swift adapter is built in, and `.lsp.json` lets you point it at the system binary.

## Wire-up

Two files at the worktree root:

`.lsp.json`:

```json
{
  "sourcekit-lsp": {
    "command": "/usr/bin/sourcekit-lsp",
    "extensionToLanguage": {
      ".swift": "swift",
      ".swiftinterface": "swift"
    }
  }
}
```

`buildServer.json` (generated by xcode-build-server — see below):

```json
{
  "name": "xcode build server",
  "version": "1.3.0",
  "bspVersion": "2.2.0",
  "languages": ["c", "cpp", "objective-c", "objective-cpp", "swift"],
  "argv": ["/opt/homebrew/bin/xcode-build-server"],
  "workspace": "/abs/path/to/Project.xcodeproj/project.xcworkspace",
  "build_root": "/Users/<you>/Library/Developer/Xcode/DerivedData/<derived-data-folder>",
  "scheme": "<your-scheme>",
  "kind": "manual",
  "indexStorePath": "/Users/<you>/Library/Developer/Xcode/DerivedData/<derived-data-folder>/Index.noindex/DataStore"
}
```

When sourcekit-lsp starts, it sees `buildServer.json` next to the worktree root, spawns `xcode-build-server` as a child via BSP, and asks it for compile flags + index path on every Swift file open.

## Generating `buildServer.json` and `.compile`

The first time, run from the directory that contains `buildServer.json` (typically the worktree root):

```bash
# 1. Do a clean build so xcodebuild emits per-file compile commands.
xcodebuild clean build \
  -project Path/To/Project.xcodeproj \
  -scheme YourScheme \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro Max' \
  2>&1 | tee /tmp/xcodebuild.log

# 2. Convert the build log into a compile-commands cache (.compile)
#    AND regenerate buildServer.json with `kind: manual` + `indexStorePath`.
xcode-build-server parse /tmp/xcodebuild.log
```

`xcode-build-server parse` writes `.compile` to the cwd, so run it from the directory you want to be the LSP root. Pass a real `xcodebuild` log (not just any file) — `parse` extracts compile commands from the build transcript.

Re-run the parse step whenever sources are added/removed or build settings change. Incremental builds don't re-emit compile commands for cached files, so use `clean build` for the parse-feeding run.

## Use case: `.xcodeproj` in a subdirectory

Common layout:

```
my-repo/
├── ios-app/
│   ├── Project.xcodeproj/
│   └── ios-app/...
├── shared-feature-a/
│   └── ui/src/iosMain/xcode/SomeFeature.swift   ← Swift sources used by ios-app
└── shared-feature-b/
    └── ui/src/iosMain/xcode/OtherFeature.swift
```

The Xcode project is at `ios-app/Project.xcodeproj` but Swift sources live anywhere in the repo. Two failure modes show up:

1. **rootURI = `ios-app/`** (e.g. via `.lsp.json` `workspaceFolder: "ios-app"`): sourcekit-lsp's BSP target only sees files **under** `ios-app/`. Files at `shared-feature-a/.../SomeFeature.swift` get classified as standalone — sourcekit-lsp falls back to default build settings, no cross-file go-to-def, no references, no module imports resolve. (Symptom in Console: `Producing syntactic diagnostics from the built-in swift-syntax because we have fallback build settings`.)
2. **rootURI = repo root, no `buildServer.json`**: sourcekit-lsp has no BSP server → fallback settings everywhere.

The recipe that works: **rootURI = repo root** AND `buildServer.json` + `.compile` at repo root. xcode-build-server still points at `ios-app/Project.xcodeproj` (`workspace` field is an absolute path), but the LSP scope covers every Swift source in the worktree.

Concretely:

```bash
# At repo root:
my-repo/
├── .lsp.json                    # no `workspaceFolder` — leave rootURI = my-repo/
├── buildServer.json             # workspace points at ios-app/Project.xcodeproj
├── .compile                     # produced by xcode-build-server parse
├── ios-app/
│   └── Project.xcodeproj/
└── shared-feature-a/...
```

Run the parse step from the repo root:

```bash
cd my-repo
xcodebuild clean build -project ios-app/Project.xcodeproj -scheme YourScheme \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro Max' \
  2>&1 | tee /tmp/xcodebuild.log
xcode-build-server parse /tmp/xcodebuild.log
```

The generated `.compile` and `buildServer.json` end up in the cwd — repo root.

## Troubleshooting

- **`No such module 'Foo'`** — Xcode build never reached the Swift compile phase, or the framework hasn't been built yet. Run a real `xcodebuild build` (not just clean) and confirm `Foo.framework` / `Foo.swiftmodule` lands in `DerivedData/.../Build/Products/<config>-<sdk>/`. Until that's there, sourcekit-lsp can't resolve the import.
- **Definitions return `null` for cross-file symbols, but in-file works** — sourcekit-lsp is on fallback build settings. Causes (in order of likelihood):
  1. `xcode-build-server` cache empty (`~/Library/Caches/xcode-build-server/<encoded-path>/compile_file-*` is `[]`). Fix: `xcode-build-server parse <xcodebuild.log>` from the LSP root.
  2. Source file is outside the rootURI (see "xcodeproj in a subdirectory" above).
  3. The build that produced the index store hasn't run since sources were added — re-run `xcodebuild build` so the `Index.noindex/DataStore` picks up new units / records.
- **Newly-added Swift file isn't compiled** — its path isn't in the xcodeproj. If you use a generator (e.g. `xcodegen`), regenerate the project so the file is added as a `PBXBuildFile`. Otherwise add it through Xcode's UI.
- **Confirm BSP is actually live**:
  ```bash
  pgrep -fa xcode-build-server   # should show a child of sourcekit-lsp
  ```
  Optionally point `XBS_LOGPATH=/tmp/xbs.log` in the `.lsp.json` `env` block to capture the BSP request/response trace.
- **Confirm sourcekit-lsp isn't on fallback**: `log show --predicate 'subsystem == "org.swift.sourcekit-lsp"' --last 5m | grep fallback`. Any "fallback build settings" hit on a file you care about means BSP wiring is broken for that file.
