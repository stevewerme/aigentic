//! The prompt menu (issue #13): the selectable list a permission request
//! and an `ask_human` question render as, in the style of Claude Code's
//! menus. One widget, two uses: the shell draws it above the composer
//! and feeds it keys, a pipe prints the rows numbered and reads a number
//! or text. The answering keys wait for the shell's `PROMPT_GRACE`.

use aigentic_runtime::aigentic_core::{RiskClass, ToolCall};
use aigentic_runtime::aigentic_policy::prefix_of;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::commands::truncate_for_display;

/// What picking a row does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Allow the call: once, for the session, or a prefix from now on.
    Allow {
        session: bool,
        prefix: Option<Vec<String>>,
    },
    /// Deny the call, with a reason when one follows.
    Deny,
    /// Answer the current question with this row's label.
    Answer,
    /// Free text for the current question: the composer takes it.
    Other,
}

/// One row of the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// A dim line after the label (an option's description).
    pub desc: Option<String>,
    pub pick: Pick,
}

/// What kind of prompt the menu is, for the plain tag and the keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
}

/// What a key or a line came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Keyed {
    /// Not for the menu; the caller carries on.
    Passed,
    /// Handled, nothing to send yet (the selection moved).
    Used,
    /// A decision ready to send, with the line to echo into the
    /// transcript so the record reads cleanly.
    Decide {
        pick: Pick,
        reason: Option<String>,
        echo: String,
    },
    /// An answer to the current question, its contribution to the
    /// composed answer, and the echo of it.
    Answer { text: String, echo: String },
    /// The composer should take the prompt's text: a deny's reason
    /// (Esc does the same) or a question's free text.
    Text,
}

/// An `ask_human` call's questions: the one being answered and the
/// answers so far, one contribution each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Questions {
    pub all: Vec<aigentic_api::AskedQuestion>,
    pub at: usize,
    /// The contribution per answered question, `header: answer` lines.
    pub answers: Vec<String>,
}

/// The prompt as a menu: what is asked, the rows to pick from and the
/// selection. The engine builds one when the thread waits on this user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Menu {
    pub kind: Kind,
    /// The plain-words header: "Run this command?".
    pub title: String,
    /// What is asked about (the command), in full: the menu wraps it.
    pub body: String,
    /// A dim line under the options (the reason, when it says something
    /// the header does not).
    pub note: Option<String>,
    /// The rows to pick from; a question without options has none and
    /// the composer takes the answer.
    pub rows: Vec<Row>,
    /// The selected row.
    pub selected: usize,
    /// Multi-select: Space toggles and Enter takes the picked rows.
    pub multi: bool,
    /// Which rows are picked, in multi mode.
    pub picked: Vec<bool>,
    /// The questions, while the menu is a question's.
    pub questions: Option<Questions>,
}

/// The header in plain words, per risk class; an MCP tool by name.
fn title(tool: &str, class: RiskClass) -> String {
    if tool.starts_with("mcp.") {
        "Call this MCP tool?".into()
    } else {
        match class {
            RiskClass::Exec => "Run this command?",
            RiskClass::Write => "Edit this file?",
            RiskClass::Network => "Use the network?",
            RiskClass::Read => "Read this?",
            RiskClass::Safe => "Allow this?",
        }
        .into()
    }
}

/// The argument the body shows: the command, the path, the pattern; the
/// whole args as JSON for anything else.
pub fn main_arg(call: &ToolCall) -> String {
    let key = match call.name.as_str() {
        "bash" => "command",
        "read_file" | "write_file" | "edit_file" | "list_dir" => "path",
        "grep" | "search_knowledge" => "pattern",
        _ => return call.args.to_string(),
    };
    call.args
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| call.args.to_string())
}

