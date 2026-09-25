# Lessons from building aigentic with aigentic

Durable lessons from real runs (2026-09-23 onwards). Each says what happened,
so the rule can be judged, not just obeyed. The `brief`, `plan-check` and
`run-audit` skills build on this file.

## How work is split

- **Plan → implement → review** for anything larger than a one-line fix:
  a planner (profile `kimi`) posts `## Plan` on the issue, an implementer
  (profile `flash`) follows it, a reviewer (`kimi`) posts `## Review` and
  closes the issue. Issue comments are the handoff; every step starts in a
  fresh thread. Trivial fixes: implementer alone.
- **Cost lives in the loop, not the planner.** The model that runs the loop
  pays for ~95% of calls, mostly re-reading its cached context. Put the
  cheap model on the many calls, the strong one on the few. On #38/#39 the
  implement step cost $0.20–0.41 on Flash; the same tokens on GLM cost 3×.
- **Review is split: verify on Flash, judge on Kimi.** The #43 review ran
  tests and pty checks on Kimi for $6.86, 39% of the cycle. A verifier
  (flash) posts `## Verification` with outputs and dumps; a judge (kimi)
  reads evidence and diff and gives the verdict in a handful of calls.
- **Every planned test is accounted for.** Flash dropped planned tests
  silently on #37 and #43. The implementer's `## Implementation` carries a
  ledger (T1, T2, … landed or not, with a reason); a planned test missing
  without a reason is `changes needed`.
- **Someone reads the plan before implementing.** Both trial plans had a
  flaw a second reader caught (#38: pipe filters counted as extra work,
  undoing #21; #31: retries collected and emitted after the fact, defeating
  live display). Corrections go in a separate `## Plan amendment` comment;
  the plan's text stays as written.

## Plans and tests

- **Check the design against the issue's purpose, not only its words.**
  "Show retries" means while it waits; "show the main command" means the
  work, not its output filters.
- **No hand-computed literals.** Expected values in tests come from the
  renderer, the code or the issue's rule (fixtures with known totals).
  Planners get wrapping and quoting wrong, and so do reviewers: on #39 a
  reviewer's "correction" of a wrap literal was itself wrong.
- **An implementer that disagrees with a plan literal** checks which side
  matches the issue's rule, fixes that side, and says so. It never changes
  an assertion just to make it pass.
- **The implementer posts its report on the issue** as `## Implementation`
  (changes per commit, tests, corrected plan literals, pty dumps). On #43
  the reviewer searched the disk for the implementer's thread to find its
  dumps: everything a later step needs goes on the issue.
- **Visual changes need a pty check** (`~/.local/share/aigentic/devtools/drive.py`,
  a venv with `pyte`); the implementer pastes dumps, and the reviewer
  repeats the check for UI work. Code-only work gets a lean review.

## Git and GitHub

- Commit with `git commit -F <file>`, never a heredoc (a heredoc once left
  `EOF )` in a pushed message), a blank line before the trailer.
- Stage files by name. Never `git add -A`, `git reset`, `git checkout --
  <file>`, `git stash` or `git clean` in a build thread; the tree may hold
  someone else's unfinished work.
- The trailer names the model that wrote the code:
  `Co-Authored-By: aigentic (<model>) <332865255+aigentic-bot@users.noreply.github.com>`.
  Never copy another trailer from history (a build once credited Claude
  for GLM's work).
- Closing comments state files changed, tests added, and what the issue got
  wrong (or "nothing"). Ask for it as a done-when item; asked for loosely,
  models write one line.
- Commit nothing under `.scratch/`; leave `.aigentic/rules.toml` alone.
- `gh issue view <n> --comments` prints nothing without a terminal (as in
  a bash tool call); use `gh issue view <n> --json body,comments`.

## Running builds

- The full test suite needs `timeout_secs: 600`; the repo default is 600.
- **One task per thread.** A new commit-sized task starts a fresh thread;
  resume the same thread only to finish what it was doing.
- A turn that stops on `max_tokens`, `max_iterations` or `max_wall_time`
  hit a cap, not a bug: resume the same thread with `continue`. Caps per
  turn: tensorx ~$9, flash ~$2.5, kimi ~$4.5.
- `provider_error: transport …` is the provider: probe it
  (`aigentic doctor --probe`), then `continue`. TensorX once hung 9.5
  minutes; retries were invisible until #31.
- A laptop that sleeps pauses tool timers: a `cargo test` "took" 37 minutes.
- A running session keeps the binary it started with: a fix installed
  mid-run only applies to sessions started afterwards.
- A new key in `aigentic.toml` makes older binaries refuse the repo until
  #37 lands: add the key in the same commit that teaches the binary.
- A message typed while a turn runs reaches the model at its next step
  (#33); use it to steer instead of interrupting.

## Memory and context

- Memory extraction reads only sentences with a durable cue ("for the
  record", "we decided", "going forward", …); long task prompts are skipped.
  When a prompt itself talks about memory cues, turn memory off for that
  session first.
- Context is bounded by eviction (sweep above 64k, ceiling 128k) and by
  dropping old reasoning. A healthy build call is ~40–70k context and
  almost all cached; many cold calls in a row, or context past ~90k, means
  something is wrong.
