//! `/copy [n|all]` (issue #41): the latest assistant reply's *source*
//! text to the system clipboard, never the rendered transcript. The
//! decorations the person saw — the `│` gutters, wrapping, colour — are
//! added by rendering (`markdown::line`, `look::render`), so copying
//! takes the text before rendering. Fence extraction follows CommonMark,
//! not `markdown::is_fence`'s "starts with ```".

use std::io::Write;
use std::process::{Command, Stdio};

/// The fenced code blocks of `text`, bodies verbatim, fence lines
/// excluded, in order. A fence is a run of three or more backticks or
/// tildes (leading whitespace before it, an info string after it); only
/// a run of the same character, at least as long, alone on its line,
/// closes it. A fence still open at the end of the text is dropped.
pub fn code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut open: Option<(char, usize)> = None;
    let mut body: Vec<String> = Vec::new();
    for line in text.split('\n') {
        // The newline separates rows; it is not part of the last row
        // before a closing fence, so a body is its lines joined by
        // newlines, with no trailing one. A CRLF's `\r` goes with the
        // line, so a `\r`-terminated fence still closes.
        let row = line.strip_suffix('\r').unwrap_or(line);
        match open {
            None => {
                if let Some(fence) = fence_open(row) {
                    open = Some(fence);
                    body.clear();
                }
            }
            Some((c, len)) => {
                if fence_close(row, c, len) {
                    blocks.push(body.join("\n"));
                    open = None;
                } else {
                    body.push(row.to_owned());
                }
            }
        }
    }
    blocks
}

/// A fence opener: leading whitespace, then a run of three or more
/// backticks or tildes, then anything (the info string).
fn fence_open(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = t.chars().take_while(|&ch| ch == c).count();
    (len >= 3).then_some((c, len))
}

/// A fence closer: a run of the same character, at least as long as the
/// opener, with nothing else on the line.
fn fence_close(line: &str, c: char, len: usize) -> bool {
    let t = line.trim();
    let run = t.chars().take_while(|&ch| ch == c).count();
    run >= len && t.trim_start_matches(c).trim().is_empty()
}

/// The OSC 52 sequence carrying `text` as base64: `ESC ] 52 ; c ; B64 BEL`.
/// The fallback transport, and the only one that works over SSH.
pub fn osc52(text: &str) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07")
}

/// One way to reach a clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A child process taking the text on stdin. The argv is pinned:
    /// `xclip` with no arguments writes the PRIMARY selection, not the
    /// clipboard, so Ctrl-V would paste something else.
    Child(&'static [&'static str]),
    /// The OSC 52 escape to this terminal: it reaches the person's own
    /// clipboard over SSH, where a child process would write the remote
    /// machine's. It cannot report failure, so it is also the last try.
    Osc52,
}

/// Which transport carried the text, so the confirmation can say when
/// the terminal may ignore it (OSC 52).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Used {
    Child,
    Osc52,
}

/// The order to try, as a pure function of where we run. Over SSH only
/// the escape reaches the person; locally the platform's tools come
/// first, the escape as the fallback.
pub fn transports(over_ssh: bool, macos: bool) -> Vec<Transport> {
    if over_ssh {
        return vec![Transport::Osc52];
    }
    if macos {
        vec![Transport::Child(&["pbcopy"]), Transport::Osc52]
    } else {
        vec![
            Transport::Child(&["wl-copy"]),
            Transport::Child(&["xclip", "-selection", "clipboard"]),
            Transport::Osc52,
        ]
    }
}

/// Run one clipboard tool with `text` on its stdin. `false` when the
/// program is missing or exits non-zero, so the caller tries the next.
fn run_clipboard_tool(argv: &[&str], text: &str) -> bool {
    let Some((prog, args)) = argv.split_first() else {
        return false;
    };
    let Ok(mut child) = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).is_err()
    {
        return false;
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

/// Put `text` on the clipboard. Over SSH that is OSC 52 alone; locally
/// the platform's tools first, the escape as the fallback.
pub fn set_clipboard(text: &str) -> Result<Used, String> {
    let over_ssh =
        std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some();
    let macos = cfg!(target_os = "macos");
    for transport in transports(over_ssh, macos) {
        match transport {
            Transport::Child(argv) => {
                if run_clipboard_tool(argv, text) {
                    return Ok(Used::Child);
                }
            }
            Transport::Osc52 => {
                let mut stdout = std::io::stdout();
                return stdout
                    .write_all(osc52(text).as_bytes())
                    .and_then(|()| stdout.flush())
                    .map(|()| Used::Osc52)
                    .map_err(|e| format!("OSC 52 write failed: {e}"));
            }
        }
    }
    Err("no clipboard tool found (pbcopy, wl-copy or xclip)".into())
}

/// What `/copy [n|all]` asks for and what the reply can answer.
///
/// The block bodies are extracted into their own strings (`code_blocks`
/// returns `Vec<String>`), not slices of the reply, so this owns its
/// text: a `CopyChoice<'a>` borrowing the reply cannot be returned from
/// `pick`, whose blocks are local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyChoice {
    /// The n-th (1-based) block, its text and line count.
    Block {
        n: usize,
        text: String,
        lines: usize,
    },
    /// The whole reply as plain markdown.
    All { text: String, lines: usize },
    /// No assistant text at all in this session.
    NoReply,
    /// A reply, but no fenced code blocks in it.
    NoBlocks,
    /// A block number that is not one of them.
    OutOfRange { n: usize, total: usize },
    /// An argument that is neither "all" nor a number.
    NotANumber,
}

