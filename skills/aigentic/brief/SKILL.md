---
name: brief
description: Turn an issue into the prompts for its build (planner, implementer, reviewer, or implementer alone for a trivial fix), following this project's playbook and lessons.
disable-model-invocation: true
---

# Brief

Write the prompts that build one issue. The person pastes them into fresh
threads, or a lead thread starts them. You write prompts; you do not edit
code.

Read first: the issue with its comments
(`gh issue view <n> --json body,comments`),
`.aigentic/knowledge/lessons.md`, and `docs/agents/issue-tracker.md`.

## 1. Size the work

- **Trivial** (one function, an obvious fix, about 30 lines, no design
  choice): implementer alone.
- **Everything else**: planner → implementer → verifier → judge.
- **Visual or terminal UI changes**: the verifier runs the pty check.
  The judge reads evidence; it re-runs a check only when the evidence is
  thin or contradicts the diff.
- **Several commits**: the planner splits them and names each message.

## 2. Find the code

Grep for the symbols the issue names and follow them one hop. Note each
file with an approximate line and what it holds. Put these pointers in the
prompts so no role has to rediscover them. Search; do not read whole files.

## 3. Note the purpose

Write down in one sentence what the issue is *for*, and the recent
decisions it must not undo (`git log --oneline -15`, closed issues touching
the same code). The planner prompt states both, so the plan is checked
against intent, not only wording.

## 4. Write the prompts

Give each role two separate fenced blocks: first the shell command that
opens its thread (`aigentic --profile kimi --mode auto` for plan and
judge, `aigentic --profile flash --mode auto` to implement and verify, from the
repo root), then the prompt itself. Never put the command inside the
prompt block. When asked, also save each prompt to
`.scratch/prompts/<issue>-<n>-<role>.txt` so it can be copied with
`pbcopy < <file>` (copying from the terminal brings its decorations).

**Planner** (profile `kimi`):
- "You are the planner for #N. Do not edit any file."
- The code pointers from step 2 and the purpose from step 3.
- What the plan must contain: the cause as found in the code, exact
  functions and types to change and how, each test with how its expected
  value is derived (from the code or the rule, never by hand), the gate,
  the commit message(s), and a reference check the verifier can run when
  the result is measurable. Number the planned tests (T1, T2, …) so the
  implementer's ledger can refer to them.
- Post one comment headed `## Plan` with `gh issue comment N --body-file <file>`;
  a length cap (60–90 lines); reply with the comment's URL.

**Implementer** (profile `flash`):
- "Read the `## Plan` comment and any `## Plan amendment` after it; where
  they differ, the amendment wins. If a planned literal disagrees with what
  the code produces, check which matches the issue's rule, fix the side
  that is wrong, and say so. Never change an assertion just to make it
  pass. If a step is impossible as written, say so instead of improvising."
- Gate: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test` with `timeout_secs: 600`. For UI work, the pty check, with
  dumps pasted in the final message.
- Commit per the plan with `git commit -F <file>`, files staged by name, a
  blank line, then exactly
  `Co-Authored-By: aigentic (<model>) <332865255+aigentic-bot@users.noreply.github.com>`,
  with `<model>` filled in by you from the implementer's profile (the
  `model` in its `[profiles.<name>]`, without the provider prefix: `flash`
  is `deepseek-v4.1-flash`). A model may not know its own id.
  Push, then `cargo install --path crates/tui --force` (every implementer
  prompt says both). Do not close the issue.
- Last step: post the final report as one comment headed
  `## Implementation` (`gh issue comment N --body-file <file>`): what
  changed per commit, any plan literal corrected and why, and a **test
  ledger**: every planned test (T1, T2, …) with its name in the code and
  `landed`, or `not landed` with the reason. A planned test may be dropped
  only with a stated reason. Later steps read the issue, not this thread.
- The safety line: never `git reset`, `git checkout -- <file>`,
  `git stash`, `git clean` or `git add -A`; commit nothing under
  `.scratch/`; leave `.aigentic/rules.toml` alone.

**Verifier** (profile `flash`): the mechanical half of the review, on the
cheap model.
- "Do not edit any file." Read the plan, amendments and `## Implementation`.
- Run the gate on the touched crates (`timeout_secs: 600`), each planned
  test by name, the reference check, and for UI work the pty check.
- Check the ledger against the code: every planned test exists under the
  name the ledger gives, or has a stated reason.
- Post one comment headed `## Verification` with the command outputs,
  dumps and ledger check, and no verdict.

**Judge** (profile `kimi`): the judgment half, a handful of calls.
- "Do not edit any file." Read the issue, plan, amendments,
  `## Implementation`, `## Verification`, and each commit (`git show`).
- Judge: does the change do what the issue is *for* and what the plan
  asks; is anything missing or wrong (file and line); do the evidence and
  the diff agree. Re-run a check only when the evidence is thin or
  contradicts the diff.
- A planned test missing without a stated reason is `changes needed`.
- Post one comment headed `## Review` with a verdict (`approve`, or
  `changes needed` with a numbered list). On approve, post the closing
  comment (files changed, tests added, what the issue got wrong or
  "nothing") and close the issue.

**Implementer alone** (trivial): the implementer prompt with the design
written into it, plus the closing comment as a done-when item: files
changed, tests added, what the issue got wrong or "nothing", then close.

## 5. Hand over

Give the prompts in order as fenced blocks, one per thread, and say which
step the person should pause after (after the plan, so it can be checked
with `/plan-check`). On "changes needed", the fix goes back to the
implementer (a short prompt listing the numbered items), then verify and
judge again.
