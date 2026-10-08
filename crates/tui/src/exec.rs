//! `aigentic exec` (phase 6 step 2, docs/PLAN-phase6.md section 8): one
//! prompt, run to the end with no prompt ever shown. Progress goes to
//! stderr, the final assistant message to stdout; `--json` prints every
//! notice as one JSON line instead, then a summary line. A request the
//! rules would ask about is denied and returned to the model, and the
//! exit code says so.

use std::io::Write;
use std::path::PathBuf;

use aigentic_api::client::Client;
use aigentic_api::{Notice, Request, Response, SwitchReply, ThreadState};
use aigentic_runtime::aigentic_core::{ContentBlock, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, ProviderRetriedPayload, ToolResultPayload, TurnEndedPayload,
};
use anyhow::bail;
use tokio::sync::mpsc;
use ulid::Ulid;

use crate::app::engine::{TurnEvents, turn_end_report};

/// The turn ended `done` and nothing was denied.
pub const EXIT_OK: i32 = 0;
/// A budget stop, a provider error, or no turn at all.
pub const EXIT_FAILED: i32 = 1;
/// The turn ended `done`, but at least one request needed a human.
pub const EXIT_NEEDS_HUMAN: i32 = 3;
/// Interrupted: Ctrl-C here, or someone else's interrupt.
pub const EXIT_INTERRUPTED: i32 = 130;

/// The answer to an `ask_human` question when nobody is there.
pub const NO_HUMAN: &str = "No human is available: this is a non-interactive run (aigentic exec). Continue without an answer, or stop and say what you needed.";

/// Why a permission request is denied here, told to the model.
/// What `exec` tells the runtime when a proposal reaches it: nobody is
/// there to answer, so the proposal is closed rather than left open
/// (issue #7).
pub const SWITCH_DECLINED: &str = "exec declines proposals";

pub const NON_INTERACTIVE: &str =
    "this is a non-interactive run (aigentic exec) and nobody can approve";

/// The line `exec` prints when it declines a switch proposal.
pub fn switch_declined_line(project: &str) -> String {
    format!("[switch to {project} not answered: non-interactive]")
}

/// How many result lines the progress on stderr shows per tool call.
const RESULT_LINES: usize = 3;

#[derive(Debug, Clone)]
pub struct ExecArgs {
    pub prompt: String,
    pub json: bool,
    pub output_last: Option<PathBuf>,
}

/// What the run came to; `code` is the process's exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub code: i32,
    pub reason: Option<String>,
    pub denied: u32,
    pub last_message: String,
}

/// The prompt: the argument, else all of stdin.
pub fn prompt_from(arg: Option<String>) -> anyhow::Result<String> {
    let prompt = match arg {
        Some(p) => p,
        None => std::io::read_to_string(std::io::stdin())?,
    };
    if prompt.trim().is_empty() {
        bail!("no prompt: pass one as an argument or on stdin");
    }
    Ok(prompt)
}