impl Menu {
    /// The approval menu for a call the rules ask about: plain words for
    /// what is asked, the full command under it, and one "allow more"
    /// row — a prefix from now on when `prefix_of` gives one, else a
    /// session grant (for `bash`, the exact command; the issue's
    /// correction of 2026-09-24).
    pub fn permission(call: &ToolCall, class: RiskClass, reason: &str) -> Self {
        let prefix = (call.name == "bash")
            .then(|| {
                call.args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .map(prefix_of)
            })
            .flatten()
            .filter(|p| !p.is_empty());
        let more = match &prefix {
            Some(p) => format!(
                "Yes, and don't ask again for `{}` in this project",
                p.join(" ")
            ),
            None if call.name == "bash" => {
                "Yes, and don't ask again for this exact command this session".into()
            }
            None => format!("Yes, and don't ask again for `{}` this session", call.name),
        };
        let more_pick = match &prefix {
            Some(p) => Pick::Allow {
                session: false,
                prefix: Some(p.clone()),
            },
            None => Pick::Allow {
                session: true,
                prefix: None,
            },
        };
        let body = if call.name == "bash" {
            main_arg(call)
        } else {
            format!("{} {}", call.name, main_arg(call))
        };
        Self {
            kind: Kind::Permission,
            title: title(&call.name, class),
            body: truncate_for_display(&body, 40, 4000),
            // The class-generated reasons say what the title already
            // does; a rule's own words are worth their line.
            note: (!reason.starts_with("class ")).then(|| reason.to_owned()),
            rows: vec![
                Row {
                    label: "Yes".into(),
                    desc: None,
                    pick: Pick::Allow {
                        session: false,
                        prefix: None,
                    },
                },
                Row {
                    label: more,
                    desc: None,
                    pick: more_pick,
                },
                Row {
                    label: "No, and tell the agent why".into(),
                    desc: None,
                    pick: Pick::Deny,
                },
            ],
            selected: 0,
            multi: false,
            picked: Vec::new(),
            questions: None,
        }
    }

    /// A question from `ask_human`: the first of the call's questions
    /// on the widget, the rest one after another as each is answered.
    /// A question without options has no rows: the composer takes the
    /// answer. An old daemon's frame has no questions, so the engine
    /// wraps its plain text as one.
    pub fn asking(all: Vec<aigentic_api::AskedQuestion>) -> Self {
        let mut menu = Self {
            kind: Kind::Question,
            title: String::new(),
            body: String::new(),
            note: None,
            rows: Vec::new(),
            selected: 0,
            multi: false,
            picked: Vec::new(),
            questions: Some(Questions {
                all,
                at: 0,
                answers: Vec::new(),
            }),
        };
        let first = menu
            .questions
            .as_ref()
            .and_then(|q| q.all.first().cloned())
            .unwrap_or(aigentic_api::AskedQuestion {
                question: String::new(),
                header: None,
                options: Vec::new(),
                multi: false,
            });
        menu.show(&first);
        menu
    }

    /// Render `q` as the current question.
    fn show(&mut self, q: &aigentic_api::AskedQuestion) {
        self.title = q.question.clone();
        self.multi = q.multi;
        self.selected = 0;
        self.picked = vec![false; q.options.len() + 1];
        self.rows = q
            .options
            .iter()
            .map(|o| Row {
                label: o.label.clone(),
                desc: o.description.clone(),
                pick: Pick::Answer,
            })
            .collect();
        if !q.options.is_empty() {
            self.rows.push(Row {
                label: "Other: type your own".into(),
                desc: None,
                pick: Pick::Other,
            });
        }
        self.note = if q.multi && !q.options.is_empty() {
            Some("space toggles · enter sends · a pipe: `1 2`".to_owned())
        } else if q.options.is_empty() {
            Some("type the answer".to_owned())
        } else {
            None
        };
    }

    /// The answer's line for the current question: `header: answer`
    /// when it has one, the answer alone when not.
    fn line_of(&self, answer: &str) -> String {
        let header = self
            .questions
            .as_ref()
            .and_then(|q| q.all.get(q.at))
            .and_then(|a| a.header.clone());
        match header {
            Some(h) => format!("{h}: {answer}"),
            None => answer.to_owned(),
        }
    }

    /// A free-text answer to the current question, headed like any
    /// other.
    pub fn free_text(&self, text: &str) -> String {
        self.line_of(text)
    }

    /// Record `contribution` as the current question's answer and move
    /// to the next; `Some` with the whole answer when that was the
    /// last, for the `AnswerHuman` request.
    pub fn answer(&mut self, contribution: &str) -> Option<String> {
        let q = self.questions.as_mut()?;
        q.answers.push(contribution.to_owned());
        q.at += 1;
        match q.all.get(q.at) {
            Some(next) => {
                let next = next.clone();
                self.show(&next);
                None
            }
            None => Some(q.answers.join("\n")),
        }
    }

