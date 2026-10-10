//! The prompt menu (issue #13): the selectable list a permission request
//! and an `ask_human` question render as, in the style of Claude Code's
//! menus. One widget, two uses: the shell draws it above the composer
//! and feeds it keys, a pipe prints the rows numbered and reads a number
//! or text. The answering keys wait for the shell's `PROMPT_GRACE`.

use aigentic_runtime::aigentic_core::{RiskClass, ToolCall};
use aigentic_runtime::aigentic_policy::{prefix_of, riskiest_segment};
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
    /// Continue the followed run past its checkpoint: `go`.
    ContinueRun,
    /// Continue with changes: `amend`, its text typed after the pick or
    /// given with it (`amend <text>`, carried as the decision's reason).
    AmendRun,
    /// Stop the followed run (issue #68), at its checkpoint prompt.
    StopRun,
    /// Leave the followed run waiting: the prompt goes, nothing is sent.
    LeaveWaiting,
    /// Switch the thread to the proposal's project (issue #82).
    SwitchYes,
    /// Turn the proposal down: the thread stays here.
    SwitchNo,
    /// The proposal is for somewhere else: the composer takes where.
    SwitchElsewhere,
    /// A typed destination, `n <where>` or `3 <where>`: #7's spelling,
    /// kept working.
    SwitchCorrected { to: String },
    /// Go to a new thread anyway: the running turn is interrupted first
    /// (issue #108).
    NewYes,
    /// Keep working in this thread: nothing is interrupted (issue #108).
    NewNo,
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
    /// A followed build's checkpoint: the answers the gate offers, and
    /// leaving it waiting.
    Checkpoint,
    /// A switch proposal (issue #82): go to the proposed project, stay,
    /// or say where it belongs.
    Switch,
    /// `/new` while a turn is open (issue #108): interrupt it and start
    /// a new thread, or keep working here. The engine's own menu, never
    /// sent to the daemon.
    NewThread,
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
    /// correction of 2026-09-24). A `bash` chain names its riskiest
    /// segment in the header and grants that segment, never the whole
    /// chain (issue #16).
    pub fn permission(call: &ToolCall, class: RiskClass, reason: &str) -> Self {
        let command = (call.name == "bash")
            .then(|| call.args.get("command").and_then(|c| c.as_str()))
            .flatten();
        let prefix = command.map(prefix_of).filter(|p| !p.is_empty());
        // A chain asks as its riskiest segment, and the header says
        // which one: `git add && git commit && git push` asks as
        // "includes git push".
        let title = match command.and_then(riskiest_segment) {
            Some(r) if r.chain && !r.words.is_empty() => {
                format!("Run this command? (includes {})", r.words.join(" "))
            }
            _ => title(&call.name, class),
        };
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
            title,
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

    /// The checkpoint prompt for a followed build: what the lead is
    /// waiting at, a row for each answer the gate offers (`go`, `amend`,
    /// `stop`, in that order), and `Leave it waiting`. The selection
    /// starts on `Leave it waiting`, the one row that sends nothing, so a
    /// stray Enter never stops or continues a run.
    pub fn checkpoint(gate: &str, shown: &[String], options: &[String]) -> Self {
        let offered = |word: &str| options.iter().any(|option| option == word);
        let row = |label: &str, pick: Pick| Row {
            label: label.into(),
            desc: None,
            pick,
        };
        let mut rows = Vec::new();
        if offered("go") {
            rows.push(row("Continue", Pick::ContinueRun));
        }
        if offered("amend") {
            rows.push(row("Continue with changes", Pick::AmendRun));
        }
        rows.push(row("Leave it waiting", Pick::LeaveWaiting));
        rows.push(row("Stop the run", Pick::StopRun));
        let selected = rows
            .iter()
            .position(|row| row.pick == Pick::LeaveWaiting)
            .unwrap_or(0);
        let note = offered("amend").then(|| "or type: amend <the changes>".to_owned());
        Self {
            kind: Kind::Checkpoint,
            title: format!("checkpoint {gate}"),
            body: shown.join("\n"),
            note,
            rows,
            selected,
            multi: false,
            picked: Vec::new(),
            questions: None,
        }
    }

    /// The switch proposal (issue #82; ADR 0002): a proposal is answered
    /// in one keystroke, in the same in-place block as a permission or a
    /// question. The title names the project the thread would move to;
    /// the reason is the body, with the target's workspace on its own
    /// line when there is one (#7's wording, kept).
    pub fn switch(project: &str, workspace: Option<&str>, reason: &str) -> Self {
        let mut body = reason.to_owned();
        if let Some(w) = workspace {
            if !body.is_empty() {
                body.push('\n');
            }
            body.push_str(&format!("in workspace {w}"));
        }
        Self {
            kind: Kind::Switch,
            title: format!("switch to {project}?"),
            body,
            note: None,
            rows: vec![
                Row {
                    label: format!("Yes, switch to {project}"),
                    desc: None,
                    pick: Pick::SwitchYes,
                },
                Row {
                    label: "No, stay here".into(),
                    desc: None,
                    pick: Pick::SwitchNo,
                },
                Row {
                    label: "No, it belongs somewhere else…".into(),
                    desc: None,
                    pick: Pick::SwitchElsewhere,
                },
            ],
            selected: 0,
            multi: false,
            picked: Vec::new(),
            questions: None,
        }
    }

    /// `/new` with a turn open (issue #108): the REPL's own question,
    /// asked in the REPL's menu style, because a new front thread closes
    /// this one and the running turn would be left behind, unseen. `No`
    /// is the default; Esc answers it too.
    pub fn new_thread() -> Self {
        Self {
            kind: Kind::NewThread,
            title: "a turn is running in this thread".into(),
            body: "Start a new thread anyway? The running turn is interrupted first.".into(),
            note: Some("[y/N]".into()),
            rows: vec![
                Row {
                    label: "Yes, interrupt it and start a new thread".into(),
                    desc: None,
                    pick: Pick::NewYes,
                },
                Row {
                    label: "No, keep working here".into(),
                    desc: None,
                    pick: Pick::NewNo,
                },
            ],
            selected: 1,
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
            // Esc on a checkpoint leaves the run waiting, at once.
            KeyCode::Esc if self.kind == Kind::Checkpoint => self.pick_of(&Pick::LeaveWaiting),
            // Esc on a switch proposal is `No, stay here`, at once.
            KeyCode::Esc if self.kind == Kind::Switch => self.pick(1),
            // Esc on the `/new` question keeps this thread, at once: it
            // must never fall through to the keymap's interrupt, which
            // is exactly what the question asks about (issue #108).
            KeyCode::Esc if self.kind == Kind::NewThread => self.pick(1),
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
            // A switch proposal's accelerators too, but only on an empty
            // composer: a message beginning with "n" must never answer
            // `No`.
            KeyCode::Char(c) if plain && self.kind == Kind::Switch && composer_empty => match c {
                'y' => self.pick(0),
                'n' => self.pick(1),
                _ => Keyed::Passed,
            },
            // The `/new` question takes the switch's rule too, not the
            // permission's: a draft beginning with "y" must never answer
            // Yes to interrupting the turn (issue #108).
            KeyCode::Char(c) if plain && self.kind == Kind::NewThread && composer_empty => {
                match c {
                    'y' => self.pick(0),
                    'n' => self.pick(1),
                    _ => Keyed::Passed,
                }
            }
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
            // Where it belongs: the composer takes the destination, as
            // a question's `Other` does.
            Pick::SwitchElsewhere => Keyed::Text,
            Pick::SwitchYes | Pick::SwitchNo => Keyed::Decide {
                pick: row.pick.clone(),
                reason: None,
                echo: format!("↳ {}", row.label),
            },
            // The `/new` question (issue #108): the engine reads these
            // two picks locally, nothing goes to the daemon from here.
            Pick::NewYes | Pick::NewNo => Keyed::Decide {
                pick: row.pick.clone(),
                reason: None,
                echo: format!("↳ {}", row.label),
            },
            // A row never carries a typed destination: only `line`
            // builds one. Nothing to send from here.
            Pick::SwitchCorrected { .. } => Keyed::Passed,
            Pick::ContinueRun | Pick::StopRun => Keyed::Decide {
                pick: row.pick.clone(),
                reason: None,
                echo: format!("↳ {}", row.label),
            },
            // The changes are typed next: the composer takes them.
            Pick::AmendRun => Keyed::Text,
            Pick::LeaveWaiting => Keyed::Decide {
                pick: Pick::LeaveWaiting,
                reason: None,
                echo: format!("↳ {}", row.label),
            },
        }
    }

    /// The row whose pick is `pick`, picked; `Passed` when there is none.
    fn pick_of(&self, pick: &Pick) -> Keyed {
        match self.rows.iter().position(|row| &row.pick == pick) {
            Some(i) => self.pick(i),
            None => Keyed::Passed,
        }
    }

    /// A typed line at a checkpoint: a row's number, `go`, `stop`,
    /// `wait`, or `amend <text>`, each only when the gate offers it.
    /// Anything else is not the prompt's.
    fn checkpoint_line(&self, text: &str) -> Option<Keyed> {
        let (word, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
        let rest = rest.trim();
        let offers = |pick: &Pick| self.rows.iter().any(|row| &row.pick == pick);
        match (word.to_ascii_lowercase().as_str(), rest.is_empty()) {
            (digits, true) if digits.parse::<usize>().is_ok() => {
                let n: usize = digits.parse().ok()?;
                (n >= 1 && n <= self.rows.len()).then(|| self.pick(n - 1))
            }
            ("go" | "continue", true) if offers(&Pick::ContinueRun) => {
                Some(self.pick_of(&Pick::ContinueRun))
            }
            ("stop", true) => Some(self.pick_of(&Pick::StopRun)),
            ("wait" | "leave", true) => Some(self.pick_of(&Pick::LeaveWaiting)),
            ("amend", false) if offers(&Pick::AmendRun) => Some(Keyed::Decide {
                pick: Pick::AmendRun,
                reason: Some(rest.to_owned()),
                echo: "↳ Continue with changes".to_owned(),
            }),
            _ => None,
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
        if self.kind == Kind::Checkpoint {
            return self.checkpoint_line(text.trim());
        }
        // The switch arm is before the generic permission match, so
        // `3 <where>` is never read as a deny with a reason.
        if self.kind == Kind::Switch {
            return self.switch_line(text);
        }
        // The `/new` question answers on its own two rows only
        // (issue #108): `1`/`y`/`yes` interrupts and starts a new
        // thread, `2`/`n`/`no` keeps this one, and nothing else is an
        // answer, so a stray line goes on as whatever it was.
        if self.kind == Kind::NewThread {
            return match text.trim() {
                "1" | "y" | "yes" => Some(self.pick(0)),
                "2" | "n" | "no" => Some(self.pick(1)),
                _ => None,
            };
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

    /// A typed line for a switch proposal: `1`/`y` is yes, `2`/`n` is
    /// no, and #7's `n <where>` spelling keeps working (`3 <where>`
    /// too). A typed bare `3` is not an answer: a typed line cannot
    /// open the composer's text input, only the `3` key does. Anything
    /// else is not an answer either. Trimmed, never case-folded, as #7.
    fn switch_line(&self, text: &str) -> Option<Keyed> {
        let t = text.trim();
        let (head, rest) = t
            .split_once(char::is_whitespace)
            .map_or((t, ""), |(h, r)| (h, r.trim()));
        match head {
            "1" | "y" if rest.is_empty() => Some(self.pick(0)),
            "2" | "n" if rest.is_empty() => Some(self.pick(1)),
            "3" if rest.is_empty() => None,
            "n" | "3" => {
                let to = rest.to_owned();
                (!to.is_empty()).then(|| Keyed::Decide {
                    pick: Pick::SwitchCorrected { to: to.clone() },
                    reason: None,
                    echo: format!("↳ No, it belongs to: {to}"),
                })
            }
            _ => None,
        }
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
            Kind::Checkpoint => "checkpoint",
            Kind::Switch => "switch",
            Kind::NewThread => "new thread",
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
        // A chain that is not all read-only: it asks as its riskiest
        // segment, `git push` here, and that is what a grant covers.
        Menu::permission(
            &bash("git add src/lib.rs && git commit -m 'x' && git push"),
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
                session: false,
                prefix: Some(vec!["git".into(), "push".into()]),
            },
            reason: None,
            echo: "↳ Yes, and don't ask again for `git push` in this project".into(),
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
        assert_eq!(menu.title, "Run this command?");
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
        // A chain asks as its riskiest segment (issue #16): the header
        // names it, and a grant covers that segment, never the whole
        // chain.
        let menu = Menu::permission(
            &bash("git add && git commit -m 'x' && git push"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(menu.title, "Run this command? (includes git push)");
        assert_eq!(
            menu.rows[1].label,
            "Yes, and don't ask again for `git push` in this project"
        );
        assert_eq!(
            menu.rows[1].pick,
            Pick::Allow {
                session: false,
                prefix: Some(vec!["git".into(), "push".into()]),
            }
        );
        // The risk is a redirection's: no command prefix covers it, so
        // the header names nothing and the grant is the session's.
        let menu = Menu::permission(
            &bash("cargo test && echo done > log.txt"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(menu.title, "Run this command?");
        assert_eq!(
            menu.rows[1].label,
            "Yes, and don't ask again for this exact command this session"
        );
        // A read-only chain never asks, so what it would grant is moot;
        // its own command keeps its plain header.
        let menu = Menu::permission(
            &bash("cargo fmt && cargo test --workspace"),
            RiskClass::Exec,
            "class exec: anything else in a shell",
        );
        assert_eq!(menu.title, "Run this command?");
        // Read-only through and through, so it would not ask; either
        // way there is no prefix to grant and row two is the session
        // one.
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

    // ---- #82: the switch-proposal block ----

    fn switch(workspace: Option<&str>) -> Menu {
        Menu::switch("customer", workspace, "the message is about the site")
    }

    fn switch_keyed(pick: Pick, echo: &str) -> Keyed {
        Keyed::Decide {
            pick,
            reason: None,
            echo: echo.to_owned(),
        }
    }

    /// T1 (#82): `Menu::switch` is titled `switch to {project}?`, shows
    /// the reason and, when there is one, the target's workspace on its
    /// own line, and offers the three rows; the plain tag is `switch`.
    #[test]
    fn a_switch_proposal_is_a_three_row_block() {
        let menu = switch(Some("~/customer"));
        assert_eq!(menu.kind, Kind::Switch);
        assert_eq!(menu.title, "switch to customer?");
        assert_eq!(
            menu.body,
            "the message is about the site\nin workspace ~/customer"
        );
        assert_eq!(
            menu.rows,
            vec![
                Row {
                    label: "Yes, switch to customer".into(),
                    desc: None,
                    pick: Pick::SwitchYes,
                },
                Row {
                    label: "No, stay here".into(),
                    desc: None,
                    pick: Pick::SwitchNo,
                },
                Row {
                    label: "No, it belongs somewhere else…".into(),
                    desc: None,
                    pick: Pick::SwitchElsewhere,
                },
            ]
        );
        assert_eq!(menu.selected, 0);

        // No workspace on the wire: just the reason.
        let menu = switch(None);
        assert_eq!(menu.body, "the message is about the site");

        let plain = menu.plain();
        assert_eq!(plain[0], "[switch] switch to customer?");
        assert!(
            plain
                .iter()
                .any(|l| l.contains("the message is about the site")),
            "{plain:#?}"
        );
        assert!(
            plain
                .iter()
                .any(|l| l.contains("1. Yes, switch to customer")),
            "{plain:#?}"
        );
        assert!(
            plain.iter().any(|l| l.contains("2. No, stay here")),
            "{plain:#?}"
        );
        assert!(
            plain
                .iter()
                .any(|l| l.contains("3. No, it belongs somewhere else…")),
            "{plain:#?}"
        );
    }

    /// T2 (#82): the switch's keys — digits 1-3, Up/Down then Enter,
    /// Esc as *No, stay here* at once, `3` as the text input, and the
    /// hidden `y`/`n` accelerators only with an empty composer, after
    /// the grace.
    #[test]
    fn a_switch_answers_by_key() {
        let empty = KeyModifiers::NONE;
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('y'), empty), true, true),
            switch_keyed(Pick::SwitchYes, "↳ Yes, switch to customer")
        );
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('n'), empty), true, true),
            switch_keyed(Pick::SwitchNo, "↳ No, stay here")
        );
        // A draft in the composer is a message, not an answer: "never
        // mind" must not answer No.
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('n'), empty), false, true),
            Keyed::Passed
        );
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('y'), empty), false, true),
            Keyed::Passed
        );

        // The digits pick, as on every menu.
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('1'), empty), true, true),
            switch_keyed(Pick::SwitchYes, "↳ Yes, switch to customer")
        );
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('2'), empty), true, true),
            switch_keyed(Pick::SwitchNo, "↳ No, stay here")
        );
        assert_eq!(
            switch(None).key(&key(KeyCode::Char('3'), empty), true, true),
            Keyed::Text,
            "3 hands the composer the destination"
        );

        // Up/Down then Enter takes the selection.
        let mut menu = switch(None);
        assert_eq!(
            menu.key(&key(KeyCode::Down, empty), true, true),
            Keyed::Used
        );
        assert_eq!(menu.selected, 1);
        assert_eq!(menu.key(&key(KeyCode::Up, empty), true, true), Keyed::Used);
        assert_eq!(menu.selected, 0);
        assert_eq!(
            menu.key(&key(KeyCode::Enter, empty), true, true),
            switch_keyed(Pick::SwitchYes, "↳ Yes, switch to customer")
        );

        // Esc is *No, stay here*, at once: no grace, no text input.
        assert_eq!(
            switch(None).key(&key(KeyCode::Esc, empty), true, false),
            switch_keyed(Pick::SwitchNo, "↳ No, stay here")
        );

        // Before the grace has passed, the answering keys are gone.
        for code in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('1')] {
            assert_eq!(
                switch(None).key(&key(code, empty), true, false),
                Keyed::Passed,
                "{code:?} waits out the grace"
            );
        }
    }

    /// T3 (#82): a typed line — `1`/`y` is yes, `2`/`n` is no, #7's
    /// `n <where>` and its `3 <where>` sibling are corrected, a bare
    /// `3` is not an answer, and neither is anything else. No
    /// case-folding, as #7.
    #[test]
    fn a_typed_line_answers_a_switch() {
        let menu = switch(None);
        assert_eq!(
            menu.line("1"),
            Some(switch_keyed(Pick::SwitchYes, "↳ Yes, switch to customer"))
        );
        assert_eq!(
            menu.line("y"),
            Some(switch_keyed(Pick::SwitchYes, "↳ Yes, switch to customer"))
        );
        assert_eq!(
            menu.line("2"),
            Some(switch_keyed(Pick::SwitchNo, "↳ No, stay here"))
        );
        assert_eq!(
            menu.line("n"),
            Some(switch_keyed(Pick::SwitchNo, "↳ No, stay here"))
        );
        for line in ["n customer X", "3 customer X"] {
            assert_eq!(
                menu.line(line),
                Some(switch_keyed(
                    Pick::SwitchCorrected {
                        to: "customer X".into()
                    },
                    "↳ No, it belongs to: customer X"
                )),
                "{line} says where it belongs"
            );
        }
        // Trimmed.
        assert_eq!(
            menu.line("  n   customer X  "),
            Some(switch_keyed(
                Pick::SwitchCorrected {
                    to: "customer X".into()
                },
                "↳ No, it belongs to: customer X"
            ))
        );
        // A typed bare `3` cannot open the text input: only the key does.
        assert_eq!(menu.line("3"), None);
        // No case-folding and no wordier spellings, as #7.
        assert_eq!(menu.line("Y"), None);
        assert_eq!(menu.line("yes"), None);
        assert_eq!(menu.line("N"), None);
        assert_eq!(menu.line("No"), None);
        // Anything else is a chat line.
        assert_eq!(menu.line("good morning"), None);
        assert_eq!(menu.line(""), None);
    }

    /// The pick a `Keyed` carries, whatever its echo says: the tests for
    /// the `/new` question care which pick, not how it reads.
    fn picked(keyed: Keyed) -> Option<Pick> {
        match keyed {
            Keyed::Decide { pick, .. } => Some(pick),
            _ => None,
        }
    }

    fn keyed_echo(keyed: &Keyed) -> String {
        match keyed {
            Keyed::Decide { echo, .. } => echo.clone(),
            other => panic!("not a decide: {other:?}"),
        }
    }

    /// T1 (#108): `Menu::new_thread` is a two-row block, `No` selected,
    /// tagged `new thread`, with the `[y/N]` note; the digits and the
    /// accelerators answer, Enter takes the default, and Esc is `No` at
    /// once.
    #[test]
    fn the_new_thread_question_answers_by_key() {
        let mut menu = Menu::new_thread();
        assert_eq!(menu.kind, Kind::NewThread);
        assert_eq!(menu.title, "a turn is running in this thread");
        assert_eq!(
            menu.body,
            "Start a new thread anyway? The running turn is interrupted first."
        );
        assert_eq!(menu.note.as_deref(), Some("[y/N]"));
        let labels: Vec<&str> = menu.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Yes, interrupt it and start a new thread",
                "No, keep working here",
            ]
        );
        let picks: Vec<Pick> = menu.rows.iter().map(|r| r.pick.clone()).collect();
        assert_eq!(picks, vec![Pick::NewYes, Pick::NewNo]);
        assert_eq!(menu.selected, 1, "No is the default");

        let empty = KeyModifiers::NONE;
        // Enter takes the selection, which is No.
        assert_eq!(
            picked(menu.key(&key(KeyCode::Enter, empty), true, true)),
            Some(Pick::NewNo)
        );
        // The digits pick; the accelerators take the switch's rule, so
        // they need an empty composer.
        for (c, want) in [
            ('1', Pick::NewYes),
            ('y', Pick::NewYes),
            ('2', Pick::NewNo),
            ('n', Pick::NewNo),
        ] {
            let keyed = menu.key(&key(KeyCode::Char(c), empty), true, true);
            assert_eq!(
                picked(keyed.clone()),
                Some(want.clone()),
                "{c} picks {want:?}"
            );
            assert_eq!(
                keyed_echo(&keyed),
                format!(
                    "↳ {}",
                    menu.rows[if want == Pick::NewYes { 0 } else { 1 }].label
                ),
                "{c} echoes the row it picked"
            );
        }
        // A draft starting with `y` is a message, not an answer: it
        // must never answer Yes.
        for c in ['y', 'n'] {
            assert_eq!(
                menu.key(&key(KeyCode::Char(c), empty), false, true),
                Keyed::Passed,
                "{c} with a draft falls through to the keymap"
            );
        }
        // Esc is No, never Passed: the keymap's own Esc interrupts.
        assert_eq!(
            picked(menu.key(&key(KeyCode::Esc, empty), true, false)),
            Some(Pick::NewNo)
        );
        assert_eq!(
            picked(menu.key(&key(KeyCode::Esc, empty), false, true)),
            Some(Pick::NewNo)
        );

        // Before the grace has passed, the answering keys are gone.
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('n'),
            KeyCode::Char('1'),
            KeyCode::Char('2'),
        ] {
            assert_eq!(
                menu.key(&key(code, empty), true, false),
                Keyed::Passed,
                "{code:?} waits out the grace"
            );
        }
    }

    /// T1 (#108): the plain block, pinned on the renderer's own text,
    /// note indented as every other plain line is.
    #[test]
    fn a_new_thread_question_prints_in_plain() {
        assert_eq!(
            Menu::new_thread().plain(),
            vec![
                "[new thread] a turn is running in this thread",
                "  Start a new thread anyway? The running turn is interrupted first.",
                "  1. Yes, interrupt it and start a new thread",
                "  2. No, keep working here",
                "  [y/N]",
            ]
        );
    }

    /// T1 (#108): a typed line answers the `/new` question by number or
    /// letter; anything else answers nothing, so it stays a chat line.
    /// No case-folding, as #7.
    #[test]
    fn a_typed_line_answers_the_new_thread_question() {
        let menu = Menu::new_thread();
        for yes in ["1", "y", "yes"] {
            assert_eq!(
                picked(menu.line(yes).expect(yes)),
                Some(Pick::NewYes),
                "{yes} says yes"
            );
        }
        for no in ["2", "n", "no"] {
            assert_eq!(
                picked(menu.line(no).expect(no)),
                Some(Pick::NewNo),
                "{no} says no"
            );
        }
        // Trimmed.
        assert_eq!(
            picked(menu.line("  y  ").expect("trimmed y")),
            Some(Pick::NewYes)
        );
        // Nothing else answers: not a bare digit out of range, not a
        // wordier spelling, not case-folded.
        for line in [
            "3",
            "",
            "good morning",
            "Y",
            "Yes",
            "N",
            "No",
            "start a new thread",
        ] {
            assert_eq!(menu.line(line), None, "{line:?} is not an answer");
        }
    }
}