/// Post the prompt to `thread` and follow it to idle. `out` receives the
/// final message (or the JSON lines), `err` the progress.
pub async fn run(
    client: &Client,
    mut notices: mpsc::Receiver<Notice>,
    thread: Ulid,
    initial: &ThreadState,
    args: &ExecArgs,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<ExecOutcome> {
    match client
        .request(Request::Post {
            thread,
            blocks: vec![ContentBlock::Text(args.prompt.clone())],
            interrupt: false,
        })
        .await?
    {
        Response::Ok => {}
        Response::Refused { reason } => bail!("the prompt was refused: {reason}"),
        other => bail!("unexpected reply posting the prompt: {other:?}"),
    }

    let mut follow = Follow {
        json: args.json,
        running: !matches!(initial, ThreadState::Idle),
        ..Follow::default()
    };
    let interrupted = loop {
        tokio::select! {
            notice = notices.recv() => {
                let Some(notice) = notice else {
                    writeln!(err, "[the daemon closed the connection]")?;
                    break false;
                };
                if follow.on(client, thread, notice, out, err).await? {
                    break false;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                // Recorded as an interrupt naming this user; the daemon
                // may go on to a short turn with the note, which this run
                // does not wait for.
                let _ = client
                    .request(Request::Post {
                        thread,
                        blocks: vec![ContentBlock::Text(
                            "[aigentic exec was interrupted with Ctrl-C: stop here]".into(),
                        )],
                        interrupt: true,
                    })
                    .await;
                break true;
            }
        }
    };

    let code = if interrupted {
        EXIT_INTERRUPTED
    } else {
        follow.code()
    };
    let outcome = ExecOutcome {
        code,
        reason: follow.reason.clone(),
        denied: follow.denied,
        last_message: follow.last_message.clone(),
    };
    if args.json {
        let summary = serde_json::json!({
            "kind": "exec_summary",
            "thread": thread.to_string(),
            "reason": outcome.reason,
            "denied": outcome.denied,
            "exit": outcome.code,
        });
        writeln!(out, "{summary}")?;
    } else {
        if !outcome.last_message.is_empty() {
            writeln!(out, "{}", outcome.last_message.trim_end())?;
        }
    }
    if let Some(path) = &args.output_last {
        std::fs::write(path, &outcome.last_message)?;
    }
    writeln!(
        err,
        "[exec: {} · {} request(s) denied · exit {} · thread {thread}]",
        outcome.reason.as_deref().unwrap_or("no turn ended"),
        outcome.denied,
        outcome.code
    )?;
    Ok(outcome)
}

/// The run's state while notices arrive.
#[derive(Default)]
struct Follow {
    json: bool,
    /// A turn has been seen running; the next `Idle` ends the run.
    running: bool,
    /// Streamed text on stderr that has not ended its line.
    open_line: bool,
    denied: u32,
    /// The last `turn_ended` reason.
    reason: Option<String>,
    last_message: String,
    /// The open turn's own events, for the turn-end report (issue #113),
    /// under the same bound the engine keeps (`app::engine::TurnEvents`).
    turn: TurnEvents,
}

impl Follow {
    /// One notice; `true` when the run is over.
    async fn on(
        &mut self,
        client: &Client,
        thread: Ulid,
        notice: Notice,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> anyhow::Result<bool> {
        if self.json {
            writeln!(out, "{}", serde_json::to_string(&notice)?)?;
        }
        match notice {
            // The model a daemon names at attach (issue #43) is for the
            // shell's footer; plain exec prints what the model says.
            Notice::Model { .. } => {}
            Notice::TextDelta { text, .. } => {
                if !self.json {
                    write!(err, "{text}")?;
                    self.open_line = !text.ends_with('\n');
                }
            }
            Notice::ToolCallStarted { call, .. } => {
                self.end_line(err)?;
                if !self.json {
                    let args = call.args.to_string();
                    let args: String = args.chars().take(160).collect();
                    writeln!(err, "→ {} {args}", call.name)?;
                }
            }
            Notice::Event { event, .. } => {
                self.turn.push(&event);
                match event.kind {
                    EventKind::AssistantMessage => {
                        self.end_line(err)?;
                        if let Ok(p) =
                            serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                        {
                            let text: Vec<&str> = p
                                .blocks
                                .iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text(t) => Some(t.as_str()),
                                    _ => None,
                                })
                                .collect();
                            if !text.is_empty() {
                                self.last_message = text.join("\n");
                            }
                        }
                    }
                    EventKind::ToolResult if !self.json => {
                        if let Ok(ToolResultPayload { result, .. }) =
                            serde_json::from_value(event.payload.clone())
                        {
                            let marker = if result.is_error { "✗" } else { "✓" };
                            let lines: Vec<&str> = result.content.lines().collect();
                            for line in lines.iter().take(RESULT_LINES) {
                                writeln!(err, "  {marker} {line}")?;
                            }
                            if lines.len() > RESULT_LINES {
                                writeln!(err, "  … +{} lines", lines.len() - RESULT_LINES)?;
                            }
                        }
                    }
                    EventKind::TurnEnded => {
                        if let Ok(p) = serde_json::from_value::<TurnEndedPayload>(event.payload) {
                            // What the turn did and what is left (issue
                            // #113), ahead of the final `[exec: …]` line
                            // and never under `--json`, whose consumers
                            // read the notices.
                            if !self.json {
                                self.end_line(err)?;
                                for line in turn_end_report(&p, self.turn.as_slice()) {
                                    writeln!(err, "{line}")?;
                                }
                            }
                            self.reason = Some(p.reason);
                        }
                    }
                    // A retry (issue #90): the turn is waiting out a
                    // provider outage, so a watcher sees it on stderr
                    // rather than a run that looks hung.
                    EventKind::ProviderRetried if !self.json => {
                        self.end_line(err)?;
                        if let Ok(p) =
                            serde_json::from_value::<ProviderRetriedPayload>(event.payload)
                        {
                            writeln!(
                                err,
                                "[retrying {}/{} · {} · next in {}]",
                                p.attempt,
                                p.retries,
                                p.reason,
                                crate::app::status::elapsed_short(
                                    std::time::Duration::from_millis(p.wait_ms,)
                                )
                            )?;
                        }
                    }
                    _ => {}
                }
            }
            Notice::State { state, .. } => match state {
                ThreadState::Idle if self.running => {
                    self.end_line(err)?;
                    return Ok(true);
                }
                ThreadState::Idle => {}
                ThreadState::Running { .. } => self.running = true,
                ThreadState::AwaitingApproval { call_id, call, .. } => {
                    self.running = true;
                    self.end_line(err)?;
                    self.denied += 1;
                    writeln!(err, "[denied {}: non-interactive]", call.name)?;
                    client
                        .request(Request::Decide {
                            thread,
                            call_id,
                            allow: false,
                            session: false,
                            prefix: None,
                            reason: Some(NON_INTERACTIVE.into()),
                        })
                        .await?;
                }
                ThreadState::AwaitingHuman {
                    call_id, question, ..
                } => {
                    self.running = true;
                    self.end_line(err)?;
                    self.denied += 1;
                    writeln!(err, "[question unanswered: {question}]")?;
                    client
                        .request(Request::AnswerHuman {
                            thread,
                            call_id,
                            text: NO_HUMAN.into(),
                        })
                        .await?;
                }
                ThreadState::AwaitingSwitch {
                    call_id, project, ..
                } => {
                    self.running = true;
                    self.end_line(err)?;
                    self.denied += 1;
                    writeln!(err, "{}", switch_declined_line(&project))?;
                    client
                        .request(Request::AnswerSwitch {
                            thread,
                            call_id,
                            answer: SwitchReply::Withdrawn {
                                note: SWITCH_DECLINED.into(),
                            },
                        })
                        .await?;
                }
            },
            Notice::Note { text, .. } => {
                self.end_line(err)?;
                writeln!(err, "[{text}]")?;
            }
            Notice::Mode { .. } | Notice::Usage { .. } => {}
        }
        Ok(false)
    }

    fn end_line(&mut self, err: &mut dyn Write) -> std::io::Result<()> {
        if self.open_line {
            writeln!(err)?;
            self.open_line = false;
        }
        Ok(())
    }

    fn code(&self) -> i32 {
        match self.reason.as_deref() {
            Some("done") if self.denied > 0 => EXIT_NEEDS_HUMAN,
            Some("done") => EXIT_OK,
            Some(r) if r == aigentic_runtime::INTERRUPTED => EXIT_INTERRUPTED,
            _ => EXIT_FAILED,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::cells::summarise_args;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use aigentic_api::client::Addr;
    use aigentic_runtime::aigentic_core::{
        Capabilities, CompletionRequest, Message, Provider, ProviderEvent, ToolCall,
    };
    use aigentic_server::build::{BuildError, ProviderFactory};
    use aigentic_server::config::{Config, ProjectConfig, ServerConfig, UserConfig};
    use aigentic_server::{Listener, NoReports, Server};
    use futures_util::Stream;

    struct Scripted(Mutex<VecDeque<Vec<ProviderEvent>>>);

    impl Provider for Scripted {
        fn complete(
            &self,
            _: &CompletionRequest<'_>,
        ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
            match self.0.lock().unwrap().pop_front() {
                Some(events) => Box::pin(futures_util::stream::iter(events)),
                None => Box::pin(futures_util::stream::pending()),
            }
        }
        fn count_tokens(&self, _: &[Message]) -> u64 {
            7
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tools: true,
                supports_images: false,
                supports_caching: false,
                supports_structured_output: false,
                max_context_tokens: 1000,
            }
        }
    }

    struct Factory(Mutex<Option<Vec<Vec<ProviderEvent>>>>);

    impl ProviderFactory for Factory {
        fn build(&self, _: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
            let script = self.0.lock().unwrap().take().unwrap_or_default();
            Ok((
                Box::new(Scripted(Mutex::new(script.into()))),
                "scripted".into(),
            ))
        }
    }

    fn text(t: &str) -> ProviderEvent {
        ProviderEvent::TextDelta(t.into())
    }
    fn finish(reason: &str) -> ProviderEvent {
        ProviderEvent::Done {
            finish_reason: reason.into(),
        }
    }

    /// A daemon with one project `p` and user steve, and a thread in it.
    async fn session(
        script: Vec<Vec<ProviderEvent>>,
    ) -> (
        tempfile::TempDir,
        Client,
        mpsc::Receiver<Notice>,
        Ulid,
        ThreadState,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("p");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("aigentic.toml"), "[project]\nname = \"p\"\n").unwrap();
        let cfg_dir = dir.path().join("cfg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let config = Config::parse(&format!(
            "threads_dir = {:?}\nbundled_dir = {:?}\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n",
            dir.path().join("threads").display(),
            dir.path().display()
        ))
        .unwrap();
        let server = ServerConfig {
            listen: "unix".into(),
            idle_unload_secs: 60,
            resume_runs: false,
            users: vec![UserConfig {
                name: "steve".into(),
                token_env: None,
                token: Some("tok".into()),
            }],
            projects: vec![ProjectConfig {
                name: "p".into(),
                root: p,
            }],
        };
        let server = Arc::new(Server::new(
            config,
            cfg_dir,
            server,
            Arc::new(Factory(Mutex::new(Some(script)))),
            Arc::new(NoReports),
        ));
        let socket = dir.path().join("d.sock");
        tokio::spawn(server.serve(Listener::Unix(socket.clone())));
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let (client, _) = Client::connect(&Addr::Unix(socket), "tok").await.unwrap();
        let Response::Thread { thread } = client
            .request(Request::CreateThread {
                project: "p".into(),
            })
            .await
            .unwrap()
        else {
            panic!("no thread")
        };
        let Response::Opened { state, .. } = client
            .request(Request::Open {
                thread: thread.id,
                from_seq: 0,
            })
            .await
            .unwrap()
        else {
            panic!("not opened")
        };
        let notices = client.take_notices().unwrap();
        (dir, client, notices, thread.id, state)
    }

    fn args(json: bool) -> ExecArgs {
        ExecArgs {
            prompt: "hi".into(),
            json,
            output_last: None,
        }
    }

    async fn exec(
        script: Vec<Vec<ProviderEvent>>,
        args: ExecArgs,
    ) -> (ExecOutcome, String, String) {
        let (_dir, client, notices, thread, state) = session(script).await;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            run(&client, notices, thread, &state, &args, &mut out, &mut err),
        )
        .await
        .expect("exec finishes")
        .unwrap();
        (
            outcome,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[tokio::test]
    async fn the_final_message_goes_to_stdout_and_exit_is_zero() {
        let (o, out, err) = exec(
            vec![vec![text("all "), text("done"), finish("stop")]],
            args(false),
        )
        .await;
        assert_eq!(o.code, EXIT_OK);
        assert_eq!(o.reason.as_deref(), Some("done"));
        assert_eq!(out, "all done\n");
        assert!(err.contains("all done"), "{err}");
        assert!(err.contains("exit 0"), "{err}");
    }

    #[tokio::test]
    async fn a_request_that_would_ask_is_denied_and_exit_is_three() {
        // `curl` is not on the default bash allow list: manual mode asks.
        let call = ProviderEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            args: serde_json::json!({"command": "curl -s https://example.com"}),
        });
        let (o, out, err) = exec(
            vec![
                vec![call, finish("tool_use")],
                vec![text("could not fetch"), finish("stop")],
            ],
            args(false),
        )
        .await;
        assert_eq!(o.denied, 1, "{err}");
        assert_eq!(o.code, EXIT_NEEDS_HUMAN, "{err}");
        assert!(err.contains("[denied bash: non-interactive]"), "{err}");
        assert_eq!(out, "could not fetch\n");
    }

    /// T11: a switch proposal is declined with `withdrawn`, the line is
    /// printed, the turn continues in place, and the exit code is three.
    #[tokio::test]
    async fn a_switch_proposal_is_declined_and_exit_is_three() {
        let call = ProviderEvent::ToolCall(ToolCall {
            id: "c1".into(),
            name: "suggest_project".into(),
            args: serde_json::json!({"project": "q", "reason": "belongs elsewhere"}),
        });
        let (o, out, err) = exec(
            vec![
                vec![call, finish("tool_use")],
                vec![text("staying put"), finish("stop")],
            ],
            args(false),
        )
        .await;
        assert_eq!(o.denied, 1, "{err}");
        assert_eq!(o.code, EXIT_NEEDS_HUMAN, "{err}");
        assert!(err.contains(&switch_declined_line("q")), "{err}");
        assert_eq!(
            out, "staying put\n",
            "the turn continued in place: nothing waited for a person"
        );
        assert_eq!(o.reason.as_deref(), Some("done"), "{err}");
    }

    #[tokio::test]
    async fn json_prints_notices_then_a_summary() {
        let (o, out, _) = exec(vec![vec![text("ok"), finish("stop")]], args(true)).await;
        assert_eq!(o.code, EXIT_OK);
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line is JSON"))
            .collect();
        assert!(lines.iter().any(|l| l["kind"] == "text_delta"));
        let last = lines.last().unwrap();
        assert_eq!(last["kind"], "exec_summary");
        assert_eq!(last["exit"], 0);
        assert_eq!(last["denied"], 0);
    }

    /// Issue #90 (T8): a retry reaches a non-interactive watcher's stderr
    /// as one line, so an `exec` run during an outage reads as waiting
    /// rather than hung. The line's wait is rendered by the same helper
    /// the tui's turn line uses.
    #[tokio::test]
    async fn a_retry_is_printed_on_stderr() {
        let wait = Duration::from_secs(8);
        let retried = ProviderEvent::Retried {
            attempt: 2,
            retries: 14,
            reason: "overloaded".into(),
            wait,
        };
        let (o, out, err) = exec(
            vec![vec![retried, text("recovered"), finish("stop")]],
            args(false),
        )
        .await;
        assert_eq!(o.code, EXIT_OK);
        assert_eq!(out, "recovered\n");
        let rendered = crate::app::status::elapsed_short(wait);
        assert!(err.contains("[retrying 2/14 · "), "{err}");
        assert!(err.contains("overloaded"), "{err}");
        assert!(err.contains(&format!("next in {rendered}]")), "{err}");

        // `--json` already carries every notice, so the stderr line is
        // for a person watching, not a parser.
        let (o, _, err) = exec(
            vec![vec![
                ProviderEvent::Retried {
                    attempt: 2,
                    retries: 14,
                    reason: "overloaded".into(),
                    wait,
                },
                text("recovered"),
                finish("stop"),
            ]],
            args(true),
        )
        .await;
        assert_eq!(o.code, EXIT_OK);
        assert!(!err.contains("[retrying"), "{err}");
    }

    /// T4 (#113): a scripted provider that makes two tool calls and
    /// then fails. The report the engine draws reaches `exec`'s stderr
    /// too, ahead of the final `[exec: …]` line, and never under
    /// `--json`, whose consumers read the notices.
    #[tokio::test]
    async fn a_failed_turn_reports_what_it_did_before_the_exec_line() {
        let read = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::json!({"path": "aigentic.toml"}),
        };
        let list = ToolCall {
            id: "c2".into(),
            name: "list_dir".into(),
            args: serde_json::json!({"path": "."}),
        };
        let failed = ProviderEvent::Error(
            aigentic_runtime::aigentic_core::ProviderError::Transport("connection closed".into()),
        );
        let script = || {
            vec![
                vec![ProviderEvent::ToolCall(read.clone()), finish("tool_use")],
                vec![ProviderEvent::ToolCall(list.clone()), finish("tool_use")],
                vec![failed.clone()],
            ]
        };
        let (o, _, err) = exec(script(), args(false)).await;
        assert_eq!(o.code, EXIT_FAILED, "{err}");
        let did = format!(
            "[before that: read_file {} · list_dir {}]",
            summarise_args(&read),
            summarise_args(&list)
        );
        let stop = err
            .lines()
            .position(|l| l.starts_with("[turn ended: the model's reply failed:"))
            .unwrap_or_else(|| panic!("no stop line in {err}"));
        assert!(
            err.lines()
                .nth(stop)
                .unwrap()
                .contains("(/why shows the raw error)")
        );
        let tail: Vec<&str> = err.lines().skip(stop).collect();
        assert_eq!(tail[1], did, "{err}");
        assert_eq!(tail[2], "[if that wasn't the end, type continue]", "{err}");
        let last = err.lines().last().unwrap();
        assert!(last.starts_with("[exec: "), "{err}");

        // `--json` carries every notice already; a person's lines stay off it.
        let (o, _, err) = exec(script(), args(true)).await;
        assert_eq!(o.code, EXIT_FAILED, "{err}");
        assert!(!err.contains("before that:"), "{err}");
        assert!(!err.contains("turn ended:"), "{err}");
    }

    /// A budget stop or a provider error at the end of a run reads as a
    /// failure on stderr; the report lines are printed before that line
    /// and never change the exit code (issue #113).
    #[tokio::test]
    async fn a_budget_or_provider_stop_is_a_failure() {
        let f = Follow {
            reason: Some("max_iterations".into()),
            ..Follow::default()
        };
        assert_eq!(f.code(), EXIT_FAILED);
        let f = Follow::default();
        assert_eq!(f.code(), EXIT_FAILED);
    }
}
