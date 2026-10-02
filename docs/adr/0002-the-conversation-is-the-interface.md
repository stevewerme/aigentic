# 0002: The conversation is the interface; structure is the harness's job

Status: accepted, 2026-10-02 (Steve), after the grilling recorded in `docs/PLAN-phase6.md` §14

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

**1. One conversation in front, any number of threads behind.** The person talks in one **front thread** that never has to end. Plain `aigentic` reopens it from any folder, and `/new` exists for a deliberate fresh start. The working set (phase 6 §13) keeps it cheap however long it gets. The person never has to choose a thread, a project or a model, or manage a context window. Behind the conversation, the harness keeps whatever the work needs:
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
| `job` | Should this run in the background, as a build or a background job? | the person |
| `working_set` | What stays in context? | no ask by design (phase 6 §13) |

**`knowledge` is the one deliberate exception** to "every kind starts at *ask*": memory writes on its own, as it does today, because a memory line is cheap to see (`/memory`) and to undo, and proposals in every turn would be noise.

New kinds are added as the harness takes on more filing. A kind is a `Decider` site when its question is small and closed (ADR 0001). Otherwise the main model proposes it.

**3. Autonomy is earned per kind, from the operator's own answers.** Each kind sits at one stage for each operator:
- **Ask.** The harness proposes and the person confirms in one keystroke (`y`, `n`, or `n <where>`). The proposal and the answer are an event, which is the label.
- **Tell.** The harness acts, says what it did in one line, and offers an undo.
- **Silent.** The harness acts. It shows in the log and in a daily digest, not in the conversation.

A kind moves up when its recent record earns it. The default rule is at least 95% of the last 30 proposals of that kind accepted unchanged, and, for a `Decider` site, ADR 0001's evaluation agreeing. It moves down on an undo, a refusal or a correction: one in *tell* or *silent* drops it a stage. Thresholds are configurable per kind. Every stage change is a new event kind (`autonomy_changed`: kind, operator, from, to, the record that justified it), so the log says why the harness stopped asking. Every kind starts at **ask**, `knowledge` excepted (point 2).

**Sequencing.** Measuring ships first, promotion later. Phase 6 builds every decision as a proposal answered in one keystroke, logged with its kind, plus a per-kind acceptance report (`aigentic stats --decisions`). The promotion to *tell* and *silent* (`autonomy_changed`, thresholds, undo, the digest) comes after phase 6's acceptance week, which provides the data the thresholds are set from. That's ADR 0001's shadow-first rule.

**4. Some decisions never leave *ask*:**
- policy, permission and deny rules;
- a push or publish beyond what a workflow already allows;
- publishing a release;
- spending past a budget;
- anything that speaks for the person to someone else (a comment, an email, a message);
- deleting anything.

These are the PRD's "authority stays with humans", made explicit.

**5. Background work is a background job.** The model proposes one with `start_job(task)` (the `job` kind). It runs in a child thread in the current project, under the front thread's permission mode and the step deny list. Its questions come to the front thread as tagged in-place prompts. The front thread carries on, and the job's summary returns as a cell. `/jobs` lists jobs, builds among them, and the status line counts the running ones. Conversation-sized work stays in the front thread, which switches project as phase 6 §9 says.

**6. The work pulse is learned, not configured.** The harness proposes structure it sees in the log: recurring routines ("you check the open issues every morning"), groupings ("these three folders are one project"), and rhythms ("builds wait for you after 17:00"). Each proposal is a decision kind on the same ladder. Scheduled work, the orchestrator's heartbeat, is one such kind and starts at *ask* like the others.

## Consequences

- **Phase 6** (`docs/PLAN-phase6.md` §14 records the decisions):
  - the front thread replaces "the latest thread" in start-is-resume (#9);
  - #7's switch proposal is the `project` kind, logged for the ladder;
  - `-w` starting scopes go, and the picker is the fallback (#10);
  - the working set (#11) becomes a requirement;
  - background jobs and `/jobs` are new scope;
  - the acceptance week (#12) measures asks, proposals per kind, jobs, the working set, and whether a person ever needed `/new`.
- **Scale:** the workspace (a client or company) is the boundary for understanding, and the project is the boundary for touching. The front thread's prefix carries the current workspace in detail and every other workspace as one line, so ten clients with three projects each stay manageable.
- **The orchestrator** (phase 8) is the part of the harness that files and coordinates *behind* the conversation, not a second place a person goes. Background jobs are its first, local piece.
- **Layer 2:** `/build` in the REPL (#68) is the first piece of starting work from the conversation, and a build is a kind of job. The `ticket` kind ("this should be an issue") comes after phase 6, tracked in its own issue and linked from #59.
- **Code:**
  - `log` gains `autonomy_changed` (added, never changed) when promotion lands, and proposals use one event shape across kinds, settled in the plan;
  - `runtime` owns each kind's current stage, folded from the log;
  - config gains per-kind thresholds;
  - no crate gains a new edge.
- **Risks:**
  - A wrong *silent* filing is invisible until it matters. So every *tell* or *silent* action must be reversible, the digest must exist before anything goes *silent*, and a correction demotes the kind at once.
  - With several people in a project (phase 5's multiplayer), stages are per operator, never shared.
  - Learning a pulse must not mean watching beyond the harness's own logs.

## Open

Deferred to the promotion work after phase 6's acceptance week:
- the default thresholds, and whether *tell* needs a time window for undo;
- the digest's form;
- whether a kind's stage can differ by project for the same operator.
