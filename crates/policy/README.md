# aigentic-policy

Permission rules for tool calls. Phase 3 of `docs/PLAN-phase3.md`;
depends on `aigentic-core` only.

`Policy::decide(call, class)` evaluates `rules` first match wins and
returns `Allow { rule }`, `Ask { reason }` or `Deny { rule }`. The rule
name (`class read`, `tool mcp.*`, `bash allow-pattern cargo test`) is
what the runtime records on the tool result. No matching rule asks.

`Policy::defaults()` is the plan's table: `safe` and `read` allow, a
`bash` command matching an allow pattern allows, `exec`, `write` and
`network` ask, `mcp.*` asks. A command line is read segment by
segment (#15): the scanner reads quoting, so `grep -n "a|b"` is one
command and not a pipe, and splits at `|`, `||`, `&&`, `&`, `;`,
newlines and grouping parens. A line runs without asking when every
segment is an allow-list command without its writing flags (`sed -i`,
`find -delete`, `git branch -d`, `sort -o`, `git diff --output`, an
awk program with `system(` or a redirection, `gh api` without GET),
`cd`, an assignment, or a substitution that is itself read-only.
`2>&1`, `>/dev/null` and `2>/dev/null` are harmless; any other `>` or
`>>` writes. `Policy::configured` maps `[policy]` in `aigentic.toml`:
`rules` are prepended, `bash_allow` replaces the defaults.
