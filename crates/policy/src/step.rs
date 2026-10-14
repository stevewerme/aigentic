//! The per-step deny overlay.
//!
//! PLAN-layer2 §6.4 splits a task between a step thread and the runner:
//! the step writes and commits, the runner checks the tree, pushes once
//! and installs. A few commands and files belong to the runner alone, so
//! a step that runs them is a defect whatever the rules, the project or
//! the mode say. The runner supplies the list (`deny`), [`StepOverlay`]
//! parses it, and the runtime consults it once the rules have spoken —
//! its answer stands in theirs. See `Runtime::policy_check`.
//!
//! The grammar is small and closed on purpose: anything the overlay does
//! not recognise is a [`DenyParseError`] where the overlay is built, never
//! a silent no-op at call time.
//!
//! This is a lexical guard, not a sandbox (that is phase 7). A word
//! names a program by its final path component and quoting it changes
//! nothing, so `\git push` and `"git" push` deny as `git push`. Stated
//! limits: a command reached through `xargs`, `find -exec`, `eval` or
//! `python -c`; a wrapper with no row in
//! [`FORWARDERS`](crate::shell::FORWARDERS) (`doas`, `setsid`, `run0`,
//! `ionice`, `chrt`, `flock`, `unbuffer`, `script`) or a multi-call
//! binary that chooses its command from its own words (`busybox git
//! push`); `env -S <line>`, whose value is a command line the reader
//! does not split; a here-document body fed to a shell (`bash <<EOF`
//! with the command inside); `PATH` shadowing, where the reader reads
//! the word `git` and never the program it resolves to; a variable set
//! in an earlier call, which is not on the line the reader reads
//! (`export GIT_INDEX_FILE=/tmp/idx` alone, then `git read-tree HEAD`
//! in the next call); and a word that looks like an assignment where
//! git takes an argument, refused along with the real thing (`git log
//! GIT_INDEX_FILE=<blob>`). A word that mixes literal text with a
//! substitution is refused rather than read: `$cmd push` reads as
//! `step deny unreadable`, and `cat $( git push )` as `step deny git
//! push`.

use std::fmt;

use aigentic_core::ToolCall;

use crate::Outcome;
use crate::shell::{GIT_VALUES, SegRead, segment_reads};

/// The one entry a step may be armed past: a writing step that has
/// committed something it must reword folds that work back with
/// [`StepOverlay::with_restack_base`]. Nothing else moves the tree.
const FOLD_ROW: &[&str] = &["git", "reset"];

/// The command entries, as word prefixes of a bash segment's effective
/// words: `(label, words, reason)`. `git commit` and `git add` are for a
/// step that only reads and reports; a writing step commits.
const COMMANDS: &[(&str, &[&str], &str)] = &[
    ("git push", &["git", "push"], PUSH_REASON),
    ("git rebase", &["git", "rebase"], TREE_REASON),
    ("git reset", FOLD_ROW, TREE_REASON),
    ("git checkout --", &["git", "checkout", "--"], TREE_REASON),
    ("git stash", &["git", "stash"], TREE_REASON),
    ("git clean", &["git", "clean"], TREE_REASON),
    ("git add -A", &["git", "add", "-A"], TREE_REASON),
    ("git update-ref", &["git", "update-ref"], TREE_REASON),
    ("git commit-tree", &["git", "commit-tree"], TREE_REASON),
    ("git read-tree", &["git", "read-tree"], TREE_REASON),
    ("git update-index", &["git", "update-index"], TREE_REASON),
    ("git replace", &["git", "replace"], TREE_REASON),
    ("git filter-branch", &["git", "filter-branch"], TREE_REASON),
    ("git filter-repo", &["git", "filter-repo"], TREE_REASON),
    (
        "git reflog expire",
        &["git", "reflog", "expire"],
        TREE_REASON,
    ),
    (
        "git reflog delete",
        &["git", "reflog", "delete"],
        TREE_REASON,
    ),
    ("git symbolic-ref", &["git", "symbolic-ref"], TREE_REASON),
    (
        "git pull --rebase",
        &["git", "pull", "--rebase"],
        TREE_REASON,
    ),
    ("git pull -r", &["git", "pull", "-r"], TREE_REASON),
    ("git commit", &["git", "commit"], READ_ONLY_REASON),
    ("git add", &["git", "add"], READ_ONLY_REASON),
];

/// The one environment variable entry: a scratch index in front of git.
const INDEX: &str = "GIT_INDEX_FILE";

/// The one symbolic entry: the `.env` file, read or written.
const COPY_ENV: &str = "copy .env";

/// The rule a command word that cannot be read denies with.
const UNREADABLE: &str = "unreadable";

/// The `git reset` spelling the fold is: the soft reset to the armed
/// base. Anything else — `--hard`, `--mixed`, another commit, a fifth
/// word — is denied by the `git reset` entry.
const FOLD_OPTION: &str = "--soft";

/// The shortest word that may name the base: git resolves an abbreviated
/// object id of seven hex digits and up, so a shorter word names
/// nothing and the fold is refused.
const BASE_ABBREV: usize = 7;

const PUSH_REASON: &str =
    "the runner pushes once after its checks (PLAN-layer2 §6.4); commit and call finish_step";

const TREE_REASON: &str =
    "the tree is the runner's evidence; commit your work and call finish_step";

const READ_ONLY_REASON: &str =
    "this step reads and reports; it changes nothing in git, so call finish_step with your report";

