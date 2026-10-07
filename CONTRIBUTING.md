# Contributing to aigentic

Thanks for looking. aigentic is mostly built by its own loop (aigentic
building aigentic, supervised by the maintainer), so a few rules keep
outside work from colliding with it.

## Which issues are open

- **An assigned issue is taken.** The maintainer's loop is building it;
  a pull request for it will be closed, however good.
- **An unassigned issue labelled `ready-for-agent` is open** to anyone,
  person or agent. These are self-contained and fully described in the
  issue; small bugs are the usual case.
- Anything else (no label, `needs-triage`, `needs-info`) isn't ready.
  Comment on the issue rather than opening a pull request.

## A pull request

- **One issue per pull request**, linked with `Fixes #<n>`, and nothing
  beyond what the issue asks.
- **Never edit `docs/PRD.md` or `docs/PLAN-*.md`.** They are binding and
  the maintainer's; if your change needs them to say something new, say
  so in the pull request and the maintainer will.
- **The gate passes**, each command on its own:

  ```bash
  cargo fmt
  ```

  ```bash
  cargo clippy --all-targets -- -D warnings
  ```

  ```bash
  cargo test --no-fail-fast
  ```

- Follow `AGENTS.md`: the crate layout, the dependency rule, the event log
  as the source of truth, and the conventions. Commit subjects read
  `crate: what changed`.
- No new dependencies without saying why in the pull request.
- Tests derive their expected values from the code or the issue's rule,
  never hand-computed numbers.

## Security

Changes to the policy crate, permission rules, the `bash` tool's process
handling, or anything that touches credentials get the closest review,
and may be redone in the loop rather than merged. Report a vulnerability
privately to the maintainer, not in a public issue.

## Licence

aigentic is MIT-licensed; by opening a pull request you agree your
contribution is under the same licence.
