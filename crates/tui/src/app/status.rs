//! The status line (plan section 6): what the daemon reports, never
//! what the client counts. Context fill is the last call's tokens in
//! the window against compaction's line — the ceiling we pay for, not
//! the provider's raw window; the clock and the calls live on the
//! turn line, the turn's cost in `/cost`; the queue is the daemon's.

use std::time::Duration;

use aigentic_api::ThreadState;

use crate::app::engine::Identity;
use crate::progress;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// The thread's project.
    pub project: String,
    /// The profile, model and effort the daemon named at attach
    /// (issue #43); `None` on a daemon that does not send them, which
    /// leaves the line as it was.
    pub identity: Option<Identity>,
    /// The thread's title, when it has one.
    pub title: Option<String>,
    pub mode: String,
    /// The working and thread figures from `Notice::Usage`.
    pub usage: Option<Figures>,
    /// How many builds the daemon drives right now, from `Notice::Usage`
    /// (issue #79). Drawn only when it is above zero: a machine with
    /// nothing running says nothing about builds.
    pub working: u32,
    /// How long the running turn has run; `None` when idle.
    pub elapsed: Option<Duration>,
    pub queued: u32,
    /// What the thread waits on, when it does.
    pub waiting: Option<&'static str>,
    /// The followed run's own segment, last on the line while a run is
    /// followed and dropped when the follow ends.
    pub run: Option<String>,
    /// The watched step's own head, as `inside #140 spec`, while a step
    /// of a running issue is open here: the line is the watch's alone
    /// then, and the front thread's parts come back when it ends.
    pub inside: Option<String>,
}

impl Status {
    /// `vendela · Crate count · flash (deepseek-v4.1-flash) · manual ·
    /// working 48k · thread 2.1M · queued 1`
    pub fn line(&self) -> String {
        // Inside a watch the line is the watch's alone: what the watched
        // run is doing, and the way back. `inside` already names the
        // issue and the step, so the run's figures are drawn bare.
        if let Some(inside) = &self.inside {
            let mut parts = vec![inside.clone()];
            if let Some(run) = &self.run {
                parts.push(run.clone());
            }
            if self.working > 0 {
                parts.push(format!("{} working", self.working));
            }
            parts.push("/back".to_owned());
            return parts.join(" · ");
        }
        let mut parts = vec![self.project.clone()];
        if let Some(t) = &self.title {
            let short: String = t.chars().take(TITLE_CHARS).collect();
            parts.push(if t.chars().count() > TITLE_CHARS {
                format!("{short}…")
            } else {
                short
            });
        }
        if let Some(who) = self.identity.as_ref().and_then(Identity::label) {
            parts.push(who);
        }
        parts.push(self.mode.clone());
        // What the model sees and how big the thread really is (issue
        // #99): information, not a warning, so no ceiling to read it
        // against. A daemon before #99 sends no thread figure.
        match self.usage {
            Some(Figures {
                working,
                thread: Some(thread),
            }) => parts.push(format!(
                "working {} · thread {}",
                count_short(working),
                count_short(thread)
            )),
            Some(Figures {
                working,
                thread: None,
            }) => parts.push(format!("working {}", count_short(working))),
            // A daemon that has sent no usage figure yet leaves the part
            // off the line: `?` was never a figure.
            None => {}
        }
        // How busy the machine is (issue #79): nothing to say when
        // nothing runs.
        if self.working > 0 {
            parts.push(format!("{} working", self.working));
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
        if let Some(run) = &self.run {
            parts.push(run.clone());
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
            ThreadState::AwaitingSwitch { .. } => self.waiting = Some("awaiting switch"),
        }
    }
}

/// The followed run's segment: `#139 spec · 3m12s · 72 calls · $0.51 ·
/// 2/6 Map the code · quiet 3m`, or `#139 waiting at decide` at a gate. The step's figures come from the
/// daemon's own fold over the lead's and the child's logs, so the line
/// repeats nothing the client counts.
pub fn run_segment(status: &aigentic_api::RunStatus) -> String {
    // The issue names the whole line; the rest is one phrase, so the
    // step reads as the issue's own while the figures follow it.
    format!("#{} {}", status.issue, run_parts(status, true).join(" · "))
}

/// The run segment's figures without the head it is drawn under: what the
/// status line shows beside a watch's own head, which already names the
/// issue and the step, so the figures are the segment's parts alone.
pub fn run_figures(status: &aigentic_api::RunStatus) -> String {
    run_parts(status, false).join(" · ")
}

/// The run segment's parts, in order: the step's own first when `named`,
/// then its figures, then the silence. One builder keeps the watched
/// figures a prefix of the segment they come from.
fn run_parts(status: &aigentic_api::RunStatus, named: bool) -> Vec<String> {
    let mut parts = Vec::new();
    match &status.phase {
        aigentic_api::RunPhase::Step {
            step,
            elapsed_secs,
            calls,
            cost_usd,
            checklist,
            ..
        } => {
            if named {
                parts.push(step.clone());
            }
            parts.push(progress::short_duration(*elapsed_secs as i64));
            parts.push(format!("{calls} calls"));
            // Two decimals, the provider's own stamp: the segment's money
            // matches `aigentic status`'s `$a` part, never its estimate.
            parts.push(format!("${cost_usd:.2}"));
            if let Some(checklist) = checklist {
                let mut item = format!("{}/{}", checklist.done, checklist.total);
                if let Some(active) = &checklist.active {
                    item.push(' ');
                    item.push_str(&progress::clip(active, progress::TITLE_CHARS));
                }
                parts.push(item);
            }
        }
        aigentic_api::RunPhase::Gate { gate } => parts.push(format!("waiting at {gate}")),
        aigentic_api::RunPhase::Move { what } => parts.push(what.clone()),
        aigentic_api::RunPhase::Stopped { reason } => {
            parts.push(format!("stopped: {}", crate::run_view::reason_line(reason)))
        }
    }
    // Two idle minutes is the point the run reads as stopped rather than
    // slow, so the figure appears only then.
    if status.idle_secs >= QUIET_AFTER_SECS {
        parts.push(format!("quiet {}m", status.idle_secs / 60));
    }
    parts
}

/// Seconds of silence after which the segment says `quiet`.
const QUIET_AFTER_SECS: u64 = 120;

/// The daemon's last `Notice::Usage` (issue #99): what the model sees
/// this call, and the whole thread as if nothing had been stubbed or
/// summarised — `None` from a daemon before #99.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Figures {
    pub working: u64,
    pub thread: Option<u64>,
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

