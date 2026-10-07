//! `aigentic status`: what the running threads are doing right now
//! (issue #109). Read straight from the logs, like `stats`, so it needs
//! no daemon and is always safe to run from a second terminal.
//!
//! One line per live thread: what it is, how long its turn has run, its
//! calls and cost so far, its checklist, and what it is doing now. Every
//! figure is a projection of the events the runtime already writes — a
//! checklist is not an event of its own (the `update_tasks` call's
//! arguments are the record), and a turn start is a `user_message` that
//! did not arrive mid-turn, whatever its author, so a runner's step
//! prompt counts.
//!
//! Two things the log cannot say and status therefore does not:
//!
//! - **it cannot tell a crash from a long tool call.** A turn whose last
//!   event is over half an hour old is `stale`; only a resume clears it;
//! - **it does not show `working`/`thread` tokens.** Those need the
//!   daemon's provider and its window (`Notice::Usage`), so they stay on
//!   the REPL's status line.
//!
//! The **`--json` shape is stable**: one object per listed thread, with
//! `id`, `title`, `state`, `turn`, `elapsed_secs`, `idle_secs`, `calls`,
//! `spent`, `estimated_spent`, `unpriced`, `checklist`, `now` and `last`.

use std::path::Path;

use aigentic_runtime::aigentic_core::{ContentBlock, Event, EventKind, ToolCall};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, CheckpointAskedPayload, MemoryExtractedPayload,
    PermissionDecidedPayload, PermissionRequestedPayload, ThreadRenamedPayload, ToolResultPayload,
    TurnEndedPayload, Usage, UserMessagePayload,
};
use aigentic_runtime::harness_tools::{ASK_HUMAN, open_checklist};
use aigentic_runtime::title::title_of;
use aigentic_server::workspaces::Workspace;
use serde::Serialize;
use time::{Duration, OffsetDateTime};

use crate::project_cmd::first_line_of;
use crate::stats::{Accum, Cost, PriceBook, SideJob, classify_cost, money};
use crate::threads_index;

/// An open turn whose last event is younger than this is live.
const LIVE_WITHIN: Duration = Duration::minutes(30);
/// An ended turn older than this is not listed, even with `--all`.
const ENDED_WITHIN: Duration = Duration::hours(1);
/// A log older than this is not read at all: a cheap reason not to open
/// a file, never a liveness signal (every append syncs the log). A turn
/// parked for over a day is the accepted miss.
const READ_WITHIN: Duration = Duration::hours(24);
/// `last event Ns ago` is only worth a field once it is over this.
const IDLE_SHOWN: Duration = Duration::seconds(60);

/// A title is cut to this many characters.
const TITLE_CHARS: usize = 40;
/// A tool call's arguments are cut to this many.
const ARGS_CHARS: usize = 60;

/// Where a thread stands, from its log alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// An open turn that wrote something in the last half hour.
    Live,
    /// Parked on a `permission_requested` nobody answered.
    WaitingForApproval,
    /// Parked on an `ask_human` call nobody answered.
    WaitingForAnAnswer,
    /// Parked on a `checkpoint_asked` nobody answered.
    WaitingAtACheckpoint,
    /// An open turn gone quiet: killed mid-turn, most likely.
    Stale,
    /// The thread's last turn ended.
    Ended,
}

impl State {
    /// The JSON token, `snake_case`, stable.
    fn slug(self) -> &'static str {
        match self {
            State::Live => "live",
            State::WaitingForApproval => "waiting_for_approval",
            State::WaitingForAnAnswer => "waiting_for_an_answer",
            State::WaitingAtACheckpoint => "waiting_at_a_checkpoint",
            State::Stale => "stale",
            State::Ended => "ended",
        }
    }

    /// The words on the line. A parked turn is live whatever its age.
    fn text(self, ended_reason: Option<&str>) -> String {
        match self {
            State::Ended => format!("ended ({})", ended_reason.unwrap_or("unknown")),
            other => other.slug().replace('_', " "),
        }
    }
}

impl Serialize for State {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.slug())
    }
}

/// A tool call on the line: `now: bash cargo test`.
#[derive(Debug, Serialize)]
pub struct Call {
    pub tool: String,
    pub args: String,
    /// Calls in flight in the same reply, this one counted: `+N` on the
    /// line is `in_flight - 1`.
    pub in_flight: u32,
}

/// The checklist, without the reminder's own call count.
#[derive(Debug, Serialize)]
pub struct ChecklistView {
    pub done: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
}

/// One listed thread, and the whole `--json` shape.
#[derive(Debug, Serialize)]
pub struct Progress {
    pub id: String,
    pub title: String,
    pub state: State,
    /// Why the last turn ended, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    /// The open turn's ordinal among the thread's turn starts, 1-based.
    pub turn: u32,
    /// The open turn's age; for an ended turn, its own duration.
    pub elapsed_secs: i64,
    /// How long ago the last event was written.
    pub idle_secs: i64,
    /// Tool calls in the open turn.
    pub calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_spent: Option<f64>,
    /// Usage lines in the turn nothing could price, kept apart from the
    /// two dollars above.
    pub unpriced: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checklist: Option<ChecklistView>,
    /// The last call with no result yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now: Option<Call>,
    /// The latest completed call, when nothing is in flight.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<Call>,
    /// The sort key: the last event's time. Not part of the shape.
    #[serde(skip)]
    last_at: OffsetDateTime,
}

/// What `status` found, and what `--json` prints.
#[derive(Debug, Default)]
pub struct Status {
    /// The listed threads, newest activity first.
    pub threads: Vec<Progress>,
    /// Open turns that have gone quiet, listed or not: the hint the
    /// default listing gives instead of hiding them.
    pub stale: usize,
    /// Logs that could not be read: counted, never swallowed, the way
    /// `stats` counts them.
    pub unreadable: u32,
}

/// `aigentic status [--all] [--json]`. `base` is the threads directory,
/// `project` the global `--project` (the global `--thread` is not read),
/// and `now` the one clock every age and both windows come from.
pub fn run(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    all: bool,
    json: bool,
    now: OffsetDateTime,
    book: &PriceBook,
) -> anyhow::Result<()> {
    let status = collect(base, workspaces, project, all, now, book)?;
    if json {
        println!("{}", as_json(&status)?);
    } else {
        print!("{}", render(&status));
    }
    Ok(())
}

/// The stable `--json` shape: an array of one object per listed thread.
pub fn as_json(status: &Status) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(&status.threads)?)
}

