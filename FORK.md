# Review Mode (`zed --review`)

Centralizes how Claude Code sessions get human review before commit / PR. Opens a dedicated GUI showing the full diff (HEAD vs working tree) of every changed file in one stacked view, lets the user drop comments on multiple files / line ranges in one pass, then ships them all back to Claude Code at once via "Send Review to Agent". Claude Code then walks each comment as a discrete instruction.

Use:

1. Build + install fork: `script/bundle-mac -i`
2. Symlink CLI to PATH: `ln -sf "/Applications/Zed.app/Contents/MacOS/cli" /usr/local/bin/zed`
3. Install Claude Code skill: `zed --install-review-skill` (writes `~/.claude/skills/start-review/SKILL.md`)
4. In any Claude Code session, type `/start-review` (or `/start-review /path/to/repo`)

Closing the review window without sending exits non-zero — skill reports "review cancelled".

# Pull Request Panel (`gh`-backed)

New left-dock panel between Git (priority 3) and Collab (priority 5). Browse, read, and review GitHub pull requests for the current project's GitHub remote without leaving Zed. Reuses the **same inline review-comment overlay UI** that claude-review's `/start-review` flow uses — but bound to a real GitHub PR instead of a local diff session.

Activates when:

- The active project has a `github.com` remote (parsed from `origin` URL — handles `https://`, `git@`, `ssh://` forms).
- A GitHub API token is resolvable via `util::github_auth::github_token()` (env var `GITHUB_TOKEN` or `gh auth token` fallback — see "GitHub Token" section below).

If either gate fails, the panel renders a hint instead of a list. Repository discovery is asynchronous on cold opens; the panel subscribes to `GitStoreEvent` and auto-populates as soon as the worktree's git remotes finish loading — no need to click Refresh.

## PR list panel

- PRs sorted by recently-updated. Title, `#N`, author, head branch, draft state.
- **Pagination**: walks up to 5 pages of 100 (max 500 PRs) via the GitHub `page=N` query param. Stops as soon as a page returns < 100 entries.
- **Filter bar**:
  - Search box: case-insensitive substring match on title / author / branch / `#N`. Live as you type.
  - Author dropdown: trigger shows the selected author's avatar + login (or "Any author"). Click opens a popover with a search input + scrollable list of every distinct author across the loaded PRs. Each row: avatar + login + PR-count badge. **Sorted by PR count descending** (most-active authors at the top), alpha tiebreak. `×` clears the filter; "Any author" entry inside the popover does the same.
  - State segmented control: `Open` / `Closed` / `All`. Switching re-fetches with the matching `state=` query param.
  - All filters AND together. Author count badge reflects whatever `state` is currently loaded — switch to `All` for true cross-state totals.
- Per-row "open in browser" icon.
- Header refresh button.
- Auto-refresh on every panel activation (initial reveal, switching back from another panel, restored layout) and on `GitStoreEvent` (`RepositoryAdded` / `RepositoryUpdated` / `ActiveRepositoryChanged`). No periodic poll.
- Click a row → opens an overview tab (`PrView`).

## PR overview tab (`PrView`)

- **Header**: author avatar, title, `#N`, mergeability **status pill** (`Mergeable` ✓ / `Conflicts` ⚠ / `Checking` ⟳), **Files (N)** button, **Checkout** button (separate, opt-in `gh pr checkout`), "open on GitHub" link, `by author · head → base`, `+adds −dels · K files`.
- **Description**: rendered Markdown of the PR body.
- **CI checks**: per-workflow row — status icon, name, colored outcome (success / failure / cancelled / skipped / neutral / timed out / action required / queued / running). Each row clickable → opens that check on github.com. Backed by `GET /commits/:sha/check-runs`.
- **File review comments summary**: one-line banner showing `N file review comment(s) — open the Files tab to read and reply inline.` Hidden when count is 0.
- **Commits**: per-commit row — author avatar, short sha (mono), first-line message, author name. Click → opens commit on github.com. Backed by `GET /pulls/:n/commits`.
- **Conversation timeline**: top-level (Issue API) comments only — not duplicated with the line-anchored ones (those live in Files). Each comment renders with avatar + bold author + soft-wrapping body.
- **Composer + 3 buttons**: Comment (top-level conversation), Approve, Request changes. Empty body allowed for Approve / Request-changes.
- All endpoints fan out via `futures::join!` so the tab paints in roughly the slowest call's time, not the sum.

## Files diff tab (`PrFilesView`)