    /// A key for the menu. Navigation is always the menu's (arrows
    /// never type); the answering keys wait for `settled`, the shell's
    /// grace, so keys meant for the composer do not answer the prompt.
    /// Enter picks the selected row only on an empty composer: a draft
    /// is a message, and an accidental Yes is the one wrong answer. A
    /// question's composer is its free-text answer, so a question's
    /// answering keys want an empty composer too.
    pub fn key(&mut self, key: &KeyEvent, composer_empty: bool, settled: bool) -> Keyed {
        let plain = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        if self.rows.is_empty() {
            // A question without options: the composer answers.
            return Keyed::Passed;
        }
        let typing_safe = self.kind == Kind::Permission || composer_empty;
        match key.code {
            KeyCode::Up => {
                self.select(self.selected + self.rows.len() - 1);
                Keyed::Used
            }
            KeyCode::Down => {
                self.select(self.selected + 1);
                Keyed::Used
            }
            // Esc opens the reason input on the deny row, at once.
            KeyCode::Esc if self.kind == Kind::Permission => Keyed::Text,
            _ if !settled => Keyed::Passed,
            KeyCode::Char(' ') if plain && self.multi && typing_safe => {
                self.toggle(self.selected);
                Keyed::Used
            }
            KeyCode::Enter if composer_empty => self.enter(),
            KeyCode::Char(c) if plain && c.is_ascii_digit() => match c.to_digit(10) {
                Some(n) if n >= 1 && (n as usize) <= self.rows.len() => {
                    let i = n as usize - 1;
                    if self.multi {
                        if !typing_safe {
                            Keyed::Passed
                        } else {
                            self.toggle(i);
                            Keyed::Used
                        }
                    } else if !typing_safe {
                        Keyed::Passed
                    } else {
                        self.pick(i)
                    }
                }
                _ => Keyed::Passed,
            },
            // The hidden accelerators, a permission only.
            KeyCode::Char(c) if plain && self.kind == Kind::Permission => match c {
                'y' => self.pick(0),
                'a' | 'p' => self.pick(1),
                'n' => Keyed::Decide {
                    pick: Pick::Deny,
                    reason: None,
                    echo: "↳ No".into(),
                },
                _ => Keyed::Passed,
            },
            _ => Keyed::Passed,
        }
    }

    /// Enter: single-select picks the selected row; multi submits the
    /// picked ones, or the selected row when none are.
    fn enter(&self) -> Keyed {
        if !self.multi {
            return self.pick(self.selected);
        }
        let labels: Vec<&str> = self
            .rows
            .iter()
            .zip(&self.picked)
            .filter(|(r, p)| **p && r.pick == Pick::Answer)
            .map(|(r, _)| r.label.as_str())
            .collect();
        if labels.is_empty() {
            // Nothing picked: the selected row — Other hands the
            // composer the free text.
            return self.pick(self.selected);
        }
        let answer = labels.join(", ");
        let text = self.line_of(&answer);
        Keyed::Answer {
            echo: format!("↳ {text}"),
            text,
        }
    }

    /// Toggle a row in multi mode; Other is free text, not a choice.
    fn toggle(&mut self, i: usize) {
        if self.rows[i].pick == Pick::Answer {
            self.picked[i] = !self.picked[i];
        }
    }

    /// Move the selection, wrapping.
    fn select(&mut self, i: usize) {
        if !self.rows.is_empty() {
            self.selected = i % self.rows.len();
        }
    }

    /// Row `i` picked, single-select: what it does and the echo of it.
    fn pick(&self, i: usize) -> Keyed {
        let row = &self.rows[i];
        match &row.pick {
            Pick::Allow { session, prefix } => Keyed::Decide {
                pick: Pick::Allow {
                    session: *session,
                    prefix: prefix.clone(),
                },
                reason: None,
                echo: format!("↳ {}", row.label),
            },
            Pick::Deny => Keyed::Decide {
                pick: Pick::Deny,
                reason: None,
                echo: format!("↳ {}", row.label),
            },
            Pick::Answer => {
                let text = self.line_of(&row.label);
                Keyed::Answer {
                    echo: format!("↳ {text}"),
                    text,
                }
            }
            Pick::Other => Keyed::Text,
        }
    }