const ENV_REASON: &str =
    "a step thread never reads or writes .env (#41); the key must not reach the log (AGENTS.md)";

const UNREADABLE_REASON: &str = "keep commands literal in a step thread";

/// A deny list the overlay's grammar does not recognise. Named loudly
/// where the overlay is built, so a workflow typo never becomes a call
/// the runner thought was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenyParseError {
    /// The entry as it was written.
    pub entry: String,
}

impl fmt::Display for DenyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown step deny entry `{}`: one of {} or `{}`",
            self.entry,
            COMMANDS
                .iter()
                .map(|(label, _, _)| format!("`{label}`"))
                .collect::<Vec<_>>()
                .join(", "),
            COPY_ENV
        )
    }
}

impl std::error::Error for DenyParseError {}

/// The parsed deny list of one step thread.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StepOverlay {
    /// The command entries the list names, in the list's order.
    commands: Vec<(&'static str, &'static [&'static str], &'static str)>,
    /// Whether the list carries the `.env` entry.
    copy_env: bool,
    /// The commit the step may fold back to, when the runner armed one:
    /// `Some` only for a writing step whose own commits are the head of
    /// an unmoved remote. [`StepOverlay::parse`] never sets it, so a
    /// list parsed on its own denies every `git reset`.
    restack_base: Option<String>,
}

impl StepOverlay {
    /// Parse a workflow's `deny` list.
    pub fn parse(deny: &[String]) -> Result<Self, DenyParseError> {
        let mut overlay = StepOverlay::default();
        for entry in deny {
            // Written either way — `git  push`, ` git add -A ` — the
            // entry means the same command.
            let text = entry.split_whitespace().collect::<Vec<_>>().join(" ");
            if let Some(&(label, words, reason)) =
                COMMANDS.iter().find(|(label, _, _)| *label == text)
            {
                overlay.commands.push((label, words, reason));
            } else if text == COPY_ENV {
                overlay.copy_env = true;
            } else {
                return Err(DenyParseError {
                    entry: entry.clone(),
                });
            }
        }
        Ok(overlay)
    }

    /// Arm the fold: the step may restack the commits it made since
    /// `base` with one exact `git reset --soft <base>` line, and
    /// `None` leaves the overlay exactly as it reads without this call.
    pub fn with_restack_base(mut self, base: Option<String>) -> Self {
        self.restack_base = base;
        self
    }

    /// What the overlay says about a call, whatever the rules and the
    /// mode say: `None` means the call is not its business, a `Deny` is
    /// final.
    pub fn decide(&self, call: &ToolCall) -> Option<Outcome> {
        if call.name == "bash" {
            let command = call.args.get("command")?.as_str()?;
            let reads = segment_reads(command);
            // A command word that cannot be read could be any entry, so
            // it is refused first: no entry may be talked past by making
            // the line unreadable, and `cat $( git push )` stays an entry
            // deny because *its* segments are readable.
            if reads.iter().any(|read| read.unreadable) {
                return Some(deny(UNREADABLE, UNREADABLE_REASON));
            }
            // A scratch index in front of git is a tree no other check
            // sees, so no deny list is needed to refuse it.
            if index_in_front(&reads) {
                return Some(deny(INDEX, TREE_REASON));
            }
            if let Some((label, reason)) = self.command(&reads) {
                return Some(deny(label, reason));
            }
            if self.copy_env && env_in_bash(&reads) {
                return Some(deny(COPY_ENV, ENV_REASON));
            }
            None
        } else if self.copy_env && env_path(call) {
            Some(deny(COPY_ENV, ENV_REASON))
        } else {
            None
        }
    }

    /// The entry with the most words a segment matches; a tie goes to
    /// the earlier entry. A segment matches when its effective words
    /// start with the entry's words, so `git push` covers
    /// `git push --force` and never `git pushy`. The one segment the
    /// fold covers matches nothing here, so the list keeps its entries
    /// and loses one spelling of one of them.
    fn command(&self, reads: &[SegRead]) -> Option<(&'static str, &'static str)> {
        let fold = self.fold_at(reads);
        let mut best: Option<(&'static str, usize, &'static str)> = None;
        for (label, words, reason) in &self.commands {
            let matched = reads
                .iter()
                .enumerate()
                .any(|(at, read)| Some(at) != fold && starts_with(&read.words, words));
            let better = match best {
                None => true,
                Some((_, len, _)) => words.len() > len,
            };
            if matched && better {
                best = Some((label, words.len(), *reason));
            }
        }
        best.map(|(label, _, reason)| (label, reason))
    }

    /// The segment the fold covers, if any: the armed base in the one
    /// spelling the route names, and only as the line's only `git reset`
    /// — two resets on one line are not that spelling, so the entry
    /// catches both. Every other word of the list, every other segment,
    /// and every guard in `decide` are untouched by the arming.
    fn fold_at(&self, reads: &[SegRead]) -> Option<usize> {
        let base = self.restack_base.as_deref()?;
        let mut resetting = reads
            .iter()
            .enumerate()
            .filter(|(_, read)| starts_with(&read.words, FOLD_ROW));
        let (at, read) = resetting.next()?;
        if resetting.next().is_some() {
            return None;
        }
        let [_, _, option, named] = read.words.as_slice() else {
            return None;
        };
        // A redirection, a `git` global option or a word that is not the
        // base each mean this is some other command that happens to read
        // as the fold's effective words, so it stays the entry.
        (option == FOLD_OPTION
            && !read.redirects
            && names_base(named, base)
            && !read
                .as_written
                .iter()
                .any(|word| GIT_VALUES.iter().any(|value| word.starts_with(value))))
        .then_some(at)
    }
}