/// Answer a `/copy` argument against the latest reply. No argument
/// copies the last block; `"all"` the whole reply; a number the n-th
/// block, `OutOfRange` when it is 0 or past the count.
pub fn pick(reply: &str, arg: Option<&str>) -> CopyChoice {
    if reply.is_empty() {
        return CopyChoice::NoReply;
    }
    if arg == Some("all") {
        return CopyChoice::All {
            text: reply.to_owned(),
            lines: reply.lines().count(),
        };
    }
    let blocks = code_blocks(reply);
    let number = match arg {
        None => return last_block(blocks),
        Some(a) => a,
    };
    if blocks.is_empty() {
        return CopyChoice::NoBlocks;
    }
    match number.parse::<usize>() {
        Ok(n) if n >= 1 && n <= blocks.len() => {
            let text = blocks[n - 1].clone();
            CopyChoice::Block {
                n,
                lines: text.lines().count(),
                text,
            }
        }
        Ok(n) => CopyChoice::OutOfRange {
            n,
            total: blocks.len(),
        },
        Err(_) => CopyChoice::NotANumber,
    }
}

fn last_block(blocks: Vec<String>) -> CopyChoice {
    let total = blocks.len();
    match blocks.into_iter().next_back() {
        Some(text) => CopyChoice::Block {
            n: total,
            lines: text.lines().count(),
            text,
        },
        None => CopyChoice::NoBlocks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_blocks_return_each_block_verbatim() {
        // The three bodies are defined once and formatted in, so the
        // expectation is the source text, never a re-typed literal.
        let one = "let a = 1;\nlet b = 2;";
        let two = "print(\"hi\")";
        let three = "SELECT 1;";
        let reply = format!(
            "First:\n\n```rust\n{one}\n```\n\nSome prose between them.\n\n\
             ```python\n{two}\n```\n\nLast:\n\n```sql\n{three}\n```\n"
        );
        assert_eq!(
            code_blocks(&reply),
            vec![one.to_owned(), two.to_owned(), three.to_owned()]
        );
    }

    #[test]
    fn code_blocks_fences_follow_commonmark() {
        // A ```` block whose body is a ``` block, copied verbatim; a
        // tilde block after it; and an unclosed fence at the end, which
        // is dropped.
        let inner = "```rust\nlet x = 1;\n```";
        let tilde = "echo hi";
        let reply = format!(
            "````markdown\n{inner}\n````\n\n~~~bash\n{tilde}\n~~~\n\n```sh\necho unfinished\n"
        );
        assert_eq!(
            code_blocks(&reply),
            vec![inner.to_owned(), tilde.to_owned()]
        );
    }

    #[test]
    fn osc52_round_trips_through_base64() {
        use base64::Engine as _;
        let text = "hello\nworld ✓ — done";
        let framed = osc52(text);
        let prefix = "\x1b]52;c;";
        let suffix = "\x07";
        assert!(framed.starts_with(prefix), "{framed:?}");
        assert!(framed.ends_with(suffix), "{framed:?}");
        let payload = &framed[prefix.len()..framed.len() - suffix.len()];
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .unwrap();
        assert_eq!(String::from_utf8(decoded).unwrap(), text);
    }

    #[test]
    fn clipboard_order_prefers_osc52_over_ssh() {
        // The expected orders are built from the rule, once: over SSH
        // the escape alone; locally the platform's tools then the
        // escape.
        let osc = Transport::Osc52;
        let ssh = vec![osc];
        let macos = vec![Transport::Child(&["pbcopy"]), osc];
        let linux = vec![
            Transport::Child(&["wl-copy"]),
            Transport::Child(&["xclip", "-selection", "clipboard"]),
            osc,
        ];
        assert_eq!(transports(true, true), ssh);
        assert_eq!(transports(true, false), ssh);
        assert_eq!(transports(false, true), macos);
        assert_eq!(transports(false, false), linux);
    }

    #[test]
    fn pick_answers_out_of_range_with_the_block_count() {
        let one = "one";
        let two = "two";
        let three = "three";
        let reply = format!("```\n{one}\n```\n\n```\n{two}\n```\n\n```\n{three}\n```\n");
        assert_eq!(
            pick(&reply, Some("4")),
            CopyChoice::OutOfRange { n: 4, total: 3 }
        );
        assert_eq!(
            pick(&reply, Some("0")),
            CopyChoice::OutOfRange { n: 0, total: 3 }
        );
        assert_eq!(pick("", None), CopyChoice::NoReply);
        assert_eq!(pick("no code here", None), CopyChoice::NoBlocks);
        assert_eq!(pick(&reply, Some("x")), CopyChoice::NotANumber);
        assert_eq!(
            pick(&reply, Some("2")),
            CopyChoice::Block {
                n: 2,
                text: two.to_owned(),
                lines: 1
            }
        );
        assert_eq!(
            pick(&reply, None),
            CopyChoice::Block {
                n: 3,
                text: three.to_owned(),
                lines: 1
            }
        );
        assert_eq!(
            pick(&reply, Some("all")),
            CopyChoice::All {
                text: reply.clone(),
                lines: reply.lines().count()
            }
        );
    }
}
