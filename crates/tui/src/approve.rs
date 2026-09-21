//! The inline permission prompt: `y` once, `a` for the session, `n` deny,
//! Ctrl-C or Ctrl-D deny. A stdin that is not a terminal denies without
//! asking, and `ask_human` answers "no human available".

use std::io::{BufRead, IsTerminal, Write};

use aigentic_runtime::aigentic_core::Author;
use aigentic_runtime::aigentic_log::PermissionRequestedPayload;
use aigentic_runtime::{Answer, Approver};

pub struct InlineApprover {
    user: Author,
    interactive: bool,
}

impl InlineApprover {
    pub fn new(user: Author) -> Self {
        Self {
            user,
            interactive: std::io::stdin().is_terminal(),
        }
    }

    fn read_line(&self) -> Option<String> {
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

/// `y` / `yes` once, `a` / `always` for the session, anything else deny.
pub fn parse_answer(line: &str) -> Answer {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Answer::Allow,
        "a" | "always" | "session" => Answer::AllowForSession,
        _ => Answer::Deny,
    }
}

/// The lines shown before the prompt.
pub fn describe(request: &PermissionRequestedPayload) -> String {
    let args = request.call.args.to_string();
    let args = crate::repl::truncate_for_display(&args, 6, 600);
    format!(
        "[permission] {} ({:?}): {}\n  {args}",
        request.call.name, request.class, request.reason
    )
}

impl Approver for InlineApprover {
    fn author(&self) -> Author {
        self.user.clone()
    }

    fn ask(&mut self, request: &PermissionRequestedPayload) -> Answer {
        if !self.interactive {
            println!("{}\n  [no terminal: denied]", describe(request));
            return Answer::Deny;
        }
        println!("{}", describe(request));
        print!("  allow? [y]es once / [a]lways this session / [n]o: ");
        match self.read_line() {
            Some(line) => parse_answer(&line),
            None => {
                println!();
                Answer::Deny
            }
        }
    }

    fn ask_human(&mut self, question: &str) -> Option<String> {
        if !self.interactive {
            return None;
        }
        println!("[the assistant asks] {question}");
        print!("  your answer: ");
        let line = self.read_line()?;
        let line = line.trim();
        (!line.is_empty()).then(|| line.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_parse() {
        assert_eq!(parse_answer("y\n"), Answer::Allow);
        assert_eq!(parse_answer(" YES "), Answer::Allow);
        assert_eq!(parse_answer("a"), Answer::AllowForSession);
        assert_eq!(parse_answer("always"), Answer::AllowForSession);
        assert_eq!(parse_answer("n"), Answer::Deny);
        assert_eq!(parse_answer(""), Answer::Deny);
        assert_eq!(parse_answer("whatever"), Answer::Deny);
    }
}
