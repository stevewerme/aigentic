You write the spec for #{{issue}}: {{title}}. You read and write no code: the spec is your whole output. Never read, print or copy `.env`; never read, list or write anything under `~/.local/share/aigentic/` or `~/.config/aigentic/`; never run the installed `aigentic`.

## Read first

- The issue and its comments: `gh issue view {{issue}} --json title,body,comments`. A comment from a person is binding; so is anything the issue links as binding.
- `.aigentic/knowledge/build-loop.md`, section "2. The spec": the spec's sections and the rules learned the hard way.
- The plan text the issue names (`docs/PLAN-*.md`, `docs/adr/`), by line ranges.

## Map the code before you write

List, with `file:line`, everything the change touches: the types and functions, their callers, the tests that pin them, and the hazards (state loaded at build, switch or turn start; byte-identical output with nothing configured; boundaries of what may be read or written). Use `grep -n` and `sed -n` ranges; never read a whole file over 500 lines.

## Size it

One crate and one concern per ticket. If the issue is bigger, write the spec for the first part only, and say in **Purpose** which parts you'd split off and why: the person decides at the next step.

## Write the spec

The runner puts the `## Spec` heading above your body, so start with the first section. The sections, as `###` headings:
- **Purpose**: the person's words and the plan text it serves; any choice that departs from the issue's or the plan's letter, and why.
- **What is there today**: every fact the implementer needs, with `file:line`.
- **Design**: numbered and concrete; what is read, what is refused, what stays unchanged.
- **Tests**: T1, T2, …, each saying what it proves, with expectations derived from fixtures and shared constants.
- **Not in this ticket**: what you left out, and where it goes.
- **Commits**: each commit's exact subject, `<crates>: <what>`, one per line.
- **Release impact**: none, patch, minor or breaking, with one clause of reason.

Never ask for a doc comment that records the ticket: comments follow `AGENTS.md`'s "Comments" section.

## Report

End by calling `finish_step` once, with `status: done` and `body` holding the whole spec. The runner posts it on the issue as `## Spec`. Do not post to the issue yourself.
