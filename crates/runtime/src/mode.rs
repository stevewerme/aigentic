//! The session's permission mode: what runs without asking beyond what the
//! rule table already allows. Session state like a session grant: never
//! persisted, never an event of its own; each call it lets through carries
//! a rule record naming the mode, so the log still says why it ran. A
//! `Deny` from the rules stands in every mode.

use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// The phase 3 behaviour: every `Ask` goes to the approver.
    #[default]
    Manual,
    /// `write`-class calls that would have asked run instead.
    AcceptEdits,
    /// Anything that would have asked runs instead.
    Auto,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Manual, Mode::AcceptEdits, Mode::Auto];

    /// The name `/mode` and `--mode` use.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Manual => "manual",
            Mode::AcceptEdits => "accept-edits",
            Mode::Auto => "auto",
        }
    }

    /// The rule name on the policy record of a call the mode let through.
    pub fn rule_name(self) -> &'static str {
        match self {
            Mode::Manual => "mode manual",
            Mode::AcceptEdits => "mode accept-edits",
            Mode::Auto => "mode auto",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The names, for an error message.
pub fn mode_names() -> String {
    Mode::ALL
        .iter()
        .map(|m| m.name())
        .collect::<Vec<_>>()
        .join(", ")
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        Mode::ALL
            .into_iter()
            .find(|m| m.name() == s || m.name().replace('-', "_") == s)
            .ok_or_else(|| format!("unknown mode {s:?}; the modes are {}", mode_names()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip_and_unknown_is_an_error() {
        for m in Mode::ALL {
            assert_eq!(m.name().parse::<Mode>().unwrap(), m);
            assert_eq!(m.to_string(), m.name());
        }
        assert_eq!("accept_edits".parse::<Mode>().unwrap(), Mode::AcceptEdits);
        assert_eq!(" AUTO ".parse::<Mode>().unwrap(), Mode::Auto);
        let err = "yolo".parse::<Mode>().unwrap_err();
        assert!(err.contains("manual, accept-edits, auto"), "{err}");
        assert_eq!(Mode::default(), Mode::Manual);
    }
}
