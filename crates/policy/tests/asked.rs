//! Every bash command the harness has ever asked about (343 at the
//! time of the snapshot), each with the classification it now gets:
//! the read-only ones run without asking (issue #15), the rest still
//! ask — and ask once, as a chain, not as one prompt per segment.
//!
//! The commands come from the `permission_requested` events in the
//! thread logs, with `~` for the home directory and `\n` for a real
//! newline; the verdicts are one per line, in the same order: `allow`
//! or `ask`, with the words a "don't ask again" answer would grant
//! after the `ask` (`-` when a redirection carries the risk, and no
//! command prefix covers that).

use aigentic_core::{RiskClass, ToolCall};
use aigentic_policy::{Outcome, Policy};
use serde_json::json;

fn read(name: &str) -> Vec<String> {
    let path = format!("tests/fixtures/{name}");
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read {name}: {e}"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// `\\` back to `\`, `\n` back to a newline.
fn unescape(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn bash(command: &str) -> ToolCall {
    ToolCall {
        id: "c1".into(),
        name: "bash".into(),
        args: json!({ "command": command }),
    }
}

#[test]
fn the_asked_fixture_classifies_as_recorded() {
    let commands = read("asked-2026-09-23.txt");
    let verdicts = read("asked-2026-09-23.verdicts");
    assert_eq!(
        commands.len(),
        343,
        "the snapshot has 343 commands; re-take it deliberately"
    );
    assert_eq!(
        commands.len(),
        verdicts.len(),
        "one verdict per command, in order"
    );

    let policy = Policy::configured(Vec::new(), None);
    let mut allowed = 0;
    for (i, line) in commands.iter().enumerate() {
        let outcome = policy.decide(&bash(&unescape(line)), RiskClass::Exec);
        let verdict = verdicts[i].as_str();
        let case = format!("#{}: {line}", i + 1);
        match verdict.strip_prefix("ask") {
            Some(_) => assert!(
                !matches!(outcome, Outcome::Allow { .. }),
                "should ask — {case}"
            ),
            None => {
                assert_eq!(verdict, "allow", "allow or `ask <words>` — {case}");
                allowed += 1;
            }
        }
    }
    assert_eq!(allowed, 275, "275 of the 343 run without asking");
}

/// A chain that is not all read-only asks once, through the exec
/// rule, with the command whole: the approver sees one prompt, not
/// one per segment.
#[test]
fn a_chain_that_is_not_read_only_asks_once() {
    let policy = Policy::configured(Vec::new(), None);
    let Outcome::Ask { reason } = policy.decide(&bash("cargo test && git push"), RiskClass::Exec)
    else {
        panic!("a chain with git push asks");
    };
    assert_eq!(reason, "class exec: anything else in a shell");
}
