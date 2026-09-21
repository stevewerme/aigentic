# aigentic-policy

Permission rules for tool calls. Phase 3 of `docs/PLAN-phase3.md`;
depends on `aigentic-core` only.

`Policy::decide(call, class)` evaluates `rules` first match wins and
returns `Allow { rule }`, `Ask { reason }` or `Deny { rule }`. The rule
name (`class read`, `tool mcp.*`, `bash allow-pattern cargo test`) is
what the runtime records on the tool result. No matching rule asks.

`Policy::defaults()` is the plan's table: `safe` and `read` allow, a
`bash` command matching an allow pattern allows, `exec`, `write` and
`network` ask, `mcp.*` asks. A pattern matches when the command's leading
words equal the pattern's words and the command contains none of `|`,
`;`, `&&`, `>`, `$(`, a backtick or a newline. `Policy::configured` maps
`[policy]` in `aigentic.toml`: `rules` are prepended, `bash_allow`
replaces the defaults.
