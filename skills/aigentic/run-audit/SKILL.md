---
name: run-audit
description: After a build thread stops, read what happened (end reason, cost, context, commits, issue state, stray files), diagnose any early stop, and say the next step.
disable-model-invocation: true
---

# Run audit

Check a finished or stopped build thread and decide what happens next. Do
not edit code.

## 1. Find the thread

The newest log is the last run:
`ls -t ~/.local/share/aigentic/threads/*/*.jsonl | head -1`. The file name
is the thread id. When `aigentic stats` exists (#31), prefer it for the
numbers.

## 2. Read the numbers

From its events: the last `turn_ended` reason; the number of model calls;
peak context (input plus cache read per call); cold calls (uncached input
over half the context); `context_evicted` sweeps; tool results that are
errors; cost from `usage.cost_usd` when present, otherwise from the
profile's prices.

Healthy: context mostly 30–70k, almost all cached, cold calls only right
after a sweep, cost within the profile's cap.

## 3. Diagnose a stop

| End reason | Meaning | Next step |
| --- | --- | --- |
| `done` | finished | audit (step 4) |
| `max_tokens`, `max_iterations`, `max_wall_time` | a cap per turn | `continue` in the same thread; if it recurs, split the task |
| `provider_error: transport …` | the provider dropped or hung | `aigentic doctor --probe`, then `continue` |
| `provider_error: http 400 … budget` | provider account out of credit | the person tops up |
| `interrupted` | someone pressed Esc | read the last user message |

Also look for: the same file read many times with no edits (context
thrash); long gaps between events with no tool running (a sleeping laptop:
`pmset -g log`); a nested session's permission prompt in a pty dump.

## 4. Audit the result

- `git log --oneline -5` and `git status -sb`: commits exist and are
  pushed; nothing unexpected staged or modified.
- Each commit's trailer names the right model and parses as a trailer
  (blank line before it).
- The gate ran (fmt, clippy with `--all-targets`, the full test suite).
- The issue: plan, review and closing comments as the brief asked;
  closed only after review.
- `.aigentic/memory/`: new files hold durable facts, not task
  instructions. Move task chatter out and report it.
- `aigentic doctor` is green (a new config key can break it).

## 5. Report

Five lines at most: what landed (commit ids), cost and calls, anything
wrong with file and line, and the next step as a ready-to-paste prompt:
`continue`, the next role's prompt, or a fix.