    /// T10 (issue #79): the working count is the daemon's, drawn from the
    /// line's own parts, and only while something is working. A count of
    /// zero never prints a `0 working` segment, and a line with no count
    /// reads as it did.
    #[test]
    fn the_line_shows_the_count_only_when_something_works() {
        let base = Status {
            project: "vendela".into(),
            mode: "manual".into(),
            ..Status::default()
        };
        assert!(!base.line().contains("working"), "{}", base.line());

        let busy = Status {
            working: 2,
            ..base.clone()
        };
        let line = busy.line();
        assert_eq!(line, "vendela · manual · 2 working");
        assert!(line.starts_with("vendela · manual"));
        // Inside a watch the line is the watch's own, and the count
        // draws on it as on any other line.
        let inside = Status {
            inside: Some("#64 · spec".into()),
            ..busy.clone()
        };
        let line = inside.line();
        assert_eq!(line, "#64 · spec · 2 working · /back");
        // Nothing running: no segment, in both shapes.
        let idle = Status {
            inside: Some("#64 · spec".into()),
            working: 0,
            ..Status::default()
        };
        assert_eq!(idle.line(), "#64 · spec · /back");
    }

    #[test]
    fn the_line_reads_left_to_right() {
        let mut s = Status {
            project: "vendela".into(),
            identity: None,
            title: None,
            mode: "manual".into(),
            usage: Some(Figures {
                working: 31_000,
                thread: Some(2_100_000),
            }),
            working: 0,
            elapsed: Some(Duration::from_secs(12)),
            queued: 1,
            waiting: None,
            run: None,
            inside: None,
        };
        // Working and thread (issue #99), each in the footer's one
        // vocabulary, and no ceiling to read them against.
        assert_eq!(
            s.line(),
            format!(
                "vendela · manual · working {} · thread {} · 12s · queued 1",
                count_short(31_000),
                count_short(2_100_000)
            )
        );
        s.elapsed = None;
        s.queued = 0;
        s.usage = None;
        assert_eq!(s.line(), "vendela · manual");
        s.title = Some("A title".into());
        assert_eq!(s.line(), "vendela · A title · manual");
        // A daemon before #99 sends no thread figure: working alone.
        s.usage = Some(Figures {
            working: 250_000,
            thread: None,
        });
        assert_eq!(
            s.line(),
            format!(
                "vendela · A title · manual · working {}",
                count_short(250_000)
            )
        );
    }

    /// A `RunStatus` whose step carries the fixture's own figures, so a
    /// test's expectation is derived rather than typed twice.
    fn a_step(issue: u64, elapsed_secs: u64, calls: u32, cost_usd: f64) -> aigentic_api::RunStatus {
        aigentic_api::RunStatus {
            issue,
            phase: aigentic_api::RunPhase::Step {
                step: STEP.into(),
                attempt: 1,
                elapsed_secs,
                calls,
                cost_usd,
                checklist: None,
            },
            idle_secs: 0,
        }
    }

    const STEP: &str = "spec";
    const GATE: &str = "decide";

