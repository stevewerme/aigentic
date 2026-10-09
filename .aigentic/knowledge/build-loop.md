# The build loop

How a ticket goes from filed to closed, as run in October 2026. Until the
pipeline job lands, a person (or the front thread) acts as **supervisor**
and runs each step in a fresh thread. This file is the supervisor's
procedure; `lessons.md` holds the reasons behind it.

## The steps

Every step is its own thread, started fresh (`/new`), with one prompt.
Steps hand off through the issue's comments, under fixed headings. All
steps run on the `flash` profile.

1. **Size**: map the code, then split if needed (supervisor).
2. **`## Spec`**: written by the supervisor and posted on the issue.
3. **`## Spec check`**: one checker thread.
4. **`## Spec v2 (consolidated)`**: one merger thread. Its prompt carries the
   supervisor's binding decisions.
5. **`## Implementation`**: one implementer thread, which commits and pushes.
6. **`## Review`**: one judge thread. On `approve` it closes the issue.
7. **`## Fix`**: when the judge says `changes needed`, one lean fixer thread.
8. **Close**: the supervisor reviews the fix, waits for green CI on the fix
   commit, and closes with a comment that states the release impact.

A trivial change (a one-line fix, a docs edit) skips 3, 4 and 6.

## 1. Size

- Map before you spec. A read-only pass lists, with `file:line`, what the
  ticket touches, what calls it, the tests that pin it, and its hazards.
- **One crate and one concern per ticket.** Split anything bigger
  *before* the spec, and ask the person to approve the split. File the
  parts, assign them, and note the split on the parent issue.
- A ticket is "reserved" by assigning it. Unassigned with
  `ready-for-agent` means open to outside contributors (`CONTRIBUTING.md`).
- A product decision (a boundary, a default, a behaviour change a person
  will notice) goes to the person before the spec, as one question with a
  recommendation.

## 2. The spec

Post it as `## Spec`, and keep a copy at `.scratch/<n>-spec.md`. The
sections are always:

- **Purpose**: the person's words and the binding plan text it serves.
  Name any choice that departs from the plan's or the issue's letter, and
  why.
- **What is there today**: every fact the implementer needs, with
  `file:line`. This is what spares the implementer from reading big files.
- **Design**: numbered, concrete. State boundaries (what is read, what
  is refused) and what stays unchanged.
- **Tests**: T1, T2, …, each saying what it proves. Expectations are
  derived from fixtures and shared constants, never hand-typed.
- **Not in this ticket**: name the follow-up tickets.
- **Commits**: exact subjects.
- **Release impact**: none, patch, minor or breaking, with the reason.

Rules learned the hard way:
- Never ask for "a doc comment that says so". Comments follow `AGENTS.md`'s
  "Comments" section; reasoning goes in the commit message and
  `## Implementation`.
- A "byte-identical" claim must cover what the model receives: the
  system messages, the tool list **and each tool's schema**. New optional
  arguments change a schema.
- Say what happens with nothing configured (no root, no workspace, no
  file). Fail closed for anything that reads or writes outside a project.
- Say when state is loaded (build, switch, turn start) and how a change is
  detected (content or mtime).

## 3. The spec check

One checker, capped at 50–70 calls. Its prompt names the files to read and
asks numbered questions: every fact against the code, escapes, byte
identity, the tests, and scope. It posts `## Spec check` as a numbered
list, or `spec ok`. Checkers sometimes cite text the spec doesn't contain,
or a wrong line.

## 4. The merge

One merger, capped at 40 calls. Its prompt holds the supervisor's
**decisions**, one per check item that left a choice. The merger:
- opens each item's cited `file:line` and rejects an item whose fact is
  wrong, saying so in the opening paragraph;
- rewrites the whole spec with every valid item merged in place (never a
  stack of amendments: an implementer given amendments loops);
- greps for each decision's key term before posting.

The supervisor then checks the posted v2 matches its file, that every
decision landed, and every rejection's reason.

## 5. The implementer

The prompt says to read only spec v2, once, and to open other files by
line ranges. The standard blocks:
- never touch `~/.local/share/aigentic/` or `~/.config/aigentic/`, never
  read `.env`, never run the installed `aigentic`; temp dirs for every
  test;
- one commit per spec subject, `git commit -F <file>`, files staged by
  name, the trailer naming the model;
- **existing tests:** never change an assertion; the only allowed edits are
  the ones the spec lists. If anything else fails, stop and report, or ask
  the person with `ask_human`;
- the gate: three bare calls (`cargo fmt`, `cargo clippy --all-targets --
  -D warnings`, `cargo test --no-fail-fast > /tmp/<n>-gate.log 2>&1` with
  `timeout_secs: 900`), then a separate `grep`;
- #103: if the full run stalls once, run per crate. #119: a known flake,
  rerun it alone once;
- a test that runs past about 2 minutes is a finding, not a stall;
- a red test is "pre-existing" only once proven at the base, built in its
  own target dir;
- push with a plain `git push`. **If it fails, stop and report**: never
  reroute ssh, change ports or accept host keys;
- the report: what changed, choices left open, edited tests with lines, the
  gate's results, and a ledger of every planned test with its exact name.

## 6. The judge

Capped at 80–120 calls. The judge **proves by running**, not by reading:
- builds probes and copies outside the repository, **each tree in its own
  `CARGO_TARGET_DIR`** (one target dir shared across trees runs stale
  binaries), and deletes them all before posting;
- for any "unchanged" claim, builds the base and the head and compares the
  real output (requests, prompts, rows) byte for byte;
- for a boundary, tries to break it (paths, symlinks, `..`, modes, grants)
  and tables the attempts;
- runs three mutations, each of which must fail a test;
- checks the ledger names exist (a misspelled `cargo test <name>` passes
  with 0 tests), the edited tests, and new comments against `AGENTS.md`;
- treats the report's claims as claims.

It posts `## Review` with `approve` or `changes needed` (numbered, each
item saying read or proven by running), then `Release impact: …`. On
`approve` it closes the issue.

## 7. The fix

A fix prompt is scoped to the review's items, capped at 30–90 calls, with
the same standard blocks. It posts `## Fix`. The supervisor reads the
diff for the risky part, waits for CI on the **full** SHA of the fix commit
(`gh run list --commit <short sha>` matches nothing), then closes with a
comment: what each item became, the costs, the release impact.

## Checking a step

After every step:
- `aigentic status --all`: did the thread end (`done`), and at what cost?
  A thread still `live` isn't finished.
- The thread has exactly **one** `user_message`. Two means a prompt landed
  in a used session.
- If a report says someone approved something, find the `ask_human` call
  and its answer in the thread's log.
- The issue's last comment is the step's heading. No comment and no thread
  means the prompt never ran.
- CI is green on the pushed commit.

## Costs

Typical Flash costs: spec check $0.50–1.30, merge $0.12–0.40, implementer
$3–10, judge $1.30–4, fix $0.30–3.50. Ask before a ticket passes $10. Cost
is volume (calls × cached context), not prompt size.

## Prompts

Prompts are files. Each one is derived from the last of its role
(`<n>-1-speccheck`, `<n>-merge`, `<n>-2-implementer`, `<n>-3-judge`,
`<n>-4-fix`), so a lesson added to one carries forward. Supervisor
extras go into the same file, never a second paste.
