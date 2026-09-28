# Layer 2 plan: the build runner

Status: draft for review, 2026-09-28 · Grilled on 2026-09-28 (nine
questions, section 11 records the answers) · Inputs: `.scratch/layer2-inputs.md`
(the 2026-09-26..28 build weekend: 17 issues, 3,868 calls, $74.51, two
releases), `docs/PRD.md` "Orchestrator", `docs/adr/0001-fast-decisions.md`,
`skills/aigentic/{brief,plan-check,run-audit}`, `.aigentic/knowledge/lessons.md`

## 0. Goal and done-when

Layer 1 wrote the build playbook down: `/brief`, `/plan-check`,
`/run-audit` and `lessons.md`, with prompts pasted by hand into fresh
threads and handoff through issue comments. Over the weekend a supervisor
(Claude Code) ran that playbook seventeen times. Its control flow was
deterministic every time; the models filled the steps. What went wrong
was mechanical: a prompt never reached its thread (#41, #47), a boilerplate
rule missing from a prompt (#40's had no force-push rule, and #40
force-pushed `main`), a report that invented plan quotes and tests (#44,
#49, #52), a commit subject noticed wrong only after the push (#47).

Layer 2 makes the harness run the playbook itself. `aigentic build <n>`
starts a lead thread whose runner is code: it renders each step's prompt
from a versioned workflow, starts the child thread, reads the child's
typed report, runs the checks the supervisor ran, pushes, and asks the
human only at the checkpoints this plan names.

Done when:

1. **Nothing is pasted.** `aigentic build <n>` takes a real issue from
   brief to closed, with every step's prompt rendered by the runner and
   every step's report on the issue, posted by the runner.
2. **The rules are in one place.** Safety line, gate, trailer and ledger
   format live in the workflow's templates; no model writes them. A rule
   learned tomorrow is one template edit.
3. **The supervisor's mechanical checks run as code**, before the push,
   and a failure goes back to the step once before anyone is asked.
4. **Paths adapt, within declared routes.** A trivial issue skips the
   planner; a trivial fix skips the second verifier; both only when the
   route's preconditions hold, and every choice is logged.
5. **Stops recover.** A cap, a budget, a saturated context or a flaky
   provider is handled by the runner's table; the human is asked only on
   the second occurrence or at the issue budget.
6. **Resume is replay.** Killing the daemon mid-step and starting it again
   resumes the run from the lead thread's log.
7. **Purpose-check earns its place.** It runs in shadow at a blind plan
   gate, is scored from the gate's answers, and makes the gate
   conditional only when the numbers in section 7 say so.

Not in layer 2: a worktree per build and parallel builds (one writing
step at a time on the shared tree stays the rule), the sandbox (phase 7),
the portfolio orchestrator (phase 8) and its model-facing tools.

## 1. Where it sits

| Crate | Gains |
| --- | --- |
| `core` | `EventKind` variants (section 9); no new dependency |
| `log` | Payload types beside the others in `payload.rs` (`StepReport`, `Handoff`, one per new kind); projections: `RunState` from a lead thread, `StepReport` from a child |
| `runtime` | `runner` module: workflow loading, template rendering, the step loop, checks, routes, budgets; the `finish_step` harness tool; a `Forge` seam over `gh` and `git push` |
| `policy` | A per-step overlay: the workflow's `deny` list applied to threads whose `thread_started.step` is set |
| `api` | `Request::Build { issue, workflow }`, `Request::AnswerCheckpoint { .. }`; `Notice::Run { .. }` for the lead thread's progress |
| `server` | Hosts runners beside thread actors; resumes unfinished runs at start-up |
| `tui` | `aigentic build <n>` (through `exec`), `/build <n>`, the checkpoint prompt |

No crate gains an edge. The runner lives in `runtime` because it needs
`log`, `tools` and `policy`, and `runtime` is the only crate allowed all
three. The `Forge` seam (issue read, comment post, push) is a trait with a
`gh`/`git` implementation and a fake for tests; GitHub goes through the
`gh` CLI, never an MCP server.

## 2. The runner is code; models fill steps

The lead thread has no model. Its events are written by the runner
(author `agent:runner`). The runner is a state machine over the workflow:
start step, await `turn_ended`, read the step's report, run checks, take a
route, repeat. Models appear in two places only:

- **Child steps** (brief, plan, plan-check, implement, verify, judge, fix,
  purpose-check), each a fresh thread on its profile.
- **Route choices**, made by the step that holds the evidence, as a typed
  field of its report: the brief's `size`, the judge's `fix`. A lead model
  would know less than the step it reads, and would re-read its own
  context on every poll.

A route is taken only if its preconditions hold (section 6.3); otherwise
the runner takes the longer route and logs why. It never falls back to a
shorter one. A situation no declared route covers is `route: escalate`,
which asks the human. `start_thread`, `await_thread` and `thread_report`
are runtime functions here, not model tools; as tools they belong to the
phase 8 orchestrator.

## 3. The workflow folder

A workflow is data, resolved like skills (project, then user, then
bundled) but never in a model's context and never offered as
`load_skill`:

```
.aigentic/workflows/build/
  workflow.toml
  templates/brief.md
  templates/planner.md
  templates/plan-check.md
  templates/implementer.md
  templates/verifier.md
  templates/judge.md
  templates/fix.md
  templates/purpose-check.md
```

`workflow.toml` (shape, not final):

```toml
name = "build"
version = 1

[budget]            # per issue, in USD; the brief picks by size
trivial = 3.0
full = 10.0
max_raise = 2.0     # the brief may raise up to 2x with a reason; above, ask

[[steps]]
id = "brief"
role = "brief"
profile = "kimi"
template = "templates/brief.md"
marker = "## Brief"
budget = 1.5
wall_secs = 900
route_by = "size"
routes = { trivial = "implement-alone", full = "plan" }

[[steps]]
id = "implement-alone"
role = "implementer"
profile = "flash"
template = "templates/implementer.md"
marker = "## Implementation"
writes = true                       # one writing step at a time per repo
checks = ["E1", "E2", "E3", "E4", "E5", "E7"]
push = true                         # the runner pushes after checks pass
deny = ["git push", "git rebase", "git reset", "git checkout --", "git stash", "git clean", "git add -A", "copy .env"]
next = "done"

[[steps]]
id = "judge"
role = "judge"
profile = "kimi"
template = "templates/judge.md"
marker = "## Review"
route_by = "fix"
routes = { none = "done", trivial = "fix-alone", full = "fix-full", escalate = "ask" }

[routes.fix-alone]
steps = ["fix", "judge-2"]
requires = { max_diff_lines = 20, files_within_reviewed_diff = true, gate_green = true }
```

Templates carry every fixed rule verbatim: role header, the safety line,
the gate (with `timeout_secs: 900`, run once, log searched), the trailer
(the runner fills `<model>` from the profile's `model` without the
provider prefix), the ledger format, "the amendment wins", the pty `.env`
rule. They take named slots only, as `{{name}}`, plus `{{#name}}…{{/name}}`
sections kept when the slot is set and non-empty (no nesting, no loops):

| Slot | Filled by | Used by |
| --- | --- | --- |
| `issue`, `title` | runner | all |
| `model` | runner, from the profile | writing steps |
| `gate_log` | runner, `<temp dir>/aigentic-gate-<issue>.log` | writing steps, checks E4 and E5 |
| `budget_trivial`, `budget_full`, `max_raise` | runner, from `[budget]` | brief |
| `pointers` (file, line, what it holds) | brief | planner, implementer |
| `purpose`, `must_not_undo` | brief | planner, plan-check, purpose-check, judge |
| `size`, `budgets` | brief | runner |
| `design`, `planned_tests` | brief when trivial, planner otherwise | implementer |
| `commits` (named messages) | brief when trivial, planner otherwise | implementer, check E2 |
| `reference_check` | brief or planner | implementer, verifier |
| `ui` (pty check needed) | brief | implementer, verifier |
| `amendments` | plan-check, the plan gate | implementer |
| `check_failures` | runner | a step sent back once (section 6) |
| `handoff` | runner | a continuation thread (section 8) |

The brief fills its slots through `finish_step` like every other step; the
runner posts them as the `## Brief` comment. A workflow's content hash is
recorded in `run_started`, so a run replays against the workflow it began
with.

## 4. Child threads and `finish_step`

- **Start.** The runner creates the child in-process with
  `thread_started.parent_thread` and `.step` set, posts the rendered
  prompt as the first `user_message` (author `agent:runner`), and
  confirms the event is in the child's log before awaiting. A prompt
  that never arrives is impossible by construction.
- **Await.** The runner waits for the child's `turn_ended` and applies
  section 8's table to its reason.
- **Report.** A child ends by calling `finish_step`, a harness tool
  offered only to threads with a `step`. It writes a `step_reported`
  event with typed fields:

| Field | Steps |
| --- | --- |
| `status`: `done` \| `partial` | all |
| `body` (prose, posted under the marker) | all |
| `slots` | brief, planner |
| `planned_tests` (T-id, what, how the expected value is derived) | planner |
| `commits` (sha, subject) | writing steps |
| `ledger` (T-id, name in code, `landed` \| `not_landed`, reason) | implementer, fix |
| `quotes` (phrases the report attributes to the plan) | implementer, fix, verifier |
| `verdict`: `approve` \| `changes_needed`, `fix` | judge |
| `release_impact` | judge; the implementer when it works alone |
| `findings` (id, text) | plan-check, purpose-check |
| `route`: `escalate` + reason | any |
| `handoff` (done, next, dirty files) | when `partial` |

- **Issue comments.** Children do not post to the issue. The runner posts
  `<marker>\n\n<body>` plus a check summary, so the issue stays the human
  handoff and the log is the machine one. Reports stay claims; section 6
  checks them.
- **Missing report.** A turn that ends `done` without `finish_step` is a
  recoverable stop: the runner posts "call finish_step" once, then
  escalates.
- **End.** The runner closes each child when its step ends.

## 5. Human checkpoints

**Always ask:** publishing a release (the runner proposes the version from
the highest `release_impact` since the last tag), `route: escalate`, the
second cap on the same step, the second `changes_needed`, a policy denial
the step cannot route around, the issue budget at 80% and 100% (section 8),
a brief raising the budget above `max_raise`.

**The plan gate** (after plan and plan-check, while purpose-check is in
shadow), in two stages so the answer stays an independent label:

1. **Blind.** Shown: the plan's link, plan-check's result, cost so far
   against the budget. Answers: `go`, `amend <text>` (posted as
   `## Plan amendment` and passed to the implementer), `stop`.
2. **Reveal.** Then purpose-check's findings; the human marks each
   `useful` or `noise`, and tags each purpose point of their own
   amendment that it did not raise as `missed`.

**Never ask:** sequencing, continue-once after a cap, probe-and-continue,
routes whose preconditions pass, closing the issue on `approve` with
green checks, the runner's push.

A checkpoint ends the lead thread's turn with `asked_human`; the person
answers whenever they get to it, from the TUI or `exec`. With nobody
there, `exec`'s default answer is `stop`, never `go`.

## 6. Checks and the push

### 6.1 Exact checks (code, no model; a failure blocks)

Sources: git, the child's log (every bash call and its exit status), the
`step_reported` fields, the diff.

| Id | Check | Weekend case |
| --- | --- | --- |
| E1 | Each commit's trailer parses (blank line before it) and names the profile's model | a build credited Claude for GLM's work |
| E2 | Commit subjects equal the named messages | #47 |
| E3 | Nothing under `.scratch/`; `.aigentic/rules.toml` untouched | rule |
| E4 | After the last edit: `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, full `cargo test`, all exit 0 | every issue |
| E5 | No full-suite re-run without an edit in between | #40, five runs |
| E6 | Every planned T-id is in the ledger; each `landed` name matches exactly one added `fn` in the diff | #44, 7 of 14 matched nothing |
| E7 | Each named test run reported at least `1 passed` | #34, `running 0 tests` |
| E8 | Each `quotes` phrase occurs verbatim in the plan or an amendment | #49 |

### 6.2 Flags (code finds, the judge weighs; never block)

- **F1** Numbers in the report body found in neither the diff nor the
  plan (#52's 48,000 against a shipped 32,000). Heuristic; false
  positives expected.
- **F2** Files changed outside the plan's named files.

Flags are rendered into the judge's prompt and appended to the issue
comment.

### 6.3 Route preconditions

Checked by code before a route is taken, for example `fix-alone`: diff
under `max_diff_lines`, only files already in the reviewed diff, gate
green. A failed precondition takes the longer route and records why in
`route_taken`.

### 6.4 The runner pushes

1. The writing step commits and calls `finish_step`; its policy overlay
   denies `git push`, rebase, the destructive tree commands and copying
   `.env`. Amend stays allowed: a step's commits are unpushed until the
   runner pushes them, and the runner's plain push is rejected if a
   pushed commit was rewritten, which it then escalates.
2. The runner runs the step's exact checks.
3. On failure it posts the failures into the same thread once ("fix
   these; the commits are unpushed, so amending is fine"), then
   escalates.
4. On success it pushes once, runs `cargo install --path crates/tui
   --force`, confirms the installed binary's commit equals HEAD, and
   records `pushed`.

One push and no force-push hold by construction.

### 6.5 Judgment

Purpose (purpose-check, section 7), whether a dropped test's reason
holds and whether a corrected literal is right (the judge) stay with
models. The supervisor's own errors (a false accusation on #47, a
recommendation on #35 that contradicted its amendment) are why the
checks read evidence first and why the reference in section 7 is scored
too.

## 7. Purpose-check in shadow

A `purpose-check` skill (to write, from the supervisor's weekend catches:
#40, #41, #47, #35, #51, #52, #19) runs as a child step after plan-check.
While in shadow its findings are shown only at the gate's reveal stage.

**Scoring**, computed from `checkpoint_answered` events and replayable by
`aigentic eval purpose`:

- **Recall**: of the human's purpose points, the share purpose-check
  raised.
- **Precision**: `useful` over all its findings.
- **Late misses**: a purpose problem the judge finds after a `go` counts
  against both purpose-check and the human.

**Handover**: over the last 10 gated plans, zero `missed`, at least 3
real catches in the window, precision at least 50%. Handover makes the
gate conditional: the runner asks only when purpose-check raises a
concern. One late miss while live returns it to shadow. The numbers are
set from one weekend's volume and are revisited after the first ten
gates.

## 8. Budgets and recoverable stops

Budgets are in USD (what `stats` measures) and wall time. The per-turn
caps in each profile (`max_tokens`, `max_iterations`, `max_wall_time`)
stay as the safety net. Each step has a default budget in the workflow;
the brief sets the issue budget from its size (trivial $3, full $10, up
to `max_raise` with a reason). A fix round draws on the issue budget.

| Trigger | Runner | Then |
| --- | --- | --- |
| Step at 80% | Posts a steer into the child: finish the unit, commit green work, `finish_step` with `status: partial` and a handoff | — |
| Step at 100% | Interrupts; takes the handoff, or builds one from git and the log | A fresh thread continues from it, once; the second time asks |
| Per-turn cap | `continue` in the same thread (warm cache), once | Second time asks |
| `context_saturated` | Steers the child to hand off at the next commit | Fresh thread from the handoff |
| Provider transport or 5xx | `doctor --probe`, back off, `continue`, up to 3 times | Then asks |
| Turn `done` without `finish_step` | Posts "call finish_step" once | Then escalates |
| Issue at 80% | Lets the current step finish | Asks before the next step |
| Issue at 100% | Starts no new step | Asks |
| Tool calls several times slower than their usual (watchdog) | Flags with the reason (disk, load, sleep) | Slice 5 |

A budget stop continues in a fresh thread and a per-turn cap in the same
one on purpose: a per-turn cap hits mid-task with a warm cache (run-audit's
rule), a budget stop means the context has grown (#35, "one task per
thread").

## 9. Events

The runner's state is a projection of the lead thread's log, and every
decision is appended before it is acted on, so replay after a restart
lands where the run was. Added, never changed.

**Lead thread** (author `agent:runner`):

| Kind | Payload |
| --- | --- |
| `run_started` | issue, workflow name, version, content hash, provisional issue budget (the workflow's full budget; the brief has not sized the issue yet) |
| `step_started` | step id, role, profile, child thread id, attempt, step budget; every re-entry (send-back, continue, fresh thread) is a new one with attempt + 1 |
| `step_finished` | step id, `done` \| `partial` \| `failed`, end reason, cost, child's `step_reported` event id |
| `checks_run` | step id, per check: `pass` \| `flag` \| `fail`, detail |
| `route_taken` | branch point, proposed, preconditions with results, taken, fallback reason, issue budget when the route sets it (the brief's) |
| `checkpoint_asked` | gate kind, what was shown, options |
| `checkpoint_answered` | answer, amendment text, reveal marks (`useful`, `noise`, `missed`) |
| `budget_warned` | scope (`step` \| `issue`), spent, limit |
| `pushed` | commits, remote ref before and after, installed binary's commit; appended after the push and install, which are idempotent |
| `run_finished` | `closed` \| `stopped` \| `escalated`, cost, release impact |

**Replay.** The last lead-thread event decides the runner's next move: `run_started` → start the first step; `step_started` → await that child; `step_finished` → run the step's checks; `checks_run` → send back, push or route by the outcome; `pushed` → install, close or route on; `route_taken` → follow it (never re-derive: preconditions read git state that can change across a crash); `checkpoint_asked` → wait; `checkpoint_answered` → act on the answer; `budget_warned` → the move of the event before it; `run_finished` → nothing. Per-step state (checks, pushed commits, step warnings) is kept per attempt.

**Child thread:** `step_reported`, written by `finish_step` (the tool's
effect is its own kind, as `remember` writes `memory_remembered`).

**Changed payload:** `thread_started` gains `parent_thread` and `step`,
both `#[serde(default)]`.

`decision_made` (ADR 0001) is not reused: purpose-check is a step and a
route choice is `route_taken`. When the `route` Decider site goes into
shadow it logs `decision_made` beside `route_taken`, which is its label.

## 10. Slices, one ticket each

Each slice is accepted on a real issue, with the supervisor running its
usual post-check blind to `checks_run`; anything the supervisor catches
that the runner missed is a bug in the slice, anything the runner catches
that the supervisor missed goes into `lessons.md`.

1. **Skeleton: brief → implement-alone.** `aigentic build <n>` and
   `/build`; the workflow folder, loader and template renderer with the
   brief and implementer templates; `finish_step` and `step_reported`;
   the runner posting comments through `Forge`; checks E1–E5 and E7; the
   runner's push and install, and closing the issue after an
   implement-alone push with the report's `release_impact`; the step
   policy overlay; the lead-thread
   events from section 9 that this path uses; replay on daemon start;
   per-turn cap continue-once and the missing-report stop. *Accepted
   when* one real trivial issue goes from `aigentic build` to pushed with
   nothing pasted, the checks show in the lead thread, and a daemon
   killed during implement resumes on restart.
2. **Full path.** Planner, plan-check, the blind plan gate (stage 1
   only), implementer, verifier, judge; checks E6, E8 and flags F1, F2;
   routes `approve`, `fix-alone`, `fix-full`, `escalate` with
   preconditions; closing the issue. *Accepted on* one real full issue.
3. **Budgets and stops.** Step and issue budgets, the 80% steer,
   `partial` and handoff, fresh-thread continuation, probe-and-continue,
   the issue-level asks. *Accepted when* a step forced over a small
   budget hands off and a fresh thread finishes it.
4. **Purpose-check in shadow.** The skill, the gate's reveal stage and
   marks, `aigentic eval purpose`. *Accepted when* the first gated issue
   produces a scored row.
5. **Releases and the watchdog.** The `release_impact` fold and the
   publish checkpoint; the slow-tool watchdog.

## 11. Decisions

1. **Runner as code, not a lead model** (Q1). Control flow was
   deterministic all 17 times and its failures were mechanical; a lead
   model adds cost per poll and knows less than the step it reads. Paths
   still adapt: the step holding the evidence picks among declared
   routes, and code checks the route's preconditions.
2. **Templates in the workflow, the brief fills slots** (Q2). Across 20
   implementer prompts the trailer line had 6 variants and the gate line
   21; #40's lacked the force-push rule. The workflow lives in
   `.aigentic/workflows/`, not `skills/`: it is config for code, and a
   skill would put it in every model's context.
3. **Children report through `finish_step`; the runner posts to the
   issue** (Q3). Typed fields for the checks, the log as the machine
   handoff, the issue as the human one.
4. **The plan gate stays while purpose-check is in shadow** (Q4); never
   ask about sequencing or passing routes; `exec`'s default is `stop`.
5. **The runner pushes** (Q5). E1–E3 are only cheap to fix before a
   push; exact failures go back once, then escalate; flags go to the
   judge.
6. **Blind gate, scored per finding, conditional after handover** (Q6).
7. **Budgets in USD and wall time; fresh thread after a budget stop,
   same thread after a per-turn cap** (Q7).
8. **Ten lead-thread kinds and `step_reported`; write-ahead; no generic
   `run_event`** (Q8).
9. **Slice 1 is the brief → implement-alone skeleton** (Q9). The claim
   guard moves to slice 2: every fabrication was on the full path, and
   with no plan E6 and E8 check nothing.

## 12. Open items

- **The runner's own profile.** None: the lead thread has no model. The
  `/build` checkpoint rendering needs a TUI cell; its shape is decided in
  slice 1.
- **E4 and E5 read bash calls from the log.** A gate run as `cargo test
  … | tee` or split across calls has to be recognised; slice 1 fixes the
  accepted forms in the implementer template so the check can be strict.
- **F1's heuristic** (which numbers count) is tuned against #52's replay
  in slice 2.
- **Planner effort.** Kimi at `high` costs $1–3 even on small issues;
  whether `size` should pick the planner's effort is measured once slice
  2 runs, not decided here.
- **Worktrees.** A worktree per run lifts the one-writing-step rule and
  enables parallel builds; it follows layer 2.
- **`lessons.md` and the skills.** Once a rule is in a template, the
  lesson keeps its story and the `brief` skill stops restating it; the
  skill shrinks to slot filling in slice 1.