    #[test]
    fn the_segment_reads_the_issues_step_its_figures_and_its_checklist() {
        const ISSUE: u64 = 139;
        const ELAPSED: u64 = 192;
        const CALLS: u32 = 72;
        const COST: f64 = 0.51;
        let mut status = a_step(ISSUE, ELAPSED, CALLS, COST);
        let aigentic_api::RunPhase::Step { checklist, .. } = &mut status.phase else {
            unreachable!("a_step builds a step");
        };
        *checklist = Some(aigentic_api::RunChecklist {
            done: 2,
            total: 6,
            active: Some("Map the code".into()),
        });

        assert_eq!(
            run_segment(&status),
            format!(
                "#{ISSUE} {STEP} · {} · {CALLS} calls · ${COST:.2} · 2/6 Map the code",
                progress::short_duration(ELAPSED as i64),
            )
        );

        // No checklist names an item: the position alone, and no part
        // when there is no checklist at all.
        let aigentic_api::RunPhase::Step { checklist, .. } = &mut status.phase else {
            unreachable!("a_step builds a step");
        };
        checklist.as_mut().unwrap().active = None;
        assert!(run_segment(&status).ends_with(" · 2/6"));
        let aigentic_api::RunPhase::Step { checklist, .. } = &mut status.phase else {
            unreachable!("a_step builds a step");
        };
        *checklist = None;
        assert_eq!(
            run_segment(&status),
            format!(
                "#{ISSUE} {STEP} · {} · {CALLS} calls · ${COST:.2}",
                progress::short_duration(ELAPSED as i64),
            )
        );
    }

    #[test]
    fn the_segment_names_the_gate_and_the_move() {
        let gate = aigentic_api::RunStatus {
            issue: 139,
            phase: aigentic_api::RunPhase::Gate {
                gate: "decide".into(),
            },
            idle_secs: 0,
        };
        assert_eq!(run_segment(&gate), "#139 waiting at decide");

        let mov = aigentic_api::RunStatus {
            issue: 139,
            phase: aigentic_api::RunPhase::Move {
                what: "running the checks".into(),
            },
            idle_secs: 0,
        };
        assert_eq!(run_segment(&mov), "#139 running the checks");
        // A move is a sentence, not a step: it carries no clock.
        assert!(!run_segment(&mov).contains("calls"));
    }

    #[test]
    fn inside_is_drawn_and_nothing_else_changes() {
        const ISSUE: u64 = 140;
        let step_fixture = a_step(ISSUE, 192, 72, 0.51);
        let figures = run_figures(&step_fixture);
        let inside = format!("inside #{ISSUE} {STEP}");

        // Nothing changes while no watch is open: the line is exactly the
        // one the front thread draws today.
        let front = Status {
            project: "vendela".into(),
            mode: "manual".into(),
            ..Status::default()
        };
        assert_eq!(front.line(), "vendela · manual");
        assert!(!front.line().contains(&inside));

        // Inside, the watch's segment leads, the run's figures follow
        // bare, and `/back` is last.
        let watching = Status {
            project: "vendela".into(),
            mode: "manual".into(),
            run: Some(figures.clone()),
            inside: Some(inside.clone()),
            ..Status::default()
        };
        assert_eq!(
            watching.line(),
            format!("{inside} · {figures} · /back"),
            "the run's head is not drawn twice"
        );

        // A watch whose run has pushed no figures yet: the segment and
        // the way back, and nothing invented between them.
        let quiet = Status {
            inside: Some(inside.clone()),
            ..Status::default()
        };
        assert_eq!(quiet.line(), format!("{inside} · /back"));

        // A followed run of another issue is not the watched one: the
        // shell hands the watch's own figures, and a status for another
        // issue never becomes one.
        let other = a_step(ISSUE + 1, 10, 1, 0.0);
        let watched = crate::app::watch::figures_after(ISSUE, None, &step_fixture);
        assert_eq!(watched.as_deref(), Some(figures.as_str()));
        assert_eq!(
            crate::app::watch::figures_after(ISSUE, watched.as_deref(), &other),
            watched,
            "another issue's run is not the watched one"
        );
        let without = Status {
            run: crate::app::watch::figures_after(ISSUE, None, &other),
            inside: Some(inside.clone()),
            ..Status::default()
        };
        assert_eq!(without.line(), format!("{inside} · /back"));
    }

    #[test]
    fn the_figures_are_the_run_segment_without_the_head() {
        const ISSUE: u64 = 140;
        let step_fixture = a_step(ISSUE, 192, 72, 0.51);
        assert_eq!(
            run_segment(&step_fixture),
            format!("#{ISSUE} {STEP} · {}", run_figures(&step_fixture))
        );

        let gate_fixture = aigentic_api::RunStatus {
            issue: ISSUE,
            phase: aigentic_api::RunPhase::Gate { gate: GATE.into() },
            idle_secs: 0,
        };
        assert_eq!(
            run_segment(&gate_fixture),
            format!("#{ISSUE} waiting at {GATE}")
        );
        assert_eq!(run_figures(&gate_fixture), format!("waiting at {GATE}"));
    }