/// Whether a word names `base`: the commit itself, or a prefix of at
/// least [`BASE_ABBREV`] hex digits, which git resolves on its own.
/// `HEAD`, a branch, a substitution and a quoted word are none of them,
/// so only the base or its abbreviation is ever a name for it. Case is
/// folded: a sha read back from a caller may be upper case.
fn names_base(word: &str, base: &str) -> bool {
    let word = word.to_ascii_lowercase();
    word.len() >= BASE_ABBREV
        && word.chars().all(|c| c.is_ascii_hexdigit())
        && base.to_ascii_lowercase().starts_with(&word)
}

fn starts_with(words: &[String], entry: &[&str]) -> bool {
    words.len() >= entry.len()
        && entry
            .iter()
            .zip(words)
            .all(|(entry, word)| *entry == word.as_str())
}

fn deny(entry: &str, reason: &str) -> Outcome {
    Outcome::Deny {
        rule: format!("step deny {entry}"),
        reason: reason.to_owned(),
    }
}

/// The `.env` file, by its final path component and nothing else:
/// `.env.example` and `dir.env/x` never match.
fn is_env(word: &str) -> bool {
    word == ".env" || word.ends_with("/.env")
}

/// Whether a `GIT_INDEX_FILE` assignment on the line feeds a git
/// command: the assignment's own segment runs git, or a later segment of
/// the line does. Reading the whole line, rather than only the segment
/// the assignment sits in, is the fail-closed direction: the reader
/// cannot tell a variable exported for later from one set for git alone.
fn index_in_front(reads: &[SegRead]) -> bool {
    reads
        .iter()
        .enumerate()
        .any(|(i, read)| read.sets_index && (runs_git(read) || reads[i + 1..].iter().any(runs_git)))
}

/// Whether a segment runs `git`, judged from its effective words. A
/// value the reader cannot fold into an assignment (quoted, or holding a
/// substitution) leaves the assignment word in the effective list, so
/// the git word is the second one; the assignment is skipped to see it.
fn runs_git(read: &SegRead) -> bool {
    let mut words = read.words.as_slice();
    if let Some((name, _)) = words.first().and_then(|w| w.split_once('='))
        && name == INDEX
    {
        words = &words[1..];
    }
    starts_with(words, &["git"])
}

/// A bash segment carrying the literal word or redirection target `.env`
/// — the rule is literal-only, so `$DIR/.env` is not a path it can read —
/// unless the segment's command word is `.` or `source`, the two ways the
/// pty check loads the key it was given by absolute path.
fn env_in_bash(reads: &[SegRead]) -> bool {
    reads.iter().any(|read| {
        let sources = read
            .words
            .first()
            .is_some_and(|w| w == "." || w == "source");
        !sources && read.literal.iter().any(|w| is_env(w))
    })
}