/// Walk the catalogue and fold every log that is worth reading.
pub fn collect(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    all: bool,
    now: OffsetDateTime,
    book: &PriceBook,
) -> anyhow::Result<Status> {
    let mut status = Status::default();
    for found in threads_index::catalogue(base) {
        // The mtime is a cheap reason not to read a file. A log we cannot
        // stat at all is read: a local command never hides a thread over
        // a stat error.
        let fresh = match std::fs::metadata(found.path()).and_then(|m| m.modified()) {
            Ok(at) => OffsetDateTime::from(at) > now - READ_WITHIN,
            Err(_) => true,
        };
        if !fresh {
            continue;
        }
        let Ok(events) = found.read() else {
            // An unreadable log has no lines to attribute it by, so it
            // counts whenever the filter cannot be shown to exclude it.
            if project.is_none() || found.legacy.as_deref() == project {
                status.unreadable += 1;
            }
            continue;
        };
        let name = threads_index::project_of(&events, found.legacy.as_deref(), workspaces);
        if project.is_some_and(|want| Some(want) != name.as_deref()) {
            continue;
        }
        // The fold runs with `--all`, so a stale thread is *counted*
        // even when it is not listed: the hint that follows the empty
        // listing is the difference between "nothing running" and
        // "nothing running, and something died here".
        let Some(progress) = progress(&events, now, true, book) else {
            continue;
        };
        if progress.state == State::Stale {
            status.stale += 1;
        }
        if !all && matches!(progress.state, State::Stale | State::Ended) {
            continue;
        }
        status.threads.push(Progress {
            id: found.id.to_string(),
            ..progress
        });
    }
    // Newest activity first, the same order in text and JSON.
    status.threads.sort_by_key(|p| std::cmp::Reverse(p.last_at));
    Ok(status)
}

fn last_turn_end(events: &[Event]) -> Option<usize> {
    events.iter().rposition(|e| e.kind == EventKind::TurnEnded)
}

/// A turn start: a `user_message` that did not arrive mid-turn, whatever
/// its author. A step prompt is `Author::Agent("runner")`; older logs
/// default `mid_turn` to false.
fn is_turn_start(event: &Event) -> bool {
    event.kind == EventKind::UserMessage
        && serde_json::from_value::<UserMessagePayload>(event.payload.clone())
            .is_ok_and(|p| !p.mid_turn)
}

/// The whole fold: the open turn, or — for `--all` — the last one that
/// ended. `None` when nothing is worth a line.
pub fn progress(
    events: &[Event],
    now: OffsetDateTime,
    all: bool,
    book: &PriceBook,
) -> Option<Progress> {
    let last_at = events.last()?.created_at;
    let starts: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_turn_start(e))
        .map(|(i, _)| i)
        .collect();
    let end = last_turn_end(events);
    let open = starts.iter().rposition(|&i| end.is_none_or(|end| i > end));

    // An open turn whose tail is `interrupted` (what `resume` writes)
    // has ended: nothing is waiting there any more. So has a turn whose
    // `turn_ended` is the last line. Either is a line of its own only
    // inside the hour, and only with `--all`.
    let interrupted = events
        .last()
        .is_some_and(|e| e.kind == EventKind::Interrupted);
    let ended = |reason: String, start: usize, turn: u32| -> Option<(usize, u32, Option<String>)> {
        (all && now - last_at <= ENDED_WITHIN).then_some((start, turn, Some(reason)))
    };
    let (start, turn, ended_reason) = match open {
        Some(pos) if interrupted => ended("interrupted".to_owned(), starts[pos], pos as u32 + 1)?,
        Some(pos) => (starts[pos], pos as u32 + 1, None),
        None => {
            let payload = events[end?].payload.clone();
            let reason = serde_json::from_value::<TurnEndedPayload>(payload)
                .map(|p| p.reason.split(':').next().unwrap_or("").to_owned())
                .unwrap_or_default();
            ended(reason, starts.last().copied()?, starts.len() as u32)?
        }
    };

    let window = &events[start..];
    // Parked or not, a turn that ended is not waiting on anyone.
    let state = if ended_reason.is_some() {
        State::Ended
    } else if let Some(parked) = parked_state(window) {
        parked
    } else if now - last_at < LIVE_WITHIN {
        State::Live
    } else if all {
        State::Stale
    } else {
        return None;
    };

    let ended = ended_reason.is_some();
    let tally = Tally::of(window, book);
    let checklist = open_checklist(events).map(|c| ChecklistView {
        done: c.done,
        total: c.total,
        active: c.active,
    });
    Some(Progress {
        // Filled by `collect`, which knows the id and the title.
        id: String::new(),
        title: title(events),
        state,
        ended_reason,
        turn,
        elapsed_secs: (if ended { last_at } else { now } - events[start].created_at)
            .whole_seconds(),
        idle_secs: (now - last_at).whole_seconds(),
        calls: tally.calls,
        spent: tally.spent,
        estimated_spent: tally.estimated,
        unpriced: tally.unpriced,
        checklist,
        now: tally.now,
        last: tally.last,
        last_at,
    })
}