- **In-memory side-by-side diff**, never touches the working tree (no checkout, no fetch, no staging).
- For each changed file: fetches base content at the PR's `base_ref` and head content at the PR's `head_sha` via the GitHub `contents` API. Builds a synthetic `Buffer` (with a synthetic `language::File` so the multi-buffer header banner shows the real filename instead of "untitled") + a `BufferDiff` whose base text is the base content. Assembles all files into a single `MultiBuffer` rendered through a `SplittableEditor` (left = base, right = head; settings-driven via `EditorSettings.diff_view_style`).
- **Tree-sitter syntax highlighting** on both sides via `LanguageRegistry::load_language_for_file_path`.
- **All hunks expanded** by default (`set_all_diff_hunks_expanded`).
- **Parallel file fetching** — concurrency 6 via `FuturesOrdered`, order preserved so excerpts appear in PR order. Progress shown as `Fetching X of Y: path/to/file`.
- **Breakpoint dots, inline diagnostics, runnable indicators** all hidden (none of them mean anything for a PR diff outside the worktree).

## Inline line comments

- Reuses **claude-review's diff-review overlay UI** (the same `+` button per line + drag-to-multiselect + popup composer + stacked thread block that `/start-review` already uses). Enabled by force-flipping `DiffReviewFeatureFlag::enabled_for_all = true` in this fork.
- **Existing PR review comments load into the same overlay** — `Editor::add_remote_review_comment(...)` stores them in the editor's `stored_review_comments` with author + avatar + remote flag, so they render in the same `render_comment_row` path as locally typed ones (no separate "block decoration" code). Each commented line auto-opens its overlay via `Editor::inject_remote_review_comment(display_row, body, author, avatar, ...)` so threads are visible without clicking `+`.
- **New comments**: drag-select line range → composer pops below → type → Enter. The local `EditorEvent::ReviewCommentsChanged` subscription POSTs every newly-stored comment to GitHub via `POST /pulls/:n/reviews` with `event=COMMENT` + `comments=[{path, line, side: "RIGHT", body}]` + `commit_id=head_sha` (matches GitHub's "single comment review" path). Local IDs tracked in `posted_comment_ids` to prevent double-post on `fetch` round-trip.
- **ESC** dismisses the empty composer only; overlays with stored threads stay open (`dismiss_overlays_without_comments` instead of `dismiss_all_diff_review_overlays`).
- **Chevron toggle** (collapse/expand thread) targets the correct overlay via `editor_handle.update + hunk_keys_match`, even with multiple overlays open. Header click also stops mouse-down propagation so the editor underneath doesn't take focus.
- Comment bodies **soft-wrap** (`min_w_0` + `whitespace_normal`); overlay block height is sized from per-comment body length so long threads don't clip.

## Editor changes (used by both PR view and existing claude-review)

- `StoredReviewComment` gained `author: Option<SharedString>`, `avatar_url: Option<SharedUri>`, `is_remote: bool`.
- `Editor::add_remote_review_comment(...)` stores remote-attributed comments.
- `Editor::inject_remote_review_comment(display_row, ...)` opens the overlay AND stores the comment with the same hunk_key the overlay just computed (single atomic step — fixes a snapshot-drift bug where opening a `+` later would dismiss remote-comment overlays because their hunk_keys didn't match).
- `render_comment_row` shows per-comment author + avatar above the body when present (falls back to the local user's avatar when not).
- `calculate_overlay_height` estimates rows from each comment body's length so wrapped text doesn't get clipped.

## What it does **not** do (yet)

- No assignee / reviewer / label filtering (state + author + free-text search are wired; the others would require GitHub's search API or extra REST roundtrips).
- No reply-to-existing-thread (a new line comment posts a fresh single-comment review; it doesn't post into an existing GitHub review thread).
- No resolve / unresolve thread.
- No webhook or push-based realtime updates — the panel auto-refreshes only on activation, not while it's already open.
- No support for self-hosted GitHub Enterprise URLs (only `github.com` is parsed).

Implementation: `crates/pr_panel/`. Four files: `pr_panel.rs` (Panel + PR list + auto-refresh on activate / GitStore events), `pr_view.rs` (overview tab: header, CI checks, commits, description, conversation, composer), `pr_files_view.rs` (read-only side-by-side multibuffer + claude-review overlay integration), `github_api.rs` (typed REST wrapper). Auth flows through `util::github_auth`. Dock activation priority `4` puts the icon left of Collab.

# GitHub Token — `gh` CLI Auto-Auth Fallback

Zed reaches GitHub's REST API in two places: latest-release lookups for built-in LSP / DAP / extension downloads (`http_client::github`) and commit-author avatars in blame popovers (`git_hosting_providers::github`). Upstream, both call sites only authenticate when the `GITHUB_TOKEN` environment variable is set; otherwise the requests go out unauthenticated and hit the 60 req/hr/IP limit, which manifests as `403 rate limit exceeded` on cold starts behind a busy NAT.

This fork adds a second resolution step. If `GITHUB_TOKEN` is missing, Zed shells out to `gh auth token` (the [GitHub CLI](https://cli.github.com)). When `gh` is installed and already authenticated, the request gets the user's existing token transparently — no Zed-side login flow, no keychain entry, no extra config.

Resolution order (memoized for the process lifetime, first non-empty wins):

1. `GITHUB_TOKEN` env var.
2. `gh auth token` stdout, when `gh` is on `PATH`.

If neither yields a token, requests stay unauthenticated (upstream behavior).

Use:

- Already on `gh`? Nothing to do — `gh auth status` should show "Logged in to github.com".
- Need to log in: `gh auth login`.
- Force a specific token instead of using `gh`: export `GITHUB_TOKEN=…`. The env var always wins.
- Rotated `gh` credentials? Restart Zed — the resolved token is cached for the process lifetime (matches the existing `GITHUB_TOKEN` contract).

Implementation: `crates/util/src/github_auth.rs` exposes `util::github_auth::github_token() -> Option<&'static str>`. Both call sites await it instead of reading the env var directly. The `gh` subprocess is only spawned the first time a token is requested and only when `gh` is actually on `PATH` (`which` lookup gates the spawn).

This does **not** unlock any new authenticated GitHub features — it only removes the rate-limit cliff for the API calls Zed already makes. The Copilot sign-in flow has its own device-flow auth and is unrelated.

# Search Everywhere (`shift shift`)

Inspired by IntelliJ IDEs' Search Everywhere. Unified fuzzy picker — **LSP workspace symbols + files + actions** in one modal. Bound in JetBrains keymap.

Use: press `shift shift`. Type. ENTER opens the file / jumps to symbol / dispatches the action. ESC closes. Hover any row for full-path tooltip. Toggle "Include .gitignored files" at bottom. Last query restored on reopen; active editor's selection prefills on open.

Sort order (priority bucket — lower wins, ties broken by descending fuzzy score):

0. **LSP workspace symbols** — pulled from `Project::symbols(query)`, which fans out to every running language server's `workspace/symbol` handler. Filtered to `SymbolLocation::InProject` so dependency sources (Xcode `SourcePackages/checkouts`, `~/.gradle/caches`, sourcekit-lsp generated interfaces) don't pollute results. Re-ranked client-side with `fuzzy_nucleo` so subsequence matches surface — typing `launchSubscriber` finds `launchSubscriberAwareMolecule`. Exact-prefix and substring matches get progressively bigger boosts.
1. **Action exact-prefix** — when the action's humanized name starts with the query (mirrors IntelliJ's "type what the action does").
2. **Files** — path-aware fuzzy via `match_path_sets` with a filename-substring boost on top: a `.kt` whose basename starts with / contains the query promotes ahead of deep paths whose components incidentally match.
3. **Directories and remaining actions**.

## Performance shape

LSP symbol queries are expensive — `workspace/symbol` fans out to every running server (kotlin-lsp, sourcekit-lsp, …) and a single-character query can return thousands of partial hits. The picker is structured to keep the UI snappy:

- **Two-phase update.** Phase 1 (files / dirs / actions, all local) publishes immediately so the picker has results to show within ~10 ms. Phase 2 (LSP `workspace/symbol`) awaits separately and merges its results in when ready, without blocking phase 1.
- **Length gate.** LSP queries fire only when the typed query is ≥ 2 characters. Single-char queries are too broad and would dump the entire symbol DB on every keystroke.
- **Cancel-on-keystroke.** Each new keystroke replaces the in-flight task; the dropped task auto-cancels, and a `cancel_flag` check at every publish point drops stale results before they render.
- **Server-side cancellation.** The LSP task drops when superseded — Zed stops polling, and the server's pending `workspace/symbol` request is abandoned client-side. The server may still finish processing it, but no time is spent on the response.

Net effect: the picker feels instantaneous on typed queries even when kotlin-lsp is mid-indexing, and stops sending traffic to the LSP the moment the user types another character.

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

# KMP — Swift→Kotlin Go-to-Definition

In a Kotlin Multiplatform project, Kotlin types exported via the `Shared` (or any other) framework appear in Swift as ObjC-imported types: `import Shared` then reference `BarPresenter`, `Foo`, etc. Stock sourcekit-lsp resolves Go-to-Definition on these to one of two synthesized targets:

- `<TempDir>/sourcekit-lsp/GeneratedInterfaces/<hash>/<Module>.swiftinterface` — Swift-style stub
- `<DerivedData>/Build/Products/<config>-<sdk>/<Module>.framework/Headers/<Module>.h` — ObjC header (with the framework name prefixed onto every type, e.g. `SharedBarPresenter`)

Neither is the actual source. In a KMP project the truth is the `.kt` file the framework was compiled from.

This fork transparently redirects those targets to the originating Kotlin source by:

1. Detecting that the Go-to-Definition target points at one of the two synthesized locations above.
2. Reading the symbol identifier at that location (peeling off the framework prefix when present — `SharedBarPresenter` → `BarPresenter`).
3. Sending `workspace/symbol` to the kotlin-lsp running in the same workspace.
4. Picking the best match (exact name; preferring `commonMain` over platform-specific source sets).
5. Replacing the LSP response with the Kotlin source location before Zed opens the buffer.

If kotlin-lsp is not running, returns no exact match, or the redirect fails for any other reason, the original swiftinterface / header location is used (existing behavior). No setting; activates whenever both `sourcekit-lsp` and `kotlin-lsp` are configured for the worktree.

Limits in v1:

- `@ObjCName` renames lose the link (Kotlin `Foo` annotated as `KFoo` in ObjC won't be found by name). A build-time Kotlin↔ObjC index would close this gap.
- Generic erasure across the ObjC bridge — `BarPresenter<Model>` resolves to `BarPresenter`. Still finds the right declaration; type parameters aren't carried over.
- Symbols with the same name in multiple Kotlin files fall back to the `commonMain`-preferred match. For multi-actual KMP types this is usually right, but not always.

Implementation: `crates/project/src/kmp_swift_to_kotlin.rs`. Hook lives in `lsp_command::location_links_from_lsp` and runs before LSP `Location` → Zed `Location` conversion, so it composes with everything downstream (preview, multi-buffer, peek-definition, etc.).

Debug: `tail -f ~/Library/Logs/Zed/Zed.log | grep kmp_swift_to_kotlin` shows `redirecting <swiftinterface-or-header> (<raw-symbol> -> <stripped-symbol>) -> <kt-uri>` on hits, `redirect failed: ...` on misses.

# JVM Library Sources — `<jar>!/<entry>` Path Support

When kotlin-lsp / jdtls / any JVM language server resolves Go-to-Definition into a third-party dependency, it returns paths in the JVM archive convention:

```
/Users/<you>/.gradle/caches/modules-2/files-2.1/.../kotlinx-coroutines-core-jvm-1.10.2-sources.jar!/commonMain/kotlinx/coroutines/CoroutineScope.kt
```

The `!/` separator points inside a `.jar` — there's no real file at that path, so stock Zed errored with **"Failed to open …"**.

This fork detects the `<archive>.jar!/<entry>` shape, extracts the requested entry once into a per-jar cache, and opens the cached file with the standard buffer flow. Result: clicking through into `CoroutineScope`, `Flow`, any third-party `.kt`/`.java` source, just works.

Cache location: `<data_dir>/jar-extracts/<sha-prefix>/<entry-relative-path>`.

- macOS: `~/Library/Application Support/Zed/jar-extracts/`
- Linux: `~/.local/share/zed/jar-extracts/`

Cached files are chmod'd `0o444` (Unix) or marked read-only (Windows) so stray edits don't accidentally diverge from the upstream archive. Re-extracted automatically when the source jar's mtime is newer than the cache file.

Trade-offs:

- The opened buffer is a real on-disk file in the cache, not a virtual one. Editor features (search, outline, find references inside the file, language-aware navigation) work normally.
- The cache grows as you navigate dependencies. Manual cleanup: `rm -rf ~/Library/Application\ Support/Zed/jar-extracts/`. No automatic GC in v1 — cache is small relative to source archives and is content-addressed by jar path so it dedupes naturally.
- Symbolic linking would be lighter-weight but breaks across filesystems and on Windows. Extraction wins on portability.

Implementation: `crates/project/src/jar_extract.rs`, hooked into `LspStore::open_local_buffer_via_lsp` so every LSP-driven buffer open benefits, not just Go-to-Definition.