/// The three tools whose `path` argument reads or writes a file.
fn env_path(call: &ToolCall) -> bool {
    if !matches!(call.name.as_str(), "read_file" | "write_file" | "edit_file") {
        return false;
    }
    let Some(path) = call.args.get("path").and_then(|p| p.as_str()) else {
        return false;
    };
    is_env(path.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Policy;
    use crate::shell::{FORWARDERS, Forwarder, GIT_FLAGS, GIT_VALUES};
    use aigentic_core::{RiskClass, ToolCall};
    use serde_json::json;

    /// The overlay over an empty rule set: every outcome here comes from
    /// the overlay, so the sibling policy is the A/B and the rule name is
    /// the assertion.
    fn policy_with(deny_list: &[&str]) -> StepOverlay {
        StepOverlay::parse(&strings(deny_list)).unwrap()
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "bash".into(),
            args: json!({"command": command}),
        }
    }

    fn call(name: &str, path: &str) -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: name.into(),
            args: json!({"path": path}),
        }
    }

    /// The rule an outcome names, or its kind when it has no rule.
    fn rule(outcome: &Outcome) -> String {
        match outcome {
            Outcome::Allow { rule } => format!("allow {rule}"),
            Outcome::Ask { .. } | Outcome::AskBoundary { .. } => "ask".into(),
            Outcome::Deny { rule, .. } => rule.clone(),
        }
    }

    fn denied(overlay: &StepOverlay, call: &ToolCall) -> String {
        match overlay.decide(call) {
            Some(outcome @ Outcome::Deny { .. }) => rule(&outcome),
            other => panic!("expected a deny, got {other:?}"),
        }
    }

    /// Each entry as a lone command line. The expectation is built from
    /// the same list the overlay parses, so no entry's name is written
    /// twice.
    #[test]
    fn t1_each_entry_denies_a_lone_command_line() {
        for (label, words, _) in COMMANDS {
            let overlay = policy_with(&[label]);
            let command = words.join(" ");
            assert_eq!(
                denied(&overlay, &bash(&command)),
                format!("step deny {label}")
            );
        }
    }

    /// Each entry as a later segment of a compound line.
    #[test]
    fn t2_each_entry_denies_as_a_later_segment() {
        for (label, words, _) in COMMANDS {
            let overlay = policy_with(&[label]);
            let command = format!("cargo test && {}", words.join(" "));
            assert_eq!(
                denied(&overlay, &bash(&command)),
                format!("step deny {label}")
            );
        }
    }

    /// The ways a line can hide an entry: shell plumbing that still runs
    /// the same command, each naming the entry it hides.
    #[test]
    fn t3_the_bypass_pack_names_the_inner_entry() {
        let overlay = policy_with(&["git push", "git rebase", "git add -A"]);
        let cases: &[(&str, &str)] = &[
            ("git -C /tmp push", "git push"),
            ("git --git-dir=/t/g push", "git push"),
            ("env git push", "git push"),
            ("command git push", "git push"),
            ("sh -c 'git push'", "git push"),
            (
                "bash -c \"cargo test && git rebase -i HEAD~2\"",
                "git rebase",
            ),
            ("git add --all", "git add -A"),
            ("echo $(git push)", "git push"),
        ];
        for (command, entry) in cases {
            assert_eq!(
                denied(&overlay, &bash(command)),
                format!("step deny {entry}"),
                "{command}"
            );
        }
    }

    /// A command word nobody can read is refused as unreadable, while a
    /// readable segment whose substitution holds the entry names the
    /// entry (the plan amendment 1 correction: `cat` is literal, so the
    /// segment is read and its substitution is recursed into).
    #[test]
    fn t4_unreadable_command_words_deny_as_unreadable() {
        let overlay = policy_with(&["git push"]);
        for command in ["cmd=$X; $cmd push", "bash -c $SCRIPT", "$cmd push"] {
            assert_eq!(
                denied(&overlay, &bash(command)),
                "step deny unreadable",
                "{command}"
            );
        }
        assert_eq!(
            denied(&overlay, &bash("cat $( git push )")),
            "step deny git push"
        );
    }

    /// The A/B: a command the overlay does not cover reads exactly as it
    /// does with no overlay at all, so a step thread is an ordinary
    /// thread for everything else. `git commit --amend` is the caller's
    /// own example of work that stays allowed. The last two rows reach the
    /// command word past a wrapper's stop option and past a global option,
    /// neither of which is the command.
    #[test]
    fn t5_an_uncovered_command_reads_as_without_the_overlay() {
        let overlay = StepOverlay::parse(&strings(&["git push"])).unwrap();
        let plain = Policy::defaults();
        for command in [
            "git commit --amend -m x",
            "cargo test",
            "git log --oneline -3",
            "command -v git push",
            "git --no-pager log",
        ] {
            let call = bash(command);
            let plain = plain.decide(&call, RiskClass::Exec, None);
            assert!(!matches!(plain, Outcome::Deny { .. }), "{command}");
            assert_eq!(overlay.decide(&call), None, "{command}");
        }
    }

    /// The `.env` entry: the file by its final component, its two
    /// sourcing forms exempt, and the instructive contrast.
    #[test]
    fn t6_the_dot_env_rule() {
        let overlay = policy_with(&[COPY_ENV]);
        let plain = Policy::defaults();
        for command in [
            "cp .env /tmp/41-pty",
            "cat .env",
            "echo x >> .env",
            "cat /repo/.env",
        ] {
            assert_eq!(
                denied(&overlay, &bash(command)),
                "step deny copy .env",
                "{command}"
            );
        }
        for (name, path) in [
            ("read_file", ".env"),
            ("write_file", "sub/.env"),
            ("edit_file", "/repo/.env"),
        ] {
            let call = call(name, path);
            assert_eq!(
                denied(&overlay, &call),
                "step deny copy .env",
                "{name} {path}"
            );
        }
        // The exemption and the exact-component rule are the A/B: the
        // overlay says nothing about these, so `None` comes out and the
        // caller falls through to the sibling policy, which does not
        // deny any of them.
        for command in [
            ". /repo/.env",
            "source /repo/.env",
            "cat .env.example",
            "echo $DIR/.env",
        ] {
            let call = bash(command);
            assert!(
                !matches!(
                    plain.decide(&call, RiskClass::Exec, None),
                    Outcome::Deny { .. }
                ),
                "{command} is not denied without the overlay"
            );
            assert_eq!(overlay.decide(&call), None, "{command}");
        }
        let call = call("read_file", ".env.example");
        assert!(
            !matches!(
                plain.decide(&call, RiskClass::Read, None),
                Outcome::Deny { .. }
            ),
            "the example file is not denied without the overlay"
        );
        assert_eq!(overlay.decide(&call), None);
    }

    /// A step that only reports may not commit or stage anything, and its
    /// refusal says so rather than asking it to commit. The longer entry
    /// still wins where both are listed.
    #[test]
    fn a_reporting_step_may_not_commit_or_stage() {
        let overlay = policy_with(&["git commit", "git add", "git add -A"]);
        for (command, rule) in [
            ("git commit -m x", "step deny git commit"),
            ("git add src/lib.rs", "step deny git add"),
            ("git add -A", "step deny git add -A"),
        ] {
            match overlay.decide(&bash(command)) {
                Some(Outcome::Deny { rule: got, reason }) => {
                    assert_eq!(got, rule, "{command}");
                    assert_eq!(
                        reason.contains("reads and reports"),
                        rule != "step deny git add -A",
                        "{command}: {reason}"
                    );
                }
                other => panic!("{command}: {other:?}"),
            }
        }
        assert_eq!(overlay.decide(&bash("git commit-tree x")), None);
        assert_eq!(overlay.decide(&bash("git log")), None);
    }

    /// Anything outside the grammar is named where the overlay is built.
    #[test]
    fn t7_parse_rejects_an_unknown_entry() {
        let err = StepOverlay::parse(&strings(&["git obliterate"])).unwrap_err();
        assert_eq!(err.entry, "git obliterate");
        assert!(err.to_string().contains("git obliterate"), "{err}");
    }

    /// The positional words a row's wrapper takes of its own, as the `1`
    /// placeholders a line writes them with. `timeout`'s duration is the
    /// only one today.
    fn positional_filler(row: &Forwarder) -> String {
        " 1".repeat(row.positional)
    }

    /// Every option [`FORWARDERS`] lists is read past its wrapper, in
    /// every spelling the reader handles, so the command word behind it
    /// is the one the overlay matches.
    #[test]
    fn t8_every_listed_forwarder_option_reads_past_the_forwarder() {
        let overlay = policy_with(&["git push"]);
        for row in FORWARDERS {
            let filler = positional_filler(row);
            for opt in row.values {
                let mut lines = vec![format!("{} {opt} v{filler} git push", row.name)];
                if opt.starts_with("--") {
                    lines.push(format!("{} {opt}=v{filler} git push", row.name));
                } else {
                    lines.push(format!("{} {opt}v{filler} git push", row.name));
                }
                for line in lines {
                    assert_eq!(
                        denied(&overlay, &bash(&line)),
                        "step deny git push",
                        "{line}"
                    );
                }
            }
            for opt in row.flags {
                let line = format!("{} {opt}{filler} git push", row.name);
                assert_eq!(
                    denied(&overlay, &bash(&line)),
                    "step deny git push",
                    "{line}"
                );
            }
        }
    }

    /// Every entry denies behind every wrapper, so the unwrapping is a
    /// property of the reader and not of the push.
    #[test]
    fn t9_a_wrapped_entry_denies_under_every_forwarder() {
        for row in FORWARDERS {
            let filler = positional_filler(row);
            for (label, words, _) in COMMANDS {
                let overlay = policy_with(&[label]);
                let line = format!("{}{filler} {}", row.name, words.join(" "));
                assert_eq!(
                    denied(&overlay, &bash(&line)),
                    format!("step deny {label}"),
                    "{line}"
                );
            }
        }
    }

    /// Which words name a program: a path's final component decides, and
    /// quoting it changes nothing, because the shell strips the quotes
    /// before the name is used. The words that merely contain a name do
    /// not match it.
    #[test]
    fn t10_a_command_word_matches_by_its_final_path_component() {
        let overlay = policy_with(&["git push"]);
        for line in [
            "/usr/bin/git push",
            "./git push",
            "dir/git push",
            "\"/usr/bin/git\" push",
            "\\git push",
            "\"git\" push",
            "\"sudo\" git push",
            "/usr/bin/sudo /usr/bin/git push",
            "/usr/bin/env git push",
            "/bin/sh -c 'git push'",
        ] {
            assert_eq!(
                denied(&overlay, &bash(line)),
                "step deny git push",
                "{line}"
            );
        }
        for line in [
            "mygit push",
            "foo.git push",
            "git.exe push",
            "git pushy",
            "echo /usr/bin/git push",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
    }

    /// An option's value is not the command word: a reader that took the
    /// first option word for the command would read `git` here and deny a
    /// line that runs `push`.
    #[test]
    fn t11_a_forwarders_value_word_is_not_its_command_word() {
        let overlay = policy_with(&["git push"]);
        for row in FORWARDERS.iter().filter(|r| !r.values.is_empty()) {
            let filler = positional_filler(row);
            for opt in row.values {
                let line = format!("{} {opt} git{filler} push", row.name);
                assert_eq!(overlay.decide(&bash(&line)), None, "{line}");
            }
        }
    }

    /// An option no table knows may be taking the next word as its value,
    /// which is exactly the hiding the reader closes: the line is refused
    /// rather than read past. So is a wrapper whose own word is missing.
    #[test]
    fn t12_an_option_no_table_knows_refuses_the_line() {
        let overlay = policy_with(&["git push"]);
        for line in [
            "sudo --definitely-not-a-sudo-option git push",
            "timeout --whatever 5 git push",
            "git --whatever push",
            "env -Q git push",
            "sudo -u",
            "timeout",
        ] {
            assert_eq!(
                denied(&overlay, &bash(line)),
                "step deny unreadable",
                "{line}"
            );
        }
    }

    /// `git`'s own global options stand between the name and the command
    /// word; the reader steps over each spelling of each of them.
    #[test]
    fn t13_every_listed_git_global_option_reads_past_git() {
        let overlay = policy_with(&["git push"]);
        let mut lines = Vec::new();
        for opt in GIT_VALUES {
            lines.push(format!("git {opt} v push"));
            if opt.starts_with("--") {
                lines.push(format!("git {opt}=v push"));
            } else {
                lines.push(format!("git {opt}v push"));
            }
        }
        for flag in GIT_FLAGS {
            lines.push(format!("git {flag} push"));
        }
        for line in lines {
            assert_eq!(
                denied(&overlay, &bash(&line)),
                "step deny git push",
                "{line}"
            );
        }
    }

    /// A wrapper's stop option means it runs no command of its own, so
    /// nothing after it is a command word. The two `sudo` options that
    /// look like stops but are not (`-k`, `--reset-timestamp`) run the
    /// command, and are listed as flags.
    #[test]
    fn t14_an_option_that_runs_nothing_ends_the_read() {
        let overlay = policy_with(&["git push"]);
        for row in FORWARDERS {
            for opt in row.stops {
                let line = format!("{} {opt} git push", row.name);
                assert_eq!(overlay.decide(&bash(&line)), None, "{line}");
            }
        }
        for line in [
            "sudo -l git push",
            "sudo -K git push",
            "command -v git push",
            "command -V git push",
            "timeout --help git push",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
        for line in ["sudo -k git push", "sudo --reset-timestamp git push"] {
            assert_eq!(
                denied(&overlay, &bash(line)),
                "step deny git push",
                "{line}"
            );
        }
    }

    /// The entry table is what the tests above say it is. `t1`, `t2` and
    /// `t9` derive their rows from `COMMANDS`, so without this pin
    /// dropping an entry would shrink the parser and its tests together.
    #[test]
    fn t15_the_entry_table_matches_the_list_written_out() {
        let expected: &[(&str, &[&str], &str)] = &[
            ("git push", &["git", "push"], PUSH_REASON),
            ("git rebase", &["git", "rebase"], TREE_REASON),
            ("git reset", &["git", "reset"], TREE_REASON),
            ("git checkout --", &["git", "checkout", "--"], TREE_REASON),
            ("git stash", &["git", "stash"], TREE_REASON),
            ("git clean", &["git", "clean"], TREE_REASON),
            ("git add -A", &["git", "add", "-A"], TREE_REASON),
            ("git update-ref", &["git", "update-ref"], TREE_REASON),
            ("git commit-tree", &["git", "commit-tree"], TREE_REASON),
            ("git read-tree", &["git", "read-tree"], TREE_REASON),
            ("git update-index", &["git", "update-index"], TREE_REASON),
            ("git replace", &["git", "replace"], TREE_REASON),
            ("git filter-branch", &["git", "filter-branch"], TREE_REASON),
            ("git filter-repo", &["git", "filter-repo"], TREE_REASON),
            (
                "git reflog expire",
                &["git", "reflog", "expire"],
                TREE_REASON,
            ),
            (
                "git reflog delete",
                &["git", "reflog", "delete"],
                TREE_REASON,
            ),
            ("git symbolic-ref", &["git", "symbolic-ref"], TREE_REASON),
            (
                "git pull --rebase",
                &["git", "pull", "--rebase"],
                TREE_REASON,
            ),
            ("git pull -r", &["git", "pull", "-r"], TREE_REASON),
            ("git commit", &["git", "commit"], READ_ONLY_REASON),
            ("git add", &["git", "add"], READ_ONLY_REASON),
        ];
        assert_eq!(COMMANDS, expected);
    }

    /// Reading history and amending a commit are what a writing step is
    /// for, so the entries that move history must not catch them: the
    /// overlay denies only the entries the step's list names, and a
    /// `git commit` entry would deny `git commit --amend` because
    /// matching is a word prefix. The two `reflog` entries are `expire`
    /// and `delete` rather than the bare word for the same reason: a
    /// reflog stays readable. `git write-tree` writes a loose object and
    /// moves no ref, so it names no entry.
    #[test]
    fn t19_the_history_reading_and_amending_forms_stay_unlisted() {
        // `COMMANDS` minus the read-only rows: the two entries a reading
        // step carries instead of `git rebase` and `git add -A`.
        let deny: Vec<&str> = COMMANDS
            .iter()
            .filter(|(_, _, reason)| *reason != READ_ONLY_REASON)
            .map(|(label, _, _)| *label)
            .collect();
        let overlay = policy_with(&deny);
        for line in [
            "git write-tree",
            "git write-tree --missing-ok",
            "git commit --amend --no-edit",
            "git log --oneline -3",
            "git status --short",
            "git reflog show",
            "git reflog --all",
            "git diff --stat",
            "git show --stat",
            "git fetch --prune",
            "git rev-parse HEAD",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
        for line in [
            "git symbolic-ref HEAD refs/heads/main",
            "git symbolic-ref -q HEAD",
        ] {
            assert_eq!(
                denied(&overlay, &bash(line)),
                "step deny git symbolic-ref",
                "{line}"
            );
        }
    }

    /// A scratch index hides the tree the runner checks: a step that
    /// commits through `GIT_INDEX_FILE` moves refs over an index no one
    /// else sees. The rule is the overlay's own, so it fires whatever
    /// the step's list names, and it reads the raw words: the assignment
    /// counts where git takes an argument and on a later segment of the
    /// line, both of them the fail-closed direction.
    #[test]
    fn t17_a_scratch_index_in_front_of_a_git_command_is_refused() {
        let blob = "0f1e2d3c4b5a69788796a5b4c3d2e1f00a1b2c3d";
        let tree = "1234567890abcdef1234567890abcdef12345678";
        let commit = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let overlay = policy_with(&["git push"]);
        for line in [
            "GIT_INDEX_FILE=/tmp/idx git read-tree HEAD".to_owned(),
            "GIT_INDEX_FILE=/tmp/idx git status".to_owned(),
            "GIT_INDEX_FILE=\"/tmp/idx\" git read-tree HEAD".to_owned(),
            "GIT_INDEX_FILE=/tmp/idx cat x; git status".to_owned(),
            "export GIT_INDEX_FILE=/tmp/idx; git read-tree HEAD".to_owned(),
            "env GIT_INDEX_FILE=/tmp/idx git log --oneline -3".to_owned(),
            "sh -c 'GIT_INDEX_FILE=/tmp/idx git commit-tree HEAD^{tree}'".to_owned(),
            format!("GIT_INDEX_FILE=/tmp/idx ./git update-index --cacheinfo 100644,{blob},x"),
            format!(
                "export GIT_INDEX_FILE=/tmp/idx; git read-tree HEAD; \
                 git update-index --cacheinfo 100644,{blob},x; git write-tree; \
                 git commit-tree {tree} -m x; git update-ref refs/heads/main {commit}"
            ),
        ] {
            assert_eq!(
                denied(&overlay, &bash(&line)),
                format!("step deny {INDEX}"),
                "{line}"
            );
        }
        // Nothing here runs git, or the assignment follows the line's
        // last git command: the flag alone changes nothing.
        for line in [
            "export GIT_INDEX_FILE=/tmp/idx",
            "rm -f /tmp/idx",
            "cat GIT_INDEX_FILE=x",
            "git status --short; GIT_INDEX_FILE=/tmp/idx cat x",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
        // Where git takes an argument that looks like an assignment, the
        // reader cannot tell the two apart, so it refuses both. The
        // overlay says nothing about `git log`, so the rule is the
        // overlay's own and not a deny-list entry.
        assert_eq!(
            denied(
                &policy_with(&["git push"]),
                &bash(&format!("git log GIT_INDEX_FILE={blob}"))
            ),
            format!("step deny {INDEX}")
        );
    }

    /// The rebase spelled as pull's own option: the reader rewrites the
    /// segment, so the entry refuses it and a list that names no pull
    /// entry refuses nothing.
    #[test]
    fn t18_a_pull_rebase_spelled_as_an_option_reads_as_the_entry() {
        let overlay = policy_with(&["git pull --rebase"]);
        for line in [
            "git -c pull.rebase=true pull",
            "git -c pull.rebase=merges pull",
            "git -c pull.rebase=interactive pull",
            "git -c pull.rebase pull",
            "git pull -c pull.rebase=true",
            "git pull --rebase=true",
            "git pull --rebase=merges",
            "git pull -r=true",
            "cd /repo && git -c pull.rebase=1 pull",
        ] {
            assert_eq!(
                denied(&overlay, &bash(line)),
                "step deny git pull --rebase",
                "{line}"
            );
        }
        for line in [
            "git pull",
            "git fetch",
            "git -c pull.rebase=false pull",
            "git -c pull.rebase=off pull",
            "git -c pull.rebase=0 pull",
            "git pull --rebase=false",
            "git pull -r=false",
            "git pull --no-rebase",
            "git log -c pull.rebase=true",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
        assert_eq!(
            policy_with(&["git push"]).decide(&bash("git -c pull.rebase=true pull")),
            None
        );
    }

    /// A 40-character commit id, as git writes one. The fold is armed
    /// with it and every other spelling below is derived from it, so no
    /// expectation is a hand-typed sha.
    const BASE: &str = "3f1a9c2b7d4e5081a6b3c9d2e5f704182a5b6c7d";

    /// A second commit id, the same length: the base a line must not name
    /// when the step is armed with [`BASE`].
    const OTHER: &str = "9c8b7a6f5e4d3c2b1a09f8e7d6c5b4a392817060";

    /// The overlay of `deny` with the fold armed at `base`.
    fn armed(deny: &[&str], base: &str) -> StepOverlay {
        policy_with(deny).with_restack_base(Some(base.to_owned()))
    }

    /// The issue's escape is exactly one spelling, and every form the
    /// entry must keep catching is refused as the entry: a harder reset,
    /// another commit, a word that names no commit, an unresolved or a
    /// quoted word, the value welded to the option, a separator, a second
    /// option, a fifth word. None of them is a spelling the guard could
    /// misread as the fold, and none is `unreadable`.
    #[test]
    fn t20_the_sanctioned_soft_reset_is_allowed_and_every_other_form_is_refused() {
        let overlay = armed(&["git reset", "git stash", "git clean"], BASE);
        assert_eq!(
            overlay.decide(&bash(&format!("git reset --soft {BASE}"))),
            None
        );
        for line in [
            "git reset --hard".to_owned(),
            format!("git reset --soft {OTHER}"),
            "git reset --soft HEAD".to_owned(),
            format!("git reset --soft {BASE}~1"),
            "git reset --soft 0123456".to_owned(),
            "git reset --soft $(git rev-parse HEAD)".to_owned(),
            "git reset --soft \"$SHA\"".to_owned(),
            format!("git reset --soft={BASE}"),
            format!("git reset --soft -- {BASE}"),
            format!("git reset -q --soft {BASE}"),
            format!("git reset --soft {BASE} extra"),
            format!("git reset --soft \"{BASE} extra\""),
        ] {
            assert_eq!(
                denied(&overlay, &bash(&line)),
                "step deny git reset",
                "{line}"
            );
        }
    }

    /// The arm names one commit: the same line is allowed only when the
    /// armed base is the commit it names, so an unarmed overlay and a
    /// differently armed one both refuse it. A full sha either way and an
    /// abbreviation of seven hex digits and up name the commit; six is
    /// not a name for it.
    #[test]
    fn t21_the_sanction_is_armed_for_one_commit_and_only_when_armed() {
        let line = format!("git reset --soft {BASE}");
        assert_eq!(armed(&["git reset"], BASE).decide(&bash(&line)), None);
        assert_eq!(
            denied(&armed(&["git reset"], OTHER), &bash(&line)),
            "step deny git reset"
        );
        assert_eq!(
            denied(&policy_with(&["git reset"]), &bash(&line)),
            "step deny git reset"
        );
        // A sha read back from a caller may be upper case.
        assert_eq!(
            armed(&["git reset"], &BASE.to_uppercase()).decide(&bash(&line)),
            None
        );
        for len in BASE_ABBREV..=BASE.len() {
            let word = &BASE[..len];
            let line = format!("git reset --soft {word}");
            assert_eq!(
                armed(&["git reset"], BASE).decide(&bash(&line)),
                None,
                "{word}"
            );
        }
        let short = &BASE[..BASE_ABBREV - 1];
        let line = format!("git reset --soft {short}");
        assert_eq!(
            denied(&armed(&["git reset"], BASE), &bash(&line)),
            "step deny git reset",
            "{short}"
        );
    }

    /// The reader steps over `git`'s global options, so a reset aimed
    /// elsewhere would otherwise reach the carve-out spelled exactly like
    /// the sanctioned line: `git -C /elsewhere reset --soft <base>` reads
    /// as four words. Every spelling the reader accepts for every option
    /// the table names is refused, and an option that takes no value is
    /// not — the guard is the value table, not "any option in front".
    #[test]
    fn t22_a_git_global_option_on_the_line_refuses_the_sanction() {
        let overlay = armed(&["git reset"], BASE);
        let value = "/elsewhere";
        for opt in GIT_VALUES {
            let sep = if opt.starts_with("--") { "=" } else { "" };
            for line in [
                format!("git {opt} {value} reset --soft {BASE}"),
                format!("git {opt}{sep}{value} reset --soft {BASE}"),
            ] {
                assert_eq!(
                    denied(&overlay, &bash(&line)),
                    "step deny git reset",
                    "{line}"
                );
            }
        }
        assert_eq!(
            overlay.decide(&bash(&format!("git --no-pager reset --soft {BASE}"))),
            None
        );
    }

    /// The carve-out is one segment, and only while it is the line's only
    /// reset: a second segment is read by its own entries, a second reset
    /// on the line makes both the entry, and the same spelling carrying a
    /// redirection is not the sanctioned line, because the effective
    /// words cannot see a target. Each refusal names the entry that
    /// caught it. The scratch-index guard runs before any entry, so it
    /// names its own rule whatever the step is armed with.
    #[test]
    fn t23_another_segment_on_the_line_still_denies() {
        let overlay = armed(&["git reset", "git stash", "git clean"], BASE);
        let fold = format!("git reset --soft {BASE}");
        let cases: Vec<(String, &str)> = vec![
            (format!("{fold} && git reset --hard"), "git reset"),
            (format!("{fold} && {fold}"), "git reset"),
            (format!("{fold}; git stash"), "git stash"),
            (format!("{fold}; git clean -fd"), "git clean"),
            (format!("{fold} > /tmp/out"), "git reset"),
            (format!("{fold} 2>/dev/null"), "git reset"),
        ];
        for (line, entry) in cases {
            assert_eq!(
                denied(&overlay, &bash(&line)),
                format!("step deny {entry}"),
                "{line}"
            );
        }
        assert_eq!(
            denied(
                &overlay,
                &bash(&format!("GIT_INDEX_FILE=/tmp/i git reset --soft {BASE}"))
            ),
            "step deny GIT_INDEX_FILE"
        );
    }

    /// One row per form the module doc's limits paragraph names, quoted
    /// here as the rows' source:
    ///
    /// > Stated limits: a command reached through `xargs`, `find -exec`,
    /// > `eval` or `python -c`; a wrapper with no row in `FORWARDERS`
    /// > (`doas`, `setsid`, `run0`, `ionice`, `chrt`, `flock`,
    /// > `unbuffer`, `script`) or a multi-call binary that chooses its
    /// > command from its own words (`busybox git push`); `env -S <line>`,
    /// > whose value is a command line the reader does not split; a
    /// > here-document body fed to a shell (`bash <<EOF` with the command
    /// > inside); `PATH` shadowing, where the reader reads the word `git`
    /// > and never the program it resolves to; a variable set in an
    /// > earlier call, which is not on the line the reader reads (`export
    /// > GIT_INDEX_FILE=/tmp/idx` alone, then `git read-tree HEAD` in the
    /// > next call); and a word that looks like an assignment where git
    /// > takes an argument, refused along with the real thing (`git log
    /// > GIT_INDEX_FILE=<blob>`).
    ///
    /// A form that stays outside the guard reads as `None`; the `PATH`
    /// row denies because the reader reads the word and never the program
    /// it resolves to. A change that closes one of these forms fails a row,
    /// which is where the paragraph is updated with it. The paragraph's
    /// clause about a word that mixes literal text with a substitution is
    /// pinned by `t4`.
    #[test]
    fn t16_the_stated_limits_still_escape_the_overlay() {
        let overlay = policy_with(&["git push"]);
        for line in [
            "xargs git push",
            "find . -exec git push \\;",
            "eval 'git push'",
            "python3 -c 'print(1)'",
            "busybox git push",
            "doas git push",
            "setsid git push",
            "run0 git push",
            "ionice git push",
            "chrt 5 git push",
            "flock /tmp/lock git push",
            "unbuffer git push",
            "script -q /dev/null git push",
            "env -S 'git push'",
            "bash <<'EOF'\ngit push\nEOF",
            "export GIT_INDEX_FILE=/tmp/idx",
        ] {
            assert_eq!(overlay.decide(&bash(line)), None, "{line}");
        }
        assert_eq!(
            denied(&overlay, &bash("PATH=/tmp/bin:$PATH git push")),
            "step deny git push"
        );
    }
}
