# Skills review

Findings from the static check (pattern version 1) over the vendored set. Generated; regenerate with `AIGENTIC_REGEN=1 cargo test -p aigentic-skills --test vendored`. Findings are for a reviewer, not verdicts: a `Pending` skill with findings blocks `aigentic skills check`, and a human clears it by setting `review` in `skills.lock.toml`. Tool references are the seed for `requires`.

## ask-matt

user-invoked · review pending · `skills/pocock/engineering/ask-matt`

Tools referenced: bash

URLs:

- `SKILL.md:32` https://www.aihero.dev/ai-coding-dictionary/smart-zone
- `PHASE-BOUNDARIES.md:21` https://www.aihero.dev/ai-coding-dictionary/smart-zone

Tool references:

- `SKILL.md:83` bash

## code-review

model-invoked · accepted by steve on 2026-09-21 · `skills/pocock/engineering/code-review`

Tools referenced: pin

Tool references:

- `SKILL.md:17` pin

## diagnosing-bugs

model-invoked · accepted by steve on 2026-09-21 · `skills/pocock/engineering/diagnosing-bugs`

Tools referenced: bash, grep, pin

Scripts and shell:

- `SKILL.md:27` 2. **Curl / HTTP script** against a running dev server.
- `SKILL.md:59` Phase 1 is done when the loop is **tight** and **red-capable**: you can name **one command** (a script path, a test invocation, a curl) that you have **already run at least once** (show the invocation and its output, redacted), and that is:
- `scripts/hitl-loop.template.sh:1` script file

URLs:

- `scripts/hitl-loop.template.sh:34` http://localhost:3000

Tool references:

- `SKILL.md:35` bash
- `SKILL.md:45` pin
- `SKILL.md:108` grep
- `SKILL.md:110` grep
- `SKILL.md:136` grep
- `scripts/hitl-loop.template.sh:1` bash
- `scripts/hitl-loop.template.sh:7` bash

## git-guardrails-claude-code

model-invoked · review pending · `skills/pocock/misc/git-guardrails-claude-code`

Tools referenced: bash, grep

Scripts and shell:

- `scripts/block-dangerous-git.sh:1` script file

Tool references:

- `SKILL.md:48` bash
- `SKILL.md:68` bash
- `SKILL.md:91` bash
- `scripts/block-dangerous-git.sh:1` bash
- `scripts/block-dangerous-git.sh:19` grep

## improve-codebase-architecture

user-invoked · review pending · `skills/pocock/engineering/improve-codebase-architecture`

URLs:

- `HTML-REPORT.md:15` https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs

## loop-me

user-invoked · review pending · `skills/pocock/in-progress/loop-me`

Widens permissions:

- `SKILL.md:27` without asking

## migrate-to-shoehorn

model-invoked · review pending · `skills/pocock/misc/migrate-to-shoehorn`

Tools referenced: bash, grep

Tool references:

- `SKILL.md:22` bash
- `SKILL.md:114` grep

## pr

model-invoked · review pending · `skills/pocock/in-progress/pr`

URLs:

- `SKILL.md:9` https://github.com/humanlayer/skills/blob/main/plugins/show-me/skills/show-me/SKILL.md
- `CREDITS.md:3` https://github.com/dexhorthy
- `CREDITS.md:3` https://github.com/humanlayer/humanlayer

## scaffold-exercises

model-invoked · review pending · `skills/pocock/misc/scaffold-exercises`

Tools referenced: bash

Tool references:

- `SKILL.md:75` bash
- `SKILL.md:92` bash

## setup-matt-pocock-skills

user-invoked · review pending · `skills/pocock/engineering/setup-matt-pocock-skills`

Widens permissions:

- `SKILL.md:59` without asking

URLs:

- `SKILL.md:45` https://gitlab.com/gitlab-org/cli
- `issue-tracker-gitlab.md:3` https://gitlab.com/gitlab-org/cli

## setup-pre-commit

model-invoked · review pending · `skills/pocock/misc/setup-pre-commit`

Tools referenced: bash

Tool references:

- `SKILL.md:31` bash

## setup-ts-deep-modules

user-invoked · review pending · `skills/pocock/in-progress/setup-ts-deep-modules`

Scripts and shell:

- `dependency-cruiser.config.cjs:1` script file

URLs:

- `SKILL.md:9` https://github.com/sverweij/dependency-cruiser

## teach

user-invoked · review pending · `skills/pocock/productivity/teach`

URLs:

- `RESOURCES-FORMAT.md:12` https://example.com
- `RESOURCES-FORMAT.md:14` https://example.com
- `RESOURCES-FORMAT.md:19` https://reddit.com/r/weightroom

## wayfinder

user-invoked · review pending · `skills/pocock/engineering/wayfinder`

Tools referenced: pin

Tool references:

- `SKILL.md:84` pin
- `SKILL.md:111` pin

## wizard

model-invoked · review pending · `skills/pocock/engineering/wizard`

Tools referenced: bash, grep

Scripts and shell:

- `template.sh:1` script file

URLs:

- `template.sh:194` https://dashboard.stripe.com/test/apikeys

Tool references:

- `SKILL.md:3` bash
- `SKILL.md:8` bash
- `SKILL.md:41` bash
- `template.sh:1` bash
- `template.sh:94` grep
- `template.sh:134` grep

## writing-for-agents

model-invoked · review pending · `skills/pocock/productivity/writing-for-agents`

Widens permissions:

- `SKILL-MECHANICS.md:9` disable
- `SKILL-MECHANICS.md:10` disable

## No findings

- claude-handoff
- codebase-design
- domain-modeling
- grill-me
- grill-with-docs
- grilling
- handoff
- implement
- implement-spec
- prototype
- research
- resolving-merge-conflicts
- retro
- tdd
- to-questionnaire
- to-spec
- to-tickets
- triage
- wait-what
- writing-beats
- writing-fragments
- writing-shape
