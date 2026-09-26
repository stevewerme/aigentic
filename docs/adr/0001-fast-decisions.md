# 0001: Fast decisions behind a `Decider`, separate from the chat model

Status: accepted, 2026-09-26 (Steve)

## Context

The harness asks a model two kinds of question. Most are open-ended:
plan this, write that, judge this diff. A growing number are small and
closed, asked often, with an answer the code consumes directly:

- Is there anything in these turns worth remembering? Today a keyword
  cue gate (#14) decides, then a full extraction call reads the whole
  stretch (17,433 input tokens for a 2-token answer on #44's
  implementer thread; 122 such calls, uncounted, #46).
- Does this message belong to another project? Today the main model
  proposes (`suggest_project`, phase 6 decision 3), and every proposal
  and answer is logged so "a later classifier" can be judged against it.
- Which skills does this turn need? Today every enabled skill's
  description sits in the prompt and the model picks.
- Later: which profile should run this step? (Routing; see below.)

A chat model answers these, but slowly (seconds), at chat prices, and
as text that has to be parsed. A new class of model (the "System 1"
models: typed output, a confidence per answer, tens of milliseconds,
input-only pricing) targets exactly this shape. Which vendor or model
is best will change week to week; this ADR fixes what the harness needs
from one, not which one.

## Decision

**1. A `Decider` trait in `core`, beside `Provider`, not inside it.**
Three question shapes, each returning a typed answer with a confidence
in [0, 1] and the ability to abstain:

- `choose(question, state, options) -> (option, confidence)`
- `yes_no(question, state) -> (bool, confidence)`
- `score(question, state, scale) -> (value, confidence)`

`state` is text the call site assembles (a transcript slice, a message,
a skill list). The trait is async the way `Provider` is
(`futures-core`, no runtime), so `core` keeps the dependency rule.

**2. Backends are adapters, and one is always available.** `providers`
gets (a) a System 1 adapter for whatever typed-decision API is current,
and (b) a structured-output adapter that asks any chat profile the same
question and reads a constrained reply. (b) means the feature never
depends on one vendor existing, and it is the baseline every System 1
backend must beat. A profile names its kind (`kind = "decider"`) and
its backend; decision sites name a profile, never a model.

**3. Every decision site has a fallback that is today's behaviour.**
Below the site's confidence threshold, on abstain, on timeout (a
per-site latency budget, default 500 ms) or on error, the site does
what it does today (run the cue gate, let the main model propose, show
all skills). A decider can therefore only save cost or time; it can
never make a site worse than it is now. Irreversible or costly-when-
wrong questions are not decision sites (see 6).

**4. Every decision is an event.** New kind `decision_made`: site,
question, answer, confidence, abstained, profile, model, latency, usage
and `acted` (whether the answer was used or the fallback ran). The log
stays the source of truth, `stats` counts and prices decisions like any
other call, and the outcome that follows (the extraction wrote nothing;
the person refused the switch) is the label that judges the decision.

**5. Sites start in shadow mode.** Per site in config:
`mode = "off" | "shadow" | "live"`, `profile`, `threshold`. Shadow
decides and logs but always runs the fallback, so accuracy is measured
on real traffic at no risk. A site goes live only when an evaluation
shows it at least matches the fallback on that site's history.

**6. Evaluation is a command, run against our own logs.**
`aigentic eval decisions --site <site> --profile <p>` replays the
labelled history of a site through a decider and reports accuracy,
calibration (are 0.9-confidence answers right 90% of the time),
latency and cost. Labelled history per site:

| Site | Question | Label in the log |
| --- | --- | --- |
| `memory_gate` | yes/no: worth extracting? | `memory_extracted.written` empty or not |
| `project_switch` | choice: which project, or none | `project_proposed` + the person's answer |
| `skill_select` | choice (multi): which skills | `skill_loaded` in the turns that followed |
| `route` (later) | choice: which profile | the step's verdict and cost (`stats --issue`) |

Re-running this when a new model appears is how "the available models
change every week" becomes a config edit, not a redesign.

**7. Order of sites.** `memory_gate` first (frequent, cheap when wrong,
cleanly labelled, fixes #46's spend). `project_switch` second (labelled
since phase 6). `skill_select` third. `route` last: a wrong route costs
money or quality silently, and it needs outcome data per profile that
`stats --issue` (#40) only starts to collect.

**Not decision sites:** release version bumps, review verdicts, anything
irreversible. The judge already reads the diff against the issue, so it
states the release impact in its `## Review`
(`Release impact: none | patch | minor | breaking`); a runner step takes
the highest since the last tag. That is a deterministic fold over
judgements already made, not a fast guess.

## Consequences

- `core` gains a trait and a payload; `log` gains `decision_made`
  (added, never changed); `providers` gains two adapters; `runtime`
  owns the sites and their fallbacks; config gains decider profiles and
  per-site settings. No crate gains a new edge.
- The chat model's prompt shrinks as sites go live (skill descriptions,
  the switch instruction), which is where most of the saving is.
- A decider backend is replaceable by editing a profile; the eval
  command says whether the replacement is better before it acts.
- Data residency is a profile property like any other: an EU,
  zero-retention decider is chosen in config, not in code.
- Risk: calibration differs by backend, so thresholds are per site and
  per profile, set from the eval's calibration table, never guessed.
- Open: whether `state` needs a size cap per site (System 1 inputs are
  priced per token too); whether multi-select is its own shape or
  repeated `yes_no` over the options.
