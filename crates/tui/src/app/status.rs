//! The status line (plan section 6): what the daemon reports, never
//! what the client counts. Context fill is the last call's tokens in
//! the window against compaction's line — the ceiling we pay for, not
//! the provider's raw window; the clock and the calls live on the
//! turn line, the turn's cost in `/cost`; the queue is the daemon's.

use std::time::Duration;

use aigentic_api::ThreadState;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The thread's project.
    pub project: String,
    /// The thread's title, when it has one.
    pub title: Option<String>,
    pub mode: String,
    /// (tokens in the window, the ceiling — compaction's line), from
    /// `Notice::Usage`.
    pub usage: Option<(u64, u64)>,
    /// How long the running turn has run; `None` when idle.
    pub elapsed: Option<Duration>,
    pub queued: u32,
    /// What the thread waits on, when it does.
    pub waiting: Option<&'static str>,
}

impl Status {
    /// `vendela · Crate count · manual · 58k / 128k context · queued 1`
    pub fn line(&self) -> String {
        let mut parts = vec![self.project.clone()];
        if let Some(t) = &self.title {
            let short: String = t.chars().take(TITLE_CHARS).collect();
            parts.push(if t.chars().count() > TITLE_CHARS {
                format!("{short}…")
            } else {
                short
            });
        }
        parts.push(self.mode.clone());
        // The last call's context against the ceiling (issue #21): not
        // a percentage — the two sizes, read as a fraction.
        match self.usage {
            Some((used, ceiling)) => parts.push(format!(
                "{} / {} context",
                count_short(used),
                count_short(ceiling)
            )),
            None => parts.push("context ?".into()),
        }
        if let Some(elapsed) = self.elapsed {
            parts.push(elapsed_short(elapsed));
        }
        if self.queued > 0 {
            parts.push(format!("queued {}", self.queued));
        }
        if let Some(w) = self.waiting {
            parts.push(w.to_owned());
        }
        parts.join(" · ")
    }

    /// The parts that come from the thread's state.
    pub fn apply_state(&mut self, state: &ThreadState) {
        match state {
            ThreadState::Idle => {
                self.queued = 0;
                self.waiting = None;
            }
            ThreadState::Running { queued, .. } => {
                self.queued = *queued;
                self.waiting = None;
            }
            ThreadState::AwaitingApproval { .. } => self.waiting = Some("awaiting approval"),
            ThreadState::AwaitingHuman { .. } => self.waiting = Some("awaiting an answer"),
        }
    }
}

/// How much of the title the footer shows.
const TITLE_CHARS: usize = 40;

/// `950`, `4.0k`, `128k`, `1.3M`: a size in decimal thousands — the
/// one vocabulary for the footer's context and `/cost`'s counts
/// (issue #21), so the 128,000 ceiling reads `128k`, not `125k`.
pub fn count_short(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else if n < 1_000_000 {
        format!("{}k", n / 1000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// `12s`, `1m 05s`, `1h 02m`.
pub fn elapsed_short(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_reads_left_to_right() {
        let mut s = Status {
            project: "vendela".into(),
            title: None,
            mode: "manual".into(),
            usage: Some((31_000, 100_000)),
            elapsed: Some(Duration::from_secs(12)),
            queued: 1,
            waiting: None,
        };
        // The two sizes, used against the ceiling, not a percentage.
        assert_eq!(
            s.line(),
            "vendela · manual · 31k / 100k context · 12s · queued 1"
        );
        s.elapsed = None;
        s.queued = 0;
        s.usage = None;
        assert_eq!(s.line(), "vendela · manual · context ?");
        s.title = Some("A title".into());
        assert_eq!(s.line(), "vendela · A title · manual · context ?");
        // A fuller window than the ceiling still reads as itself.
        s.usage = Some((250_000, 100_000));
        assert_eq!(s.line(), "vendela · A title · manual · 250k / 100k context");
    }

    #[test]
    fn sizes_read_in_decimal_thousands() {
        // The 128,000 ceiling reads 128k, not 125k (issue #21).
        assert_eq!(count_short(128_000), "128k");
        assert_eq!(count_short(32_768), "32k");
        assert_eq!(count_short(1_048_576), "1.0M");
        assert_eq!(count_short(200_000), "200k");
        assert_eq!(count_short(4_000), "4.0k");
        assert_eq!(count_short(512), "512");
    }

    #[test]
    fn elapsed_formats() {
        assert_eq!(elapsed_short(Duration::from_secs(5)), "5s");
        assert_eq!(elapsed_short(Duration::from_secs(65)), "1m 05s");
        assert_eq!(elapsed_short(Duration::from_secs(3725)), "1h 02m");
    }

    #[test]
    fn state_sets_queue_and_waiting() {
        let mut s = Status::default();
        s.apply_state(&ThreadState::Running {
            by: aigentic_runtime::aigentic_core::Author::System,
            queued: 2,
        });
        assert_eq!(s.queued, 2);
        s.apply_state(&ThreadState::AwaitingHuman {
            call_id: "c".into(),
            question: "q".into(),
            questions: vec![],
        });
        assert_eq!(s.waiting, Some("awaiting an answer"));
        s.apply_state(&ThreadState::Idle);
        assert_eq!(s.queued, 0);
        assert_eq!(s.waiting, None);
    }
}