    #[test]
    fn inside_is_distinguishable_by_sight() {
        const ISSUE: u64 = 140;
        let step_fixture = a_step(ISSUE, 192, 72, 0.51);
        let figures = run_figures(&step_fixture);
        let inside = format!("inside #{ISSUE} {STEP}");

        let watching = Status {
            run: Some(figures.clone()),
            inside: Some(inside.clone()),
            ..Status::default()
        };
        assert!(watching.line().contains(&figures));

        // The two configurations the shell builds: with a watch, the
        // watch's line; without one, the front thread's byte for byte.
        let front = Status {
            project: "vendela".into(),
            mode: "manual".into(),
            run: Some(figures.clone()),
            ..Status::default()
        };
        assert_eq!(front.line(), format!("vendela · manual · {figures}"));
        assert!(!front.line().contains(&inside));
    }

    #[test]
    fn a_step_quiet_for_two_minutes_says_so() {
        let at = |idle_secs| aigentic_api::RunStatus {
            idle_secs,
            ..a_step(139, 192, 72, 0.51)
        };

        assert!(
            !run_segment(&at(QUIET_AFTER_SECS - 1)).contains("quiet"),
            "just under the threshold stays silent"
        );
        for idle_secs in [QUIET_AFTER_SECS, 900] {
            assert!(
                run_segment(&at(idle_secs)).ends_with(&format!(" · quiet {}m", idle_secs / 60)),
                "quiet at {idle_secs}s"
            );
        }
    }

    #[test]
    fn the_line_names_the_profile_and_model_and_says_the_effort_when_one_is_set() {
        let identity = |profile: Option<&str>, model: &str, effort: Option<&str>| Identity {
            profile: profile.map(str::to_owned),
            model: model.to_owned(),
            effort: effort.map(str::to_owned),
        };
        let mut s = Status {
            project: "vendela".into(),
            identity: Some(identity(Some("flash"), "deepseek-v4.1-flash", None)),
            mode: "auto".into(),
            usage: Some(Figures {
                working: 76_000,
                thread: Some(310_000),
            }),
            ..Status::default()
        };
        assert_eq!(
            s.line(),
            "vendela · flash (deepseek-v4.1-flash) · auto · working 76k · thread 310k"
        );
        // The effort rides the model when the profile sets one.
        s.identity = Some(identity(Some("flash"), "deepseek-v4.1-flash", Some("50")));
        assert_eq!(
            s.line(),
            "vendela · flash (deepseek-v4.1-flash) · effort 50 · auto · working 76k · thread 310k"
        );
        // A thread with no profile names the model alone.
        s.identity = Some(identity(None, "deepseek-v4.1-flash", None));
        assert_eq!(
            s.line(),
            "vendela · deepseek-v4.1-flash · auto · working 76k · thread 310k"
        );
        // A daemon that knows no model leaves the line as it was.
        s.identity = Some(identity(None, "unknown", None));
        assert_eq!(s.line(), "vendela · auto · working 76k · thread 310k");
        s.identity = None;
        assert_eq!(s.line(), "vendela · auto · working 76k · thread 310k");
    }

    /// The model's own label: profile and model in one name, the effort
    /// after it, and nothing for a stand-in or an empty model.
    #[test]
    fn the_identity_label() {
        let who = |profile: Option<&str>, model: &str, effort: Option<&str>| {
            Identity {
                profile: profile.map(str::to_owned),
                model: model.to_owned(),
                effort: effort.map(str::to_owned),
            }
            .label()
        };
        assert_eq!(
            who(Some("flash"), "deepseek-v4.1-flash", None).as_deref(),
            Some("flash (deepseek-v4.1-flash)")
        );
        assert_eq!(
            who(Some("kimi"), "k3", Some("max")).as_deref(),
            Some("kimi (k3) · effort max")
        );
        assert_eq!(
            who(None, "gpt-5", Some("low")).as_deref(),
            Some("gpt-5 · effort low")
        );
        assert_eq!(who(None, "unknown", Some("low")), None);
        assert_eq!(who(Some("flash"), "", None), None);
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
        s.apply_state(&ThreadState::AwaitingSwitch {
            call_id: "c".into(),
            project: "there".into(),
            workspace: Some("~/there".into()),
            reason: "belongs there".into(),
        });
        assert_eq!(s.waiting, Some("awaiting switch"));
        s.apply_state(&ThreadState::Idle);
        assert_eq!(s.queued, 0);
        assert_eq!(s.waiting, None);
    }
}
