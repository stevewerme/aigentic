# aigentic-skills

Skill discovery, lockfile verification and the static check. Phase 3 of
`docs/PLAN-phase3.md`; depends on `aigentic-core` only.

- `Manifest::parse` reads a `SKILL.md`: flat `key: value` frontmatter
  (`name`, `description`, `disable-model-invocation`, `argument-hint`),
  then the body. `disable-model-invocation: true` makes the skill
  user-invoked. Every companion file is listed; `scripts()` picks the
  ones that look like scripts.
- `discover` walks project, user and bundled roots recursively; the same
  name closer wins. Two folders with one name in a single root is an error.
- `Lockfile` is `skills.lock.toml`: one `[[skill]]` per entry with source,
  commit, the SHA-256 of `SKILL.md`, the SHA-256 of every other file,
  invocation, `requires` and `review` (`"pending"` or `{ by, on }`).
  `LockEntry::verify` refuses on any mismatch, extra or missing file.
- `check` produces `Finding`s with file and line: tool references, URLs,
  script files and shell invocations, permission widening, ignore-rules
  phrases. Pattern lists are constants; `PATTERN_VERSION` dates a review.
- `SkillSet::load(enabled, roots, lock, tools)` resolves, verifies and
  checks `requires`; `descriptions()` is the byte-stable prefix block.

Fixtures for every finding kind, the planted injection, and project and
user overrides live in `tests/fixtures`.