    /// A plain line as an answer: a number picks that row, the old
    /// permission letters still work, a reason may follow a deny, and
    /// for a question anything else is free text. `None` when the line
    /// is not an answer, so it goes on as whatever it was.
    pub fn line(&self, text: &str) -> Option<Keyed> {
        if self.kind == Kind::Question {
            return Some(self.question_line(text.trim()));
        }
        if self.rows.is_empty() {
            return None;
        }
        let t = text.trim();
        if t.is_empty() {
            return None;
        }
        let (head, rest) = t
            .split_once(char::is_whitespace)
            .map_or((t, ""), |(h, r)| (h, r.trim()));
        let reason = (!rest.is_empty()).then(|| rest.to_owned());
        let deny = |reason: &Option<String>| Keyed::Decide {
            pick: Pick::Deny,
            reason: reason.clone(),
            echo: match reason {
                Some(r) => format!("↳ No: {r}"),
                None => "↳ No".into(),
            },
        };
        Some(match head.to_ascii_lowercase().as_str() {
            "1" | "y" | "yes" => self.pick(0),
            "2" | "a" | "always" | "p" => self.pick(1),
            "3" | "n" | "no" => deny(&reason),
            _ => return None,
        })
    }

    /// A line for a question: numbers pick rows (several, space or
    /// comma separated, when the question allows it) and anything else
    /// is free text; the old single-question shape takes every line.
    fn question_line(&self, t: &str) -> Keyed {
        let numbers: Option<Vec<usize>> = t
            .split([',', ' '])
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<usize>()
                    .ok()
                    .filter(|&n| n >= 1 && n <= self.rows.len())
            })
            .collect();
        match numbers {
            Some(ns) if !ns.is_empty() && self.multi && ns.len() > 1 => {
                let labels: Vec<&str> = ns
                    .iter()
                    .map(|&i| self.rows[i - 1].label.as_str())
                    .collect();
                let answer = labels.join(", ");
                let text = self.line_of(&answer);
                Keyed::Answer {
                    echo: format!("↳ {text}"),
                    text,
                }
            }
            Some(ns) if ns.len() == 1 && !self.rows.is_empty() => self.pick(ns[0] - 1),
            _ => {
                // Free text; an empty line answers with an empty
                // line, as it always did.
                let text = if t.is_empty() {
                    String::new()
                } else {
                    self.line_of(t)
                };
                Keyed::Answer {
                    echo: format!("↳ {text}"),
                    text,
                }
            }
        }
    }

    /// Plain lines, for a pipe: the options numbered, read a number or
    /// text.
    pub fn plain(&self) -> Vec<String> {
        let tag = match self.kind {
            Kind::Permission => "permission",
            Kind::Question => "question",
        };
        let mut lines = vec![format!("[{tag}] {}", self.title)];
        if !self.body.is_empty() {
            for l in truncate_for_display(&self.body, 8, 2000).lines() {
                lines.push(format!("  {l}"));
            }
        }
        for (i, row) in self.rows.iter().enumerate() {
            let desc = row
                .desc
                .as_ref()
                .map(|d| format!(" · {d}"))
                .unwrap_or_default();
            lines.push(format!("  {}. {}{}", i + 1, row.label, desc));
        }
        if let Some(note) = &self.note {
            lines.push(format!("  {note}"));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: json!({"command": command}),
        }
    }

    fn approval() -> Menu {
        // Compound, so `prefix_of` gives nothing and row two is the
        // session one.
        Menu::permission(
            &bash("echo hi && echo bye"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        )
    }

    fn plain_question(text: &str) -> Menu {
        Menu::asking(vec![aigentic_api::AskedQuestion {
            question: text.into(),
            header: None,
            options: vec![],
            multi: false,
        }])
    }

    fn asked() -> Vec<aigentic_api::AskedQuestion> {
        vec![
            aigentic_api::AskedQuestion {
                question: "Which colour?".into(),
                header: Some("colour".into()),
                options: vec![
                    aigentic_api::AskedOption {
                        label: "Red".into(),
                        description: Some("the warm one".into()),
                    },
                    aigentic_api::AskedOption {
                        label: "Green".into(),
                        description: None,
                    },
                ],
                multi: false,
            },
            aigentic_api::AskedQuestion {
                question: "Which tests?".into(),
                header: None,
                options: vec![
                    aigentic_api::AskedOption {
                        label: "unit".into(),
                        description: None,
                    },
                    aigentic_api::AskedOption {
                        label: "integration".into(),
                        description: None,
                    },
                ],
                multi: true,
            },
            aigentic_api::AskedQuestion {
                question: "Ship it?".into(),
                header: None,
                options: vec![],
                multi: false,
            },
        ]
    }

    fn answer(text: &str) -> Keyed {
        Keyed::Answer {
            text: text.into(),
            echo: format!("\u{21b3} {text}"),
        }
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn once() -> Keyed {
        Keyed::Decide {
            pick: Pick::Allow {
                session: false,
                prefix: None,
            },
            reason: None,
            echo: "↳ Yes".into(),
        }
    }

    fn more() -> Keyed {
        Keyed::Decide {
            pick: Pick::Allow {
                session: true,
                prefix: None,
            },
            reason: None,
            echo: "↳ Yes, and don't ask again for this exact command this session".into(),
        }
    }

    fn no(reason: Option<&str>) -> Keyed {
        Keyed::Decide {
            pick: Pick::Deny,
            reason: reason.map(str::to_owned),
            echo: match reason {
                Some(r) => format!("↳ No: {r}"),
                None => "↳ No".into(),
            },
        }
    }

    /// Arrows and Enter pick, digits pick, and Enter waits for an empty
    /// composer: a draft is a message, and an accidental Yes is the one
    /// wrong answer.
    #[test]
    fn arrows_enter_and_digits_pick_rows() {
        let mut menu = approval();
        assert_eq!(
            menu.key(&key(KeyCode::Down, KeyModifiers::NONE), true, true),
            Keyed::Used
        );
        assert_eq!(menu.selected, 1);
        assert_eq!(
            menu.key(&key(KeyCode::Up, KeyModifiers::NONE), true, true),
            Keyed::Used
        );
        assert_eq!(menu.selected, 0);
        assert_eq!(
            menu.key(&key(KeyCode::Enter, KeyModifiers::NONE), true, true),
            once()
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('2'), KeyModifiers::NONE), true, true),
            more()
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('2'), KeyModifiers::NONE), true, true),
            more()
        );
        // A digit past the rows, or Enter over a draft, is not the
        // menu's.
        assert_eq!(
            menu.key(&key(KeyCode::Char('9'), KeyModifiers::NONE), true, true),
            Keyed::Passed
        );
        assert_eq!(
            menu.key(&key(KeyCode::Enter, KeyModifiers::NONE), false, true),
            Keyed::Passed
        );
        assert_eq!(menu.selected, 0, "a passed key does not move");
    }

    /// The answering keys wait for the grace; navigation never types,
    /// so it works at once.
    #[test]
    fn answering_keys_wait_for_the_grace() {
        let mut menu = approval();
        assert_eq!(
            menu.key(&key(KeyCode::Char('y'), KeyModifiers::NONE), true, false),
            Keyed::Passed
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('2'), KeyModifiers::NONE), true, false),
            Keyed::Passed
        );
        assert_eq!(
            menu.key(&key(KeyCode::Enter, KeyModifiers::NONE), true, false),
            Keyed::Passed
        );
        assert_eq!(
            menu.key(&key(KeyCode::Down, KeyModifiers::NONE), true, false),
            Keyed::Used
        );
        assert_eq!(menu.selected, 1);
        // A modified key is never an accelerator.
        assert_eq!(
            menu.key(&key(KeyCode::Char('y'), KeyModifiers::CONTROL), true, true),
            Keyed::Passed
        );
    }

    /// y/a/p/n stay as hidden accelerators; Esc opens the reason input
    /// at once; a question in the pre-options shape takes no keys.
    #[test]
    fn the_hidden_accelerators_and_esc() {
        let mut menu = approval();
        assert_eq!(
            menu.key(&key(KeyCode::Char('y'), KeyModifiers::NONE), true, true),
            once()
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('a'), KeyModifiers::NONE), true, true),
            more()
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('p'), KeyModifiers::NONE), true, true),
            more()
        );
        assert_eq!(
            menu.key(&key(KeyCode::Char('n'), KeyModifiers::NONE), true, true),
            no(None)
        );
        // Esc opens the reason input, without waiting for the grace.
        assert_eq!(
            menu.key(&key(KeyCode::Esc, KeyModifiers::NONE), true, false),
            Keyed::Text
        );
        let mut menu = plain_question("which colour?");
        assert_eq!(
            menu.key(&key(KeyCode::Char('y'), KeyModifiers::NONE), true, true),
            Keyed::Passed
        );
        assert_eq!(
            menu.key(&key(KeyCode::Esc, KeyModifiers::NONE), true, true),
            Keyed::Passed,
            "Esc interrupts the turn, as before"
        );
    }

    /// A pipe reads a number or text: the digits pick rows, the old
    /// letters still work, a reason may follow a deny, and anything
    /// else is not an answer.
    #[test]
    fn a_line_answers_by_number_or_letter() {
        let menu = approval();
        assert_eq!(menu.line("1"), Some(once()));
        assert_eq!(menu.line("2"), Some(more()));
        assert_eq!(menu.line("3"), Some(no(None)));
        assert_eq!(menu.line("y"), Some(once()));
        assert_eq!(menu.line("Yes"), Some(once()));
        assert_eq!(menu.line("a"), Some(more()));
        assert_eq!(menu.line("p"), Some(more()));
        assert_eq!(menu.line("n"), Some(no(None)));
        assert_eq!(
            menu.line("n the build directory is shared"),
            Some(no(Some("the build directory is shared")))
        );
        assert_eq!(
            menu.line("3 the build directory is shared"),
            Some(no(Some("the build directory is shared")))
        );
        assert_eq!(menu.line("hello there"), None);
        assert_eq!(menu.line(""), None);
        assert_eq!(
            plain_question("which colour?").line("blue"),
            Some(answer("blue"))
        );
    }

    /// The issue's correction of 2026-09-24: option two must say what a
    /// Yes covers. A prefix covers those words in this project; a bash
    /// session grant only the exact command; any other tool's, every
    /// call to it.
    #[test]
    fn option_two_names_what_a_yes_covers() {
        let menu = Menu::permission(
            &bash("curl -s https://example.com"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(
            menu.rows[1].label,
            "Yes, and don't ask again for `curl -s` in this project"
        );
        assert_eq!(
            menu.rows[1].pick,
            Pick::Allow {
                session: false,
                prefix: Some(vec!["curl".into(), "-s".into()]),
            }
        );
        let menu = Menu::permission(
            &bash("echo hi && echo bye"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(
            menu.rows[1].label,
            "Yes, and don't ask again for this exact command this session"
        );
        assert_eq!(
            menu.rows[1].pick,
            Pick::Allow {
                session: true,
                prefix: None,
            }
        );
        let call = ToolCall {
            id: "c2".into(),
            name: "edit_file".into(),
            args: json!({"path": "docs/PLAN-phase6.md"}),
        };
        let menu = Menu::permission(&call, RiskClass::Write, "class write: ask");
        assert_eq!(
            menu.rows[1].label,
            "Yes, and don't ask again for `edit_file` this session"
        );
        assert_eq!(menu.title, "Edit this file?");
        assert_eq!(menu.body, "edit_file docs/PLAN-phase6.md");
    }

    /// A pipe prints the options numbered and reads a number or text;
    /// the class-generated reason is dropped, a rule's own words stay.
    #[test]
    fn plain_prints_the_options_numbered() {
        let menu = Menu::permission(
            &bash("rm -rf build"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(
            menu.plain(),
            vec![
                "[permission] Run this command?",
                "  rm -rf build",
                "  1. Yes",
                "  2. Yes, and don't ask again for `rm -rf build` in this project",
                "  3. No, and tell the agent why",
            ]
        );
        let call = ToolCall {
            id: "c3".into(),
            name: "mcp.docs.search".into(),
            args: json!({"query": "phase 6"}),
        };
        let menu = Menu::permission(&call, RiskClass::Network, "mcp.docs: the plan says ask");
        assert_eq!(menu.title, "Call this MCP tool?");
        assert_eq!(menu.plain()[0], "[permission] Call this MCP tool?");
        assert_eq!(menu.plain()[1], "  mcp.docs.search {\"query\":\"phase 6\"}");
        assert_eq!(menu.plain()[5], "  mcp.docs: the plan says ask");
        let menu = plain_question("which colour?");
        assert_eq!(
            menu.plain(),
            vec!["[question] which colour?", "  type the answer"]
        );
    }

    /// A question with options renders on the same widget: the options,
    /// Other last, the description dim; a digit answers and the menu
    /// moves to the next question; the composed answer is one line per
    /// question.
    #[test]
    fn a_question_with_options_answers_one_after_another() {
        let mut menu = Menu::asking(asked());
        assert_eq!(menu.title, "Which colour?");
        let labels: Vec<&str> = menu.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["Red", "Green", "Other: type your own"]);
        assert_eq!(
            menu.plain(),
            vec![
                "[question] Which colour?",
                "  1. Red · the warm one",
                "  2. Green",
                "  3. Other: type your own",
            ]
        );
        // A digit picks a row; the answer is the contribution, headed.
        assert_eq!(
            menu.key(&key(KeyCode::Char('2'), KeyModifiers::NONE), true, true),
            answer("colour: Green")
        );
        // The engine applies it: the next question shows, multi this time.
        assert_eq!(menu.answer("colour: Green"), None);
        assert_eq!(menu.title, "Which tests?");
        assert!(menu.multi);
        assert_eq!(
            menu.note.as_deref(),
            Some("space toggles · enter sends · a pipe: `1 2`")
        );
        // Space toggles the selected row; digits toggle too.
        assert_eq!(
            menu.key(&key(KeyCode::Char(' '), KeyModifiers::NONE), true, true),
            Keyed::Used
        );
        assert!(menu.picked[0]);
        assert_eq!(
            menu.key(&key(KeyCode::Char('2'), KeyModifiers::NONE), true, true),
            Keyed::Used
        );
        assert!(menu.picked[1]);
        // Enter submits the picked rows.
        assert_eq!(
            menu.key(&key(KeyCode::Enter, KeyModifiers::NONE), true, true),
            answer("unit, integration")
        );
        assert_eq!(menu.answer("unit, integration"), None);
        // A question without options: the composer takes it, free text.
        assert_eq!(menu.title, "Ship it?");
        assert!(menu.rows.is_empty());
        assert_eq!(menu.line("yes, friday"), Some(answer("yes, friday")));
        assert_eq!(
            menu.answer("yes, friday").unwrap(),
            "colour: Green\nunit, integration\nyes, friday"
        );
    }

    /// A pipe reads numbers for a question: one number picks a row,
    /// several (space or comma separated) pick several when the question
    /// allows it, and anything else is free text.
    #[test]
    fn a_pipe_answers_a_question_by_number_or_text() {
        let mut menu = Menu::asking(asked());
        assert_eq!(menu.line("1"), Some(answer("colour: Red")));
        assert_eq!(menu.line("red please"), Some(answer("colour: red please")));
        assert_eq!(menu.answer("colour: red please"), None);
        assert_eq!(menu.line("2 1"), Some(answer("integration, unit")));
        assert_eq!(menu.line("2,1"), Some(answer("integration, unit")));
        assert_eq!(
            menu.line("9"),
            Some(answer("9")),
            "a number past the rows is free text"
        );
        assert_eq!(menu.line("both please"), Some(answer("both please")));
        assert_eq!(menu.answer("both please"), None, "the third question waits");
        // The whole call is answered in one go: one line per question.
        assert_eq!(
            menu.answer("friday"),
            Some("colour: red please\nboth please\nfriday".to_owned())
        );
        // A number is not an answer while a draft is being typed: the
        // composer is the free-text answer.
        let mut menu = Menu::asking(asked());
        assert_eq!(
            menu.key(&key(KeyCode::Char('1'), KeyModifiers::NONE), false, true),
            Keyed::Passed
        );
        // Picking Other hands the composer to the question.
        let mut menu = Menu::asking(asked());
        assert_eq!(
            menu.key(&key(KeyCode::Char('3'), KeyModifiers::NONE), true, true),
            Keyed::Text
        );
        assert_eq!(menu.free_text("magenta"), "colour: magenta");
    }
}
