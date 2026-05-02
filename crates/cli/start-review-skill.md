---
name: start-review
description: Launch the Zed-based review surface (claude-review-v2 fork) to
  review uncommitted git changes (HEAD vs working tree). The user walks
  file-by-file in the dedicated review window, leaves line-level comments,
  and clicks "Send Review to Agent". This skill captures the resulting
  structured payload and acts on each comment as a discrete instruction.
  Triggers "/start-review", "let me review this", "open the review tool".
argument-hint: "[path-to-repo]"
allowed-tools: Bash(zed:*) Bash(zed-review:*) Read
---

# Start Review

Launch the review surface, wait for the user to submit, and act on the
comments.

## Steps

1. **Resolve the repo path.** Use `$1` if the user passed an argument,
   otherwise the current working directory. The path must be inside a
   git repository (walk parents looking for `.git`); abort with an
   explanatory message if not.

2. **Spawn the review window.** Run:

       zed --review <repo-path>

   The CLI blocks until the user clicks "Send Review to Agent" or
   closes the window. On Send Review, it writes a JSON payload to
   stdout and exits 0. Non-zero exit means cancellation — stop and
   report the message Zed wrote to stderr.

   If the `zed` CLI is not on `$PATH`, instruct the user to install
   the claude-review-v2 fork (`cargo run -p cli --release --` or
   add the built binary to PATH) and stop.

3. **Parse the JSON.** Validate it matches the schema in
   `claude-review/docs/SPECIFICATION.md` §6.2:

       {
         "repo_root": "...",
         "head_sha": "..." | null,
         "head_branch": "..." | null,
         "files": [
           {
             "path": "...",
             "old_path": "..." | null,
             "status": "modified" | "added" | "deleted" | "renamed" | "untracked",
             "old_content": { "kind": "text" | "binary" | "missing", "text"?: "..." },
             "new_content": { "kind": "text" | "binary" | "missing", "text"?: "..." }
           }, ...
         ],
         "comments": [
           {
             "id": <int>,
             "file_path": "...",
             "side": "old" | "new",
             "line_start": <int>,
             "line_end": <int>,
             "body": "..."
           }, ...
         ]
       }

   On parse failure, dump the raw output and stop.

4. **Handle empty comments.** If `comments` is empty, tell the user
   "Review submitted with no comments — nothing to act on." and stop.

5. **Group comments by file.** For each unique `file_path`, collect
   all its comments. Process files one at a time so you don't re-read
   or re-edit the same file repeatedly.

6. **For each comment, treat the body as a discrete instruction**
   scoped to the given line range:

   - Read the **current** state of `{repo_root}/{file_path}` from
     disk. Do NOT trust `new_content` from the payload — the user
     may have edited the file after submitting.
   - The line range `[line_start..=line_end]` on the indicated `side`
     identifies *where* the comment applies. The `body` is *what* the
     user wants done.
   - Apply or address the request. "Apply" if it's an actionable
     instruction (rename, refactor, fix bug, add doc). "Address" if
     it's a question or observation (answer in prose without editing).

7. **Side `old` is informational.** A comment with `side == "old"`
   typically means "why did you remove this line?" — answer in prose
   rather than re-adding deleted code, unless the body explicitly asks
   for the line back.

8. **Summarize.** When all comments are handled, give the user a 2–4
   line summary per file: what changed, what didn't (and why). Don't
   replay every comment verbatim.

## Notes

- Don't pre-emptively read files or summarize the diff before
  launching — let the GUI be the review surface.
- Binary-file comments should not occur (the review surface refuses
  comments on binaries); if one shows up, skip with a warning.
- This skill is user-invoked only — never auto-trigger.
