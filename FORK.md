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
