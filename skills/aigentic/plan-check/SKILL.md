---
name: plan-check
description: Check a posted `## Plan` against its issue and the code before anyone implements it, and post a `## Plan amendment` when something is wrong.
disable-model-invocation: true
---

# Plan check

A second reader for a plan, between the planner and the implementer. Both
early trial plans had a flaw only a second reader caught. Do not edit code.

Read: the issue and all comments (`gh issue view <n> --comments`), the
latest `## Plan`, `.aigentic/knowledge/lessons.md`, and the code at the
lines the plan names. Open the real functions; do not trust the plan's
description of them.

## Checklist

1. **Purpose.** Does the design achieve what the issue is *for*, not only
   what it says? Examples: retries must be visible *while* waiting, so the
   events are emitted as they happen, not collected and sent after (#31);
   the "main command" of a line is its work, so output filters after a `|`
   are not extra work (#38).
2. **Regressions.** Does it undo a recent decision? Check
   `git log --oneline -20 -- <files>` and closed issues touching the same
   code. A plan that changes behaviour a recent issue set on purpose needs
   an explicit reason.
3. **Literals.** Every expected value in a test must come from the code,
   the renderer or the issue's rule. Flag hand-computed ones and say how to
   derive them. Do not "correct" a literal you have not produced by running
   the code; your hand computation is as fallible as the planner's.
4. **Compatibility.** Event kinds are added, never changed; new payload
   fields are `#[serde(default)]`; old logs must still replay. A new config
   key breaks older binaries until #37: the key and the code that reads it
   land together.
5. **Gate and scope.** `cargo clippy --all-targets -- -D warnings`,
   `cargo test` with `timeout_secs: 600`. Files outside the issue's scope
   need a reason.
6. **Order of operations.** Before/after checks run in the right order
   (a "before" measurement happens before any edit).

## Output

- Nothing wrong: reply `plan ok` and one line per checklist item you
  verified.
- Something wrong: post one comment headed `## Plan amendment` with
  numbered corrections, each saying what to change, why, and the expected
  result. Start with "where they differ, this wins". Leave the plan's own
  comment untouched. Reply with the comment's URL and a two-line summary.