/// The thread's title: the latest `thread_renamed`, else the first line
/// of its first turn start's text, cut to [`TITLE_CHARS`].
fn title(events: &[Event]) -> String {
    if let Some(name) = title_of(events) {
        return clip(&name, TITLE_CHARS);
    }
    let first = events
        .iter()
        .find(|e| is_turn_start(e))
        .and_then(|e| serde_json::from_value::<UserMessagePayload>(e.payload.clone()).ok())
        .map(|p| {
            p.blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    clip(&first_line_of(&first), TITLE_CHARS)
}

/// What the open turn is parked on: the latest unanswered of the three
/// ways a person is asked. Permission, a question and a checkpoint can
/// all be open; the last one written is what the thread waits on.
fn parked_state(window: &[Event]) -> Option<State> {
    let answered: Vec<String> = window
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .filter_map(|e| serde_json::from_value::<ToolResultPayload>(e.payload.clone()).ok())
        .map(|p| p.result.id)
        .collect();
    let decided: Vec<String> = window
        .iter()
        .filter(|e| e.kind == EventKind::PermissionDecided)
        .filter_map(|e| serde_json::from_value::<PermissionDecidedPayload>(e.payload.clone()).ok())
        .map(|p| p.call_id)
        .collect();

    let mut waiting: Vec<(usize, State)> = Vec::new();
    for (i, event) in window.iter().enumerate() {
        match event.kind {
            EventKind::AssistantMessage => {
                let Ok(p) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                else {
                    continue;
                };
                for block in &p.blocks {
                    if let ContentBlock::ToolCall(call) = block
                        && call.name == ASK_HUMAN
                        && !answered.contains(&call.id)
                    {
                        waiting.push((i, State::WaitingForAnAnswer));
                    }
                }
            }
            EventKind::PermissionRequested => {
                let Ok(p) =
                    serde_json::from_value::<PermissionRequestedPayload>(event.payload.clone())
                else {
                    continue;
                };
                if !decided.contains(&p.call.id) {
                    waiting.push((i, State::WaitingForApproval));
                }
            }
            _ => {}
        }
    }
    // A checkpoint answer clears the gate it answered (`RunState`'s own
    // rule), so only an ask after the last answer is open.
    let asked = window
        .iter()
        .rposition(|e| e.kind == EventKind::CheckpointAsked);
    let cleared = window
        .iter()
        .rposition(|e| e.kind == EventKind::CheckpointAnswered);
    if let Some(at) = asked
        && cleared.is_none_or(|done| at > done)
        && serde_json::from_value::<CheckpointAskedPayload>(window[at].payload.clone()).is_ok()
    {
        waiting.push((at, State::WaitingAtACheckpoint));
    }
    waiting.into_iter().max_by_key(|(i, _)| *i).map(|(_, s)| s)
}

/// The open turn's calls and dollars.
#[derive(Debug, Default)]
struct Tally {
    calls: u32,
    spent: Option<f64>,
    estimated: Option<f64>,
    unpriced: u32,
    now: Option<Call>,
    last: Option<Call>,
}

impl Tally {
    fn of(window: &[Event], book: &PriceBook) -> Self {
        let mut tally = Self::default();
        let mut calls: Vec<ToolCall> = Vec::new();
        let mut results: Vec<String> = Vec::new();
        // The dollars come from `stats`' own arithmetic: `Accum` sums the
        // calls and the side jobs the way its thread row does.
        let mut accum = Accum::default();

        for event in window {
            match event.kind {
                EventKind::AssistantMessage => {
                    let Ok(p) =
                        serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    for block in &p.blocks {
                        if let ContentBlock::ToolCall(call) = block {
                            calls.push(call.clone());
                        }
                    }
                    if let Some(usage) = &p.usage {
                        tally.price(&mut accum, usage, book, None);
                    }
                }
                EventKind::ToolResult => {
                    if let Ok(p) =
                        serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                    {
                        results.push(p.result.id);
                    }
                }
                EventKind::MemoryExtracted => {
                    let Ok(p) =
                        serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    let mut usage = p.usage;
                    // The payload's own `model` is the provider that ran
                    // the extraction, so a line from before the cost was
                    // stamped can still be priced by name.
                    if usage.model.is_none() {
                        usage.model = Some(p.model);
                    }
                    tally.price(&mut accum, &usage, book, Some(SideJob::Extraction));
                }
                EventKind::ThreadRenamed => {
                    let Ok(p) =
                        serde_json::from_value::<ThreadRenamedPayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    let Some(mut usage) = p.usage else { continue };
                    if usage.model.is_none() {
                        usage.model = p.model;
                    }
                    tally.price(&mut accum, &usage, book, Some(SideJob::Title));
                }
                _ => {}
            }
        }

        tally.calls = calls.len() as u32;
        tally.spent = accum.row_spent();
        tally.estimated = accum.row_estimated();

        let flight: Vec<&ToolCall> = calls.iter().filter(|c| !results.contains(&c.id)).collect();
        match flight.split_first() {
            Some((first, rest)) => {
                tally.now = Some(Call {
                    tool: first.name.clone(),
                    args: arguments(first),
                    in_flight: rest.len() as u32 + 1,
                })
            }
            None => {
                tally.last = calls.last().map(|c| Call {
                    tool: c.name.clone(),
                    args: arguments(c),
                    in_flight: 0,
                })
            }
        }
        tally
    }

    /// One usage line, into `stats`' own accumulator, and counted as
    /// unpriced when nothing could price it.
    fn price(&mut self, accum: &mut Accum, usage: &Usage, book: &PriceBook, job: Option<SideJob>) {
        let context = usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
        let cost = classify_cost(usage, book);
        if matches!(cost, Cost::Unpriced) {
            self.unpriced += 1;
        }
        match job {
            Some(job) => accum.add_job(job, context, usage.output_tokens, cost),
            None => accum.add_call(context, usage.cache_read_tokens, cost),
        }
    }
}

/// A call's arguments on the line: `bash`'s own command, first line,
/// because that is what a reader wants to see; anything else as its JSON,
/// cut to [`ARGS_CHARS`].
fn arguments(call: &ToolCall) -> String {
    let text = match call.args.get("command").and_then(|c| c.as_str()) {
        Some(command) if call.name == "bash" => command.to_owned(),
        _ => call.args.to_string(),
    };
    clip(&text, ARGS_CHARS)
}

/// The first `n` characters, with `…` when there were more.
fn clip(text: &str, n: usize) -> String {
    let mut out: String = text.chars().take(n).collect();
    if out.chars().count() < text.chars().count() {
        out.push('…');
    }
    out
}

/// A short duration: `42s`, `4m12s`, `2h05m`, `3d4h`.
fn short_duration(secs: i64) -> String {
    let secs = secs.max(0);
    let (days, hours, mins) = (secs / 86_400, (secs % 86_400) / 3_600, (secs % 3_600) / 60);
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{mins:02}m")
    } else if mins > 0 {
        format!("{mins}m{:02}s", secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// One line per thread, ` · ` between the fields, or the empty case.
pub fn render(status: &Status) -> String {
    if status.threads.is_empty() {
        return if status.stale > 0 {
            format!("nothing running · {} stale (status --all)\n", status.stale)
        } else {
            "nothing running\n".to_owned()
        };
    }
    let mut out = String::new();
    for thread in &status.threads {
        out.push_str(&line(thread));
        out.push('\n');
    }
    out
}

fn line(thread: &Progress) -> String {
    let mut parts = vec![
        format!(
            "{} {}",
            thread.id.chars().take(6).collect::<String>(),
            thread.title
        ),
        thread.state.text(thread.ended_reason.as_deref()),
        format!("turn {}", thread.turn),
        short_duration(thread.elapsed_secs),
    ];
    if thread.idle_secs > IDLE_SHOWN.whole_seconds() {
        parts.push(format!(
            "last event {} ago",
            short_duration(thread.idle_secs)
        ));
    }
    parts.push(format!("{} calls", thread.calls));
    parts.push(money(thread.spent, thread.estimated_spent));
    if let Some(checklist) = &thread.checklist {
        parts.push(match &checklist.active {
            Some(step) => format!("checklist {}/{}: {step}", checklist.done, checklist.total),
            None => format!("checklist {}/{}", checklist.done, checklist.total),
        });
    }
    if let Some(call) = &thread.now {
        let more = match call.in_flight {
            0 | 1 => String::new(),
            n => format!(" +{}", n - 1),
        };
        parts.push(format!("now: {} {}{more}", call.tool, call.args));
    } else if let Some(call) = &thread.last {
        parts.push(format!("last: {} {}", call.tool, call.args));
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::{Value, json};
    use time::format_description::well_known::Rfc3339;
    use ulid::Ulid;

    /// The same `[prices]` shape `stats`' tests use, so the two commands
    /// can answer one way about one line.
    const PRICED: &str = r#"
default_profile = "flash"
[profiles.flash]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "deepseek/deepseek-v4.1-flash"
api_key_env = "TENSORX_API_KEY"
[profiles.flash.prices]
input = 0.50
output = 1.50
cache_read = 0.13
cache_write = 0.50

[profiles.tensorx]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "z-ai/glm-5.3"
api_key_env = "TENSORX_API_KEY"
[profiles.tensorx.prices]
input = 1.75
output = 4.5
cache_read = 0.44
cache_write = 1.75
"#;

    fn book() -> PriceBook {
        PriceBook::from_config(&Config::parse(PRICED).unwrap(), None).unwrap()
    }

    /// A log written straight to disk, the way `stats`' fixtures do:
    /// `append` would stamp `created_at` itself and these need the past.
    fn write(base: &Path, id: Ulid, lines: &[Value]) {
        let text: String = lines
            .iter()
            .enumerate()
            .map(|(seq, line)| {
                let mut e = line.clone();
                e["id"] = json!(Ulid::generate().to_string());
                e["thread_id"] = json!(id.to_string());
                e["seq"] = json!(seq);
                format!("{e}\n")
            })
            .collect();
        std::fs::write(base.join(format!("{id}.jsonl")), text).unwrap();
    }

    /// The log read back the way `collect` reads it: the events the
    /// expectations are computed from.
    fn read(base: &Path, id: Ulid) -> Vec<Event> {
        threads_index::catalogue(base)
            .into_iter()
            .find(|f| f.id == id)
            .expect("the log just written")
            .read()
            .expect("a readable log")
    }

    fn at(stamp: &str) -> OffsetDateTime {
        OffsetDateTime::parse(stamp, &Rfc3339).unwrap()
    }

    /// The fixture's own last event, the clock every age is measured from.
    fn last_at(events: &[Event]) -> OffsetDateTime {
        events.last().unwrap().created_at
    }

    fn text_block(text: &str) -> Value {
        json!({"type": "text", "text": text})
    }

    fn call_block(name: &str, id: &str, args: Value) -> Value {
        json!({"type": "tool_call", "id": id, "name": name, "args": args})
    }

    fn user(stamp: &str, text: &str) -> Value {
        json!({
            "kind": "user_message",
            "author": {"kind": "user", "id": "steve"},
            "payload": {"blocks": [text_block(text)], "mid_turn": false},
            "created_at": stamp,
        })
    }

    /// A message the runtime queued inside a running turn (issue #33).
    fn mid_turn(stamp: &str, text: &str) -> Value {
        let mut line = user(stamp, text);
        line["payload"]["mid_turn"] = json!(true);
        line
    }

    /// A step thread's prompt: the runner's own message, no human.
    fn runner(stamp: &str, text: &str) -> Value {
        json!({
            "kind": "user_message",
            "author": {"kind": "agent", "id": "runner"},
            "payload": {"blocks": [text_block(text)], "mid_turn": false},
            "created_at": stamp,
        })
    }

    fn assistant(stamp: &str, blocks: Vec<Value>, usage: Option<Value>) -> Value {
        json!({
            "kind": "assistant_message",
            "author": {"kind": "agent", "id": "assistant"},
            "payload": {"blocks": blocks, "usage": usage},
            "created_at": stamp,
        })
    }

    /// The payload is flat: `ToolResultPayload` flattens `ToolResult`.
    fn result(stamp: &str, call_id: &str) -> Value {
        json!({
            "kind": "tool_result",
            "author": {"kind": "system"},
            "payload": {"id": call_id, "content": "ok", "is_error": false},
            "created_at": stamp,
        })
    }

    fn turn_ended(stamp: &str, reason: &str) -> Value {
        json!({
            "kind": "turn_ended",
            "author": {"kind": "system"},
            "payload": {"reason": reason, "touched": []},
            "created_at": stamp,
        })
    }

    /// A usage line the runtime stamped with the provider's own dollars.
    fn stamped(cost: f64) -> Value {
        json!({
            "input_tokens": 100, "output_tokens": 20, "cache_read_tokens": 40,
            "cache_write_tokens": 0, "estimated": false, "cost_usd": cost,
            "model": "z-ai/glm-5.3", "profile": "tensorx",
        })
    }

    /// No stamp, but a model the config can price: retro-priced, `~$`.
    fn retro() -> Value {
        json!({
            "input_tokens": 300, "output_tokens": 60, "cache_read_tokens": 0,
            "cache_write_tokens": 0, "estimated": false, "cost_usd": null,
            "model": "z-ai/glm-5.3", "profile": "tensorx",
        })
    }

    /// Neither a stamp nor a model any table knows: unpriced.
    fn unpriced() -> Value {
        json!({
            "input_tokens": 10, "output_tokens": 2, "cache_read_tokens": 0,
            "cache_write_tokens": 0, "estimated": false, "cost_usd": null,
            "model": null, "profile": null,
        })
    }

    /// Every tool call the fixture's lines carry.
    fn calls_in(lines: &[Value]) -> u32 {
        blocks_in(lines)
            .iter()
            .filter(|b| b["type"] == "tool_call")
            .count() as u32
    }

    fn blocks_in(lines: &[Value]) -> Vec<Value> {
        lines
            .iter()
            .filter(|l| l["kind"] == "assistant_message")
            .flat_map(|l| {
                l["payload"]["blocks"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect()
    }

    /// The fixture's own `update_tasks` list, counted the way #102 counts
    /// it: `done` of `total`, and the `active` step, else the first one
    /// that is not done.
    fn checklist_in(lines: &[Value]) -> (usize, usize, Option<String>) {
        let tasks: Vec<Value> = blocks_in(lines)
            .into_iter()
            .filter(|b| b["name"] == "update_tasks")
            .flat_map(|b| b["args"]["tasks"].as_array().cloned().unwrap_or_default())
            .collect();
        let done = tasks.iter().filter(|t| t["state"] == "done").count();
        let active = tasks
            .iter()
            .find(|t| t["state"] == "active")
            .or_else(|| tasks.iter().find(|t| t["state"] != "done"))
            .map(|t| t["text"].as_str().unwrap().to_owned());
        (done, tasks.len(), active)
    }

    /// What the config's own table says a fixture line costs: recomputed
    /// in the test from the same `[prices]` block, never hand-written.
    fn expected(line: &Value, profile: &str) -> f64 {
        let config = Config::parse(PRICED).unwrap();
        let (_, p) = config.select(Some(profile)).unwrap();
        let prices = p.prices.as_ref().unwrap().prices();
        let usage: Usage = serde_json::from_value(line["payload"]["usage"].clone()).unwrap();
        prices.cost_usd(&usage)
    }

    fn mtime_of(path: &Path) -> OffsetDateTime {
        OffsetDateTime::from(std::fs::metadata(path).unwrap().modified().unwrap())
    }

    // ---- T2: the fold -------------------------------------------------

    /// A live open turn: three calls, the last in flight, a stamped, a
    /// retro-priced and an unpriced usage line, a checklist with an
    /// active step.
    fn live_fixture() -> (tempfile::TempDir, Ulid, Vec<Value>) {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user(
                "2026-09-27T12:00:00Z",
                "the first line of the prompt\nthe rest",
            ),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "cargo fmt"}))],
                Some(stamped(0.5)),
            ),
            result("2026-09-27T12:00:02Z", "c1"),
            assistant(
                "2026-09-27T12:00:03Z",
                vec![call_block(
                    "update_tasks",
                    "c2",
                    json!({"tasks": [
                        {"text": "read the spec", "state": "done"},
                        {"text": "write the fold", "state": "done"},
                        {"text": "write commit 1", "state": "active"},
                        {"text": "gate it", "state": "pending"},
                        {"text": "report", "state": "pending"},
                    ]}),
                )],
                Some(retro()),
            ),
            result("2026-09-27T12:00:04Z", "c2"),
            assistant(
                "2026-09-27T12:00:05Z",
                vec![call_block(
                    "bash",
                    "c3",
                    json!({"command": "cargo test --no-fail-fast"}),
                )],
                Some(unpriced()),
            ),
        ];
        write(dir.path(), id, &lines);
        (dir, id, lines)
    }

    #[test]
    fn a_live_turn_shows_its_calls_cost_checklist_and_the_call_in_flight() {
        let (dir, id, lines) = live_fixture();
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(30);
        let p = progress(&events, now, false, &book()).expect("a live turn");

        assert_eq!(p.state, State::Live);
        assert_eq!(p.turn, 1);
        assert_eq!(p.title, "the first line of the prompt");
        assert_eq!(p.elapsed_secs, (now - events[0].created_at).whole_seconds());
        assert_eq!(p.idle_secs, 30);
        assert_eq!(p.calls, calls_in(&lines));
        assert_eq!(p.calls, 3);
        assert_eq!(p.spent, Some(0.5));
        assert_eq!(p.estimated_spent, Some(expected(&lines[3], "tensorx")));
        assert_eq!(p.unpriced, 1);

        let now_call = p.now.as_ref().expect("a call in flight");
        assert_eq!(now_call.tool, "bash");
        assert_eq!(now_call.args, "cargo test --no-fail-fast");
        assert_eq!(now_call.in_flight, 1);
        assert!(p.last.is_none());

        let (done, total, active) = checklist_in(&lines);
        let checklist = p.checklist.as_ref().expect("a checklist");
        assert_eq!((checklist.done, checklist.total), (done, total));
        assert_eq!(checklist.active, active);

        let line = line(&p);
        assert!(line.contains("live"), "{line}");
        assert!(line.contains("turn 1"), "{line}");
        assert!(line.contains("3 calls"), "{line}");
        assert!(line.contains("checklist 2/5: write commit 1"), "{line}");
        assert!(
            line.contains("now: bash cargo test --no-fail-fast"),
            "{line}"
        );
        // A stamped dollar and a retro-priced one stay apart, `stats`'
        // own four-decimal cell.
        assert!(line.contains("$0.5000"), "{line}");
        assert!(
            line.contains(&format!("~${:.4}", expected(&lines[3], "tensorx"))),
            "{line}"
        );
    }

    #[test]
    fn a_parked_turn_is_live_three_hours_later() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T09:00:00Z", "please choose"),
            assistant(
                "2026-09-27T09:00:01Z",
                vec![call_block(
                    ASK_HUMAN,
                    "c1",
                    json!({"question": "which one?"}),
                )],
                None,
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::hours(3);

        let p = progress(&events, now, false, &book()).expect("parked, so live");
        assert_eq!(p.state, State::WaitingForAnAnswer);
        assert_eq!(p.idle_secs, 3 * 3_600);
        assert_eq!(p.turn, 1);
        // The call is in flight, so it is `now`, not `last`.
        assert_eq!(p.now.as_ref().unwrap().tool, ASK_HUMAN);
        assert!(line(&p).contains("waiting for an answer"), "{}", line(&p));

        // The same log, once the question is answered: a plain live turn.
        let answered = [lines.clone(), vec![result("2026-09-27T09:00:02Z", "c1")]].concat();
        let dir2 = tempfile::tempdir().unwrap();
        let id2 = Ulid::generate();
        write(dir2.path(), id2, &answered);
        let events = read(dir2.path(), id2);
        let now = last_at(&events) + Duration::seconds(5);
        let p = progress(&events, now, false, &book()).expect("answered, still live");
        assert_eq!(p.state, State::Live);
        assert_eq!(p.last.as_ref().unwrap().tool, ASK_HUMAN);
        assert!(p.now.is_none());
    }

    #[test]
    fn a_parked_turn_on_a_permission_or_a_checkpoint_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T09:00:00Z", "go on"),
            json!({
                "kind": "permission_requested",
                "author": {"kind": "system"},
                "payload": {
                    "call": {"id": "c9", "name": "bash", "args": {"command": "rm -rf x"}},
                    "class": "exec",
                    "reason": "runs a command",
                },
                "created_at": "2026-09-27T09:00:01Z",
            }),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::hours(2);
        let p = progress(&events, now, false, &book()).expect("parked, so live");
        assert_eq!(p.state, State::WaitingForApproval);
        assert_eq!(p.calls, 0, "a permission request is not a call of its own");
        assert!(line(&p).contains("waiting for approval"), "{}", line(&p));

        // A decision clears it: the turn is open, unparked, and old, so
        // stale rather than waiting.
        let dir2 = tempfile::tempdir().unwrap();
        let id2 = Ulid::generate();
        let decided = [
            lines.clone(),
            vec![json!({
                "kind": "permission_decided",
                "author": {"kind": "user", "id": "steve"},
                "payload": {"call_id": "c9", "allow": true, "scope": "once"},
                "created_at": "2026-09-27T09:05:00Z",
            })],
        ]
        .concat();
        write(dir2.path(), id2, &decided);
        let events = read(dir2.path(), id2);
        let now = last_at(&events) + Duration::hours(2);
        assert!(progress(&events, now, false, &book()).is_none());
        assert_eq!(
            progress(&events, now, true, &book()).unwrap().state,
            State::Stale
        );

        // A checkpoint ask nobody answered, and one that was answered.
        let dir3 = tempfile::tempdir().unwrap();
        let id3 = Ulid::generate();
        let asked = [
            user("2026-09-27T09:00:00Z", "go on"),
            json!({
                "kind": "checkpoint_asked",
                "author": {"kind": "system"},
                "payload": {"gate": "plan_gate", "shown": [], "options": ["go", "stop"]},
                "created_at": "2026-09-27T09:00:01Z",
            }),
        ];
        write(dir3.path(), id3, &asked);
        let events = read(dir3.path(), id3);
        let now = last_at(&events) + Duration::hours(2);
        let p = progress(&events, now, false, &book()).expect("parked, so live");
        assert_eq!(p.state, State::WaitingAtACheckpoint);
        assert!(line(&p).contains("waiting at a checkpoint"), "{}", line(&p));

        let dir4 = tempfile::tempdir().unwrap();
        let id4 = Ulid::generate();
        let answered: Vec<Value> = asked
            .iter()
            .cloned()
            .chain(vec![json!({
                "kind": "checkpoint_answered",
                "author": {"kind": "user", "id": "steve"},
                "payload": {"answer": "go"},
                "created_at": "2026-09-27T09:10:00Z",
            })])
            .collect();
        write(dir4.path(), id4, &answered);
        let events = read(dir4.path(), id4);
        let now = last_at(&events) + Duration::seconds(10);
        let p = progress(&events, now, false, &book()).expect("answered, still live");
        assert_eq!(p.state, State::Live);
    }

    /// A thread whose only turn ended.
    fn closed_fixture(stamp: &str, reason: &str) -> (tempfile::TempDir, Ulid, Vec<Value>) {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user(stamp, "one short job"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "ls"}))],
                Some(stamped(0.25)),
            ),
            result("2026-09-27T12:00:02Z", "c1"),
            turn_ended("2026-09-27T12:00:03Z", reason),
        ];
        write(dir.path(), id, &lines);
        (dir, id, lines)
    }

    #[test]
    fn a_closed_thread_is_none_by_default_and_ended_under_all() {
        let (dir, id, lines) = closed_fixture("2026-09-27T12:00:00Z", "done");
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::minutes(5);

        assert!(
            progress(&events, now, false, &book()).is_none(),
            "an ended turn is not live"
        );
        let p = progress(&events, now, true, &book()).expect("listed under --all");
        assert_eq!(p.state, State::Ended);
        assert_eq!(p.ended_reason.as_deref(), Some("done"));
        assert_eq!(p.turn, 1);
        assert_eq!(p.calls, calls_in(&lines));
        assert_eq!(p.spent, Some(0.25));
        assert_eq!(p.elapsed_secs, 3, "the turn's own duration");
        assert_eq!(p.idle_secs, 300);
        assert!(line(&p).contains("ended (done)"), "{}", line(&p));

        // A reason the stats tables group by keeps only its head.
        let (dir2, id2, _) = closed_fixture("2026-09-27T12:00:00Z", "provider_error: http 503");
        let events = read(dir2.path(), id2);
        let now = last_at(&events) + Duration::minutes(5);
        let p = progress(&events, now, true, &book()).unwrap();
        assert_eq!(p.ended_reason.as_deref(), Some("provider_error"));
    }

    #[test]
    fn an_ended_turn_older_than_an_hour_is_not_even_under_all() {
        let (dir, id, _) = closed_fixture("2026-09-27T12:00:00Z", "done");
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::minutes(61);
        assert!(progress(&events, now, true, &book()).is_none());
    }

    #[test]
    fn an_open_turn_two_hours_old_is_stale() {
        let (dir, id, _) = live_fixture();
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::hours(2);

        assert!(progress(&events, now, false, &book()).is_none());
        let p = progress(&events, now, true, &book()).expect("stale, under --all");
        assert_eq!(p.state, State::Stale);
        assert_eq!(p.idle_secs, 2 * 3_600);
        assert!(line(&p).contains("stale"), "{}", line(&p));
        assert!(line(&p).contains("last event 2h00m ago"), "{}", line(&p));
    }

    #[test]
    fn a_turn_a_resume_interrupted_has_ended() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "a job that got killed"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "sleep 900"}))],
                Some(stamped(0.1)),
            ),
            json!({
                "kind": "interrupted",
                "author": {"kind": "system"},
                "payload": {"reason": "killed"},
                "created_at": "2026-09-27T12:05:00Z",
            }),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::minutes(5);
        assert!(progress(&events, now, false, &book()).is_none());
        let p = progress(&events, now, true, &book()).expect("listed under --all");
        assert_eq!(p.state, State::Ended);
        assert_eq!(p.ended_reason.as_deref(), Some("interrupted"));
    }

    #[test]
    fn the_open_turn_counts_turn_starts() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T09:00:00Z", "turn one"),
            assistant(
                "2026-09-27T09:00:01Z",
                vec![call_block("bash", "a", json!({"command": "ls"}))],
                None,
            ),
            turn_ended("2026-09-27T09:00:02Z", "done"),
            user("2026-09-27T10:00:00Z", "turn two"),
            turn_ended("2026-09-27T10:00:01Z", "done"),
            user("2026-09-27T11:00:00Z", "turn three"),
            assistant(
                "2026-09-27T11:00:01Z",
                vec![call_block("bash", "b", json!({"command": "ls"}))],
                None,
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(30);

        let p = progress(&events, now, false, &book()).expect("the third turn is open");
        assert_eq!(p.turn, 3);
        assert_eq!(
            p.title, "turn one",
            "the title is the thread's first prompt"
        );
        // The elapsed time is the open turn's, not the thread's.
        assert_eq!(p.elapsed_secs, 31);
        assert_eq!(p.calls, 1);
    }

    #[test]
    fn a_mid_turn_message_is_not_a_turn_start() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "start"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "ls"}))],
                None,
            ),
            mid_turn("2026-09-27T12:00:02Z", "actually, steer left"),
            assistant(
                "2026-09-27T12:00:03Z",
                vec![call_block("bash", "c2", json!({"command": "ls -l"}))],
                None,
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(10);

        let p = progress(&events, now, false, &book()).expect("still one turn");
        assert_eq!(p.turn, 1, "a queued message does not open a turn");
        assert_eq!(p.calls, 2);
        assert_eq!(
            p.elapsed_secs,
            (now - at("2026-09-27T12:00:00Z")).whole_seconds()
        );
    }

    #[test]
    fn a_step_thread_is_titled_by_the_runners_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            runner(
                "2026-09-27T12:00:00Z",
                "build 29: close the ticket\nrun the gate first",
            ),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "cargo fmt"}))],
                Some(stamped(0.4)),
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(20);
        let p = progress(&events, now, false, &book()).expect("a step thread runs");
        assert_eq!(p.title, "build 29: close the ticket");
        assert_eq!(p.turn, 1);
        assert_eq!(p.spent, Some(0.4));

        // A rename wins over the prompt, and a long title is cut.
        let renamed = [
            lines.clone(),
            vec![json!({
                "kind": "thread_renamed",
                "author": {"kind": "agent", "id": "title"},
                "payload": {"title": "a".repeat(50), "model": null, "usage": null},
                "created_at": "2026-09-27T12:00:02Z",
            })],
        ]
        .concat();
        let dir2 = tempfile::tempdir().unwrap();
        let id2 = Ulid::generate();
        write(dir2.path(), id2, &renamed);
        let events = read(dir2.path(), id2);
        let now = last_at(&events) + Duration::seconds(10);
        let p = progress(&events, now, false, &book()).unwrap();
        assert_eq!(
            p.title.chars().count(),
            TITLE_CHARS + 1,
            "cut with an ellipsis"
        );
        assert!(p.title.ends_with('…'));
    }

    #[test]
    fn several_calls_in_one_reply_show_the_first_and_the_rest_counted() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "two at once"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![
                    call_block("bash", "c1", json!({"command": "cargo fmt"})),
                    call_block("bash", "c2", json!({"command": "cargo clippy"})),
                    call_block("bash", "c3", json!({"command": "cargo test"})),
                ],
                None,
            ),
            result("2026-09-27T12:00:02Z", "c1"),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(5);
        let p = progress(&events, now, false, &book()).unwrap();
        assert_eq!(p.calls, 3);
        let now_call = p.now.as_ref().unwrap();
        assert_eq!(now_call.in_flight, 2, "the first in flight, plus one");
        assert!(
            line(&p).contains("now: bash cargo clippy +1"),
            "{}",
            line(&p)
        );
    }

    #[test]
    fn a_call_of_a_tool_other_than_bash_shows_its_arguments_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "read a file"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block(
                    "read_file",
                    "c1",
                    json!({"path": "crates/tui/src/progress.rs"}),
                )],
                None,
            ),
            result("2026-09-27T12:00:02Z", "c1"),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(5);
        let p = progress(&events, now, false, &book()).unwrap();
        let last = p.last.as_ref().expect("the latest completed call");
        assert_eq!(last.tool, "read_file");
        assert_eq!(last.args, r#"{"path":"crates/tui/src/progress.rs"}"#);
        assert!(p.now.is_none());
    }

    #[test]
    fn a_long_argument_is_cut_to_sixty_characters_and_a_long_command_to_its_first_line() {
        let command = format!("printf '%s' {}\nsecond line", "x".repeat(100));
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "one long command"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": command}))],
                None,
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(5);
        let p = progress(&events, now, false, &book()).unwrap();
        let shown = &p.now.as_ref().unwrap().args;
        assert!(!shown.contains('\n'), "the first line only: {shown}");
        assert_eq!(
            shown.chars().count(),
            ARGS_CHARS + 1,
            "cut with an ellipsis"
        );
        assert_eq!(
            shown.as_str(),
            format!("{}…", &command.lines().next().unwrap()[..ARGS_CHARS])
        );
    }

    #[test]
    fn the_side_jobs_are_priced_the_way_the_thread_row_prices_them() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let extraction = json!({
            "input_tokens": 500, "output_tokens": 50, "cache_read_tokens": 0,
            "cache_write_tokens": 0, "estimated": false, "cost_usd": null,
            "model": "deepseek/deepseek-v4.1-flash", "profile": null,
        });
        // `expected` prices the fixture's own usage, so the two sides of
        // the assertion come from one `[prices]` table.
        let lines = vec![
            user("2026-09-27T12:00:00Z", "a turn with a side job"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "ls"}))],
                Some(stamped(0.5)),
            ),
            json!({
                "kind": "memory_extracted",
                "author": {"kind": "agent", "id": "memory"},
                "payload": {"through_seq": 1, "written": [], "model": "deepseek/deepseek-v4.1-flash", "usage": extraction},
                "created_at": "2026-09-27T12:00:02Z",
            }),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(5);
        let p = progress(&events, now, false, &book()).unwrap();
        assert_eq!(p.spent, Some(0.5));
        assert_eq!(p.estimated_spent, Some(expected(&lines[2], "flash")));
    }

    // ---- T3: which threads, and in what order --------------------------

    /// A live thread, a parked thread and a thread that went quiet.
    fn mixed_fixture() -> (tempfile::TempDir, Ulid, Ulid, Ulid) {
        let dir = tempfile::tempdir().unwrap();
        let live = Ulid::generate();
        let parked = Ulid::generate();
        let stale = Ulid::generate();
        write(
            dir.path(),
            live,
            &[
                user("2026-09-27T10:00:00Z", "the live one"),
                assistant(
                    "2026-09-27T12:00:00Z",
                    vec![call_block("bash", "c1", json!({"command": "ls"}))],
                    None,
                ),
            ],
        );
        write(
            dir.path(),
            parked,
            &[
                user("2026-09-27T10:00:00Z", "the parked one"),
                assistant(
                    "2026-09-27T11:00:00Z",
                    vec![call_block(
                        ASK_HUMAN,
                        "c1",
                        json!({"question": "which one?"}),
                    )],
                    None,
                ),
            ],
        );
        write(
            dir.path(),
            stale,
            &[
                user("2026-09-27T08:00:00Z", "the one that died"),
                assistant(
                    "2026-09-27T08:00:01Z",
                    vec![call_block("bash", "c1", json!({"command": "ls"}))],
                    None,
                ),
            ],
        );
        (dir, live, parked, stale)
    }

    #[test]
    fn the_default_listing_holds_the_live_threads_newest_activity_first() {
        let (dir, live, parked, stale) = mixed_fixture();
        // The live thread's last event is the newest, so it leads; the
        // parked one's is older, but parked is live at any age.
        let events = read(dir.path(), live);
        let now = last_at(&events) + Duration::seconds(30);
        let status = collect(dir.path(), &[], None, false, now, &book()).unwrap();

        let ids: Vec<&str> = status.threads.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec![live.to_string(), parked.to_string()]);
        assert_eq!(status.threads[0].state, State::Live);
        assert_eq!(status.threads[1].state, State::WaitingForAnAnswer);
        assert_eq!(status.stale, 1, "the quiet one is counted, not listed");
        assert!(
            !ids.contains(&stale.to_string().as_str()),
            "a stale thread is not in the default listing"
        );
    }

    #[test]
    fn all_adds_the_stale_and_the_recently_ended() {
        let (dir, live, parked, stale) = mixed_fixture();
        let ended = Ulid::generate();
        write(
            dir.path(),
            ended,
            &[
                user("2026-09-27T11:30:00Z", "the one that finished"),
                assistant(
                    "2026-09-27T11:30:01Z",
                    vec![call_block("bash", "c1", json!({"command": "ls"}))],
                    None,
                ),
                turn_ended("2026-09-27T11:30:02Z", "done"),
            ],
        );
        let events = read(dir.path(), live);
        let now = last_at(&events) + Duration::seconds(30);
        let status = collect(dir.path(), &[], None, true, now, &book()).unwrap();

        let mut ids: Vec<String> = status.threads.iter().map(|p| p.id.clone()).collect();
        ids.sort();
        let mut want = vec![
            live.to_string(),
            parked.to_string(),
            stale.to_string(),
            ended.to_string(),
        ];
        want.sort();
        assert_eq!(ids, want);
        let states: Vec<State> = status.threads.iter().map(|p| p.state).collect();
        assert!(states.contains(&State::Stale));
        assert!(states.contains(&State::Ended));
        assert_eq!(status.stale, 1);
    }

    #[test]
    fn nothing_running_names_the_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        write(
            dir.path(),
            id,
            &[
                user("2026-09-27T08:00:00Z", "the only thread, and it died"),
                assistant(
                    "2026-09-27T08:00:01Z",
                    vec![call_block("bash", "c1", json!({"command": "ls"}))],
                    None,
                ),
            ],
        );
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::hours(4);

        let status = collect(dir.path(), &[], None, false, now, &book()).unwrap();
        assert!(status.threads.is_empty());
        assert_eq!(status.stale, 1);
        assert_eq!(
            render(&status),
            "nothing running · 1 stale (status --all)\n"
        );

        // And with nothing open at all, the plain line.
        let empty = collect(dir.path(), &[], None, false, now, &book()).unwrap();
        assert!(empty.threads.is_empty());
        assert_eq!(render(&Status::default()), "nothing running\n");
    }

    #[test]
    fn json_carries_the_full_object_for_a_thread_with_no_open_turn() {
        let (dir, id, _) = closed_fixture("2026-09-27T12:00:00Z", "done");
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::minutes(5);
        let status = collect(dir.path(), &[], None, true, now, &book()).unwrap();
        let progress = &status.threads[0];
        let value = serde_json::to_value(progress).unwrap();

        assert_eq!(value["id"], id.to_string());
        assert_eq!(value["state"], "ended");
        assert_eq!(value["ended_reason"], "done");
        assert_eq!(value["turn"], 1);
        assert_eq!(value["calls"], 1);
        assert_eq!(value["spent"], 0.25);
        assert_eq!(value["idle_secs"], 300);
        assert_eq!(value["elapsed_secs"], 3);
        assert_eq!(value["unpriced"], 0);
        // Every optional field is there exactly when it has a value.
        assert_eq!(
            value.get("estimated_spent").is_none(),
            progress.estimated_spent.is_none()
        );
        assert_eq!(
            value.get("checklist").is_none(),
            progress.checklist.is_none()
        );
        assert_eq!(value.get("now").is_none(), progress.now.is_none());
        assert_eq!(value.get("last").is_none(), progress.last.is_none());
        // The sort key stays out of the shape.
        assert!(value.get("last_at").is_none());
    }

    /// T5: `--json` is the documented shape, field for field.
    #[test]
    fn json_is_an_array_of_the_documented_fields() {
        let (dir, id, _) = live_fixture();
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(30);
        let status = collect(dir.path(), &[], None, false, now, &book()).unwrap();

        let parsed: Value = serde_json::from_str(&as_json(&status).unwrap()).unwrap();
        let threads = parsed.as_array().expect("an array");
        assert_eq!(threads.len(), 1);
        let mut keys: Vec<&str> = threads[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut want = vec![
            "id",
            "title",
            "state",
            "turn",
            "elapsed_secs",
            "idle_secs",
            "calls",
            "spent",
            "estimated_spent",
            "unpriced",
            "checklist",
            "now",
            "last",
        ];
        // The two call fields are a pair: `last` is absent while a call
        // is in flight, which the fixture itself says.
        let answered: Vec<String> = events
            .iter()
            .filter(|e| e.kind == EventKind::ToolResult)
            .filter_map(|e| {
                serde_json::from_value::<aigentic_runtime::aigentic_log::ToolResultPayload>(
                    e.payload.clone(),
                )
                .ok()
            })
            .map(|p| p.result.id)
            .collect();
        let in_flight = events.iter().any(|e| {
            e.kind == EventKind::AssistantMessage
                && serde_json::from_value::<
                    aigentic_runtime::aigentic_log::AssistantMessagePayload,
                >(e.payload.clone())
                .is_ok_and(|p| {
                    p.blocks.iter().any(|b| match b {
                        ContentBlock::ToolCall(c) => !answered.contains(&c.id),
                        _ => false,
                    })
                })
        });
        assert!(in_flight, "the fixture leaves a call unanswered");
        want.retain(|k| *k != "last");
        want.sort_unstable();
        assert_eq!(keys, want);
        assert_eq!(threads[0]["id"], id.to_string());
        assert_eq!(threads[0]["state"], "live");
    }

    // ---- T4: the read filter ------------------------------------------

    /// A log that cannot be read: one line that is not JSON at all.
    fn corrupt(dir: &Path) -> std::path::PathBuf {
        let path = dir.join(format!("{}.jsonl", Ulid::generate()));
        std::fs::write(&path, "not a json line at all\n").unwrap();
        path
    }

    #[test]
    fn a_log_older_than_the_window_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = corrupt(dir.path());
        let now = mtime_of(&path) + Duration::hours(25);
        let status = collect(dir.path(), &[], None, true, now, &book()).unwrap();
        assert_eq!(status.unreadable, 0, "a log outside the window is skipped");
        assert!(status.threads.is_empty());
    }

    #[test]
    fn an_unreadable_log_inside_the_window_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = corrupt(dir.path());
        let now = mtime_of(&path) + Duration::minutes(1);
        let status = collect(dir.path(), &[], None, true, now, &book()).unwrap();
        assert_eq!(status.unreadable, 1, "counted, never swallowed");
        assert!(status.threads.is_empty());
    }

    #[test]
    fn a_readable_log_older_than_the_window_is_skipped_too() {
        let (dir, id, _) = live_fixture();
        let now = mtime_of(&dir.path().join(format!("{id}.jsonl"))) + Duration::hours(25);
        let status = collect(dir.path(), &[], None, true, now, &book()).unwrap();
        assert!(
            status.threads.is_empty(),
            "the filter is by mtime, not by state"
        );
        assert_eq!(status.unreadable, 0);
    }

    // ---- The project filter, and the CLI's own tests -------------------

    #[test]
    fn a_thread_of_another_project_is_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            json!({
                "kind": "thread_started",
                "author": {"kind": "system"},
                "payload": {"project": "alpha", "root": "/tmp/alpha",
                            "created_by": {"kind": "user", "id": "steve"}},
                "created_at": "2026-09-27T12:00:00Z",
            }),
            user("2026-09-27T12:00:00Z", "in alpha"),
            assistant(
                "2026-09-27T12:00:01Z",
                vec![call_block("bash", "c1", json!({"command": "ls"}))],
                None,
            ),
        ];
        write(dir.path(), id, &lines);
        let events = read(dir.path(), id);
        let now = last_at(&events) + Duration::seconds(30);
        let workspaces = Vec::new();
        assert_eq!(
            collect(dir.path(), &workspaces, Some("alpha"), false, now, &book())
                .unwrap()
                .threads
                .len(),
            1
        );
        assert!(
            collect(dir.path(), &workspaces, Some("beta"), false, now, &book())
                .unwrap()
                .threads
                .is_empty()
        );
    }
}
