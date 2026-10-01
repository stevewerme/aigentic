# 0002: The conversation is the interface; structure is the harness's job

Status: proposed, 2026-10-01 (Steve)

## Context

Using aigentic today means carrying its structure in your head:

- **Which thread.** A thread per task, resumed by id. Builds add a lead and a child thread per step.
- **Which project.** Chosen at start, or switched by `/project use`.
- **Which model.** A `--profile` per session, and a profile per workflow step.
- **The context window.** Watched, compacted, restarted.
- **Where things go.** An issue, a comment, memory, knowledge, `lessons.md`, the spec, or a scratch file.
- **Who runs what.** A supervisor writes prompt files, a person pastes them into fresh threads, and the supervisor checks each step's log.

Steve's aim (2026-10-01): none of that is a person's job. The person carries **intent**; the harness carries **structure**. There is one intelligent, centred conversation, and the models file everything where it belongs. At first a human confirms. Over time the harness learns the operator's structure and pulse of work, and adapts.

Pieces of this already exist or are planned:
- **Phase 6** (`docs/PLAN-phase6.md`): a thread belongs to the person, and the project is the context the thread is in, proposed by the model and confirmed by the person (§9). The working set aims at "a person never manages the context window" (§13). The phase's acceptance counts the asks a day (#12).
- **ADR 0001:** small, closed questions are `Decider` sites. Each starts in shadow, is judged against labels from the log (`decision_made`), and goes live only when an evaluation says so.
- **Layer 2** (`docs/PLAN-layer2.md`): a build's sequencing, checks and push are code, and a person is asked only at named checkpoints.
- **The PRD's orchestrator:** an agent that coordinates work across projects.

What's missing is the principle that joins them, and a rule for **when a decision moves from asking to acting**.

## Decision

**1. One conversation in front, any number of threads behind.** The person talks in one thread. They never have to choose a thread, a project or a model, or manage a context window. Behind the conversation, the harness keeps whatever the work needs:
- project threads;
- build leads and their step children;
- read-only sub-contexts;
- later, the orchestrator's coordination.

All of it is logs a person *can* open, and none of it is something they *must* track. The PRD's "monothreading" (one ordered log, one writer, per thread) is unchanged. This is about what the person sees.

**2. Filing is a set of decision kinds the harness makes.** Each kind is named, logged, and judged separately:

| Kind | The question | Today |
| --- | --- | --- |
| `project` | Which project does this message belong to? | the model proposes, the person confirms (phase 6 §9) |
| `ticket` | Is this a new issue, a comment on one, or nothing? | the person decides |
| `knowledge` | Memory, project knowledge, a lesson, or nothing? | a keyword cue gate plus extraction (#14, ADR 0001's `memory_gate`) |
| `route` | Which profile or model runs this turn or step? | the person (`--profile`) or the workflow file |
| `work` | Should this become a build, a lane thread, or a task posted elsewhere? | the person |
| `working_set` | What stays in context? | no ask by design (phase 6 §13) |

New kinds are added as the harness takes on more filing. A kind is a `Decider` site when its question is small and closed (ADR 0001). Otherwise the main model proposes it.

**3. Autonomy is earned per kind, from the operator's own answers.** Each kind sits at one stage for each operator:
- **Ask.** The harness proposes and the person confirms in one keystroke (`y`, `n`, or `n <where>`). The proposal and the answer are an event, which is the label.
- **Tell.** The harness acts, says what it did in one line, and offers an undo.
- **Silent.** The harness acts. It shows in the log and in a daily digest, not in the conversation.

A kind moves up when its recent record earns it. The default rule is at least 95% of the last 30 proposals of that kind accepted unchanged, and, for a `Decider` site, ADR 0001's evaluation agreeing. It moves down on an undo, a refusal or a correction: one in *tell* or *silent* drops it a stage. Thresholds are configurable per kind. Every stage change is a new event kind (`autonomy_changed`: kind, operator, from, to, the record that justified it), so the log says why the harness stopped asking. Every kind starts at **ask**.

**4. Some decisions never leave *ask*:**
- policy, permission and deny rules;
- a push or publish beyond what a workflow already allows;
- publishing a release;
- spending past a budget;
- anything that speaks for the person to someone else (a comment, an email, a message);
- deleting anything.

These are the PRD's "authority stays with humans", made explicit.

**5. The work pulse is learned, not configured.** The harness proposes structure it sees in the log: recurring routines ("you check the open issues every morning"), groupings ("these three folders are one project"), and rhythms ("builds wait for you after 17:00"). Each proposal is a decision kind on the same ladder. Scheduled work, the orchestrator's heartbeat, is one such kind and starts at *ask* like the others.

## Consequences

- **Phase 6:**
  - #7 (the project proposal) gains the ladder, where today it always asks.
  - #9 and #10 (start-is-resume; workspaces and the picker) are read through point 1. The picker becomes the fallback, not the way in.
  - #11 (the working set) is already consistent with this.
  - #12's ask count becomes the measure this ADR optimises.
- **The orchestrator** (phase 8) is the part of the harness that files and coordinates *behind* the conversation, not a second place a person goes. Parts of it (`ticket`, `work`) may come before phase 8.
- **Layer 2:** `/build` in the REPL (#68) is the first step, starting work from the conversation. Later, `work` proposes "this should be a build" itself.
- **Code:**
  - `log` gains `autonomy_changed` (added, never changed), and proposals reuse `decision_made` or the existing proposal events;
  - `runtime` owns each kind's current stage, folded from the log;
  - config gains per-kind thresholds.
  - No crate gains a new edge.
- **Risks:**
  - A wrong *silent* filing is invisible until it matters. So every *tell* or *silent* action must be reversible, the digest must exist before anything goes *silent*, and a correction demotes the kind at once.
  - With several people in a project (phase 5's multiplayer), stages are per operator, never shared.
  - Learning a pulse must not mean watching beyond the harness's own logs.

## Open

- The default thresholds, and whether *tell* needs a time window for undo.
- The digest's form: a daily thread message, `aigentic stats`, or the status line.
- Whether a kind's stage can differ by project for the same operator.
- Which kinds come first after `project`. `knowledge` is a likely candidate, since ADR 0001 already orders `memory_gate` first.
