//! The `recall` harness tool (issue #75): bringing back what the harness
//! forgot. Three forms, one of which every call must be — a dropped
//! result by its handle, a range of the log, or a search over the whole
//! thread.
//!
//! The output is text assembled here and capped like any tool output. It
//! reads the log's events, never the projection, so a range across a
//! `compacted` event shows the originals and the summary that replaced
//! them. Nothing is appended but the call's own tool result.

use aigentic_core::{Event, EventKind};
use aigentic_log::{
    ToolResultPayload, call_of, kind_name, project_at, render_range, search, short_args,
};
use aigentic_tools::{DEFAULT_OUTPUT_CAP, truncate_output};
use serde::Deserialize;

/// The number of events a range shows, and the most hits a query may ask
/// for. Both are the harness's own limits, not the model's.
pub const RANGE_MAX_EVENTS: usize = 50;
/// The hits a query returns when it does not say how many.
pub const QUERY_DEFAULT_LIMIT: u64 = 10;
/// The most hits a query may ask for.
pub const QUERY_MAX_LIMIT: u64 = 50;

/// A `recall` call's arguments: one struct of options, because the three
/// forms share the wire and the check that exactly one is present is
/// named below. An unknown key is refused like every harness call.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecallArgs {
    #[serde(default)]
    pub handle: Option<u64>,
    #[serde(default)]
    pub from_line: Option<u64>,
    #[serde(default)]
    pub from: Option<u64>,
    #[serde(default)]
    pub to: Option<u64>,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// Which of the three forms a call asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    /// One result, from `from_line` (1-based) on.
    Handle { handle: u64, from_line: Option<u64> },
    /// The log's events from `from` to `to`, inclusive.
    Range { from: u64, to: u64 },
    /// A search over the thread.
    Query { query: String, limit: usize },
}

/// The error every other combination gets. One place, so the wording the
/// model is taught is the wording it is told.
pub const FORM_ERROR: &str = "recall takes exactly one of: handle (optionally with from_line), from and to together, or query (optionally with limit)";

/// Exactly one form, or [`FORM_ERROR`]. A bare `from` or `to`, a
/// `from_line` without a handle, a `limit` without a query, and any two
/// forms together are all the same error.
pub fn form(args: &RecallArgs) -> Result<Form, String> {
    match args {
        RecallArgs {
            handle: Some(handle),
            from_line,
            from: None,
            to: None,
            query: None,
            limit: None,
        } => Ok(Form::Handle {
            handle: *handle,
            from_line: *from_line,
        }),
        RecallArgs {
            handle: None,
            from_line: None,
            from: Some(from),
            to: Some(to),
            query: None,
            limit: None,
        } => Ok(Form::Range {
            from: *from,
            to: *to,
        }),
        RecallArgs {
            handle: None,
            from_line: None,
            from: None,
            to: None,
            query: Some(query),
            limit,
        } => Ok(Form::Query {
            query: query.clone(),
            limit: limit.unwrap_or(QUERY_DEFAULT_LIMIT).min(QUERY_MAX_LIMIT) as usize,
        }),
        _ => Err(FORM_ERROR.to_owned()),
    }
}

/// The text a `recall` call returns. Ranges are `render_range`'s own
/// text; the handle form fits whole lines and pages with `from_line`
/// rather than letting a cut fall mid-line. Every form is then capped
/// like any other tool output.
pub fn output(events: &[Event], form: &Form) -> Result<String, String> {
    let current = project_at(events, u64::MAX);
    let text = match form {
        Form::Handle { handle, from_line } => handle_output(events, *handle, *from_line, &current)?,
        Form::Range { from, to } => range_output(events, *from, *to, &current),
        Form::Query { query, limit } => query_output(events, query, *limit, &current),
    };
    Ok(truncate_output(&text, DEFAULT_OUTPUT_CAP))
}

/// One result, in full, from `from_line` on; cut by the caller's cap with
/// a paging line when it does not all fit.
fn handle_output(
    events: &[Event],
    handle: u64,
    from_line: Option<u64>,
    current: &Option<String>,
) -> Result<String, String> {
    let Some(event) = events.iter().find(|e| e.seq == handle) else {
        return Err(format!("no event at seq {handle} in this thread"));
    };
    if event.kind != EventKind::ToolResult {
        return Err(format!(
            "{handle} is a {}, not a tool result; recall it with from and to",
            kind_name(event.kind)
        ));
    }
    let p: ToolResultPayload = serde_json::from_value(event.payload.clone())
        .map_err(|e| format!("the result at {handle} does not parse: {e}"))?;
    let head = match call_of(events, &p.result.id) {
        Some((name, args)) => format!("{name} {}", short_args(&args)),
        None => "call not found".to_owned(),
    };
    let header = format!("[recalled result {handle} · {head}]");
    let lines: Vec<&str> = p.result.content.lines().collect();
    let total = lines.len();
    let first = from_line.unwrap_or(1).max(1) as usize;
    let start = (first - 1).min(total);
    let pages = total - start;

    // Fill the cap with whole lines, leaving room for the paging line and
    // the read-only marker that may follow the tail.
    let marker = read_only_marker(events, handle, current);
    let reserve = marker.as_ref().map_or(0, |m| m.len() + 1);
    let mut shown = pages;
    let text = loop {
        let paging = if shown < pages {
            let paging = paging_line(start + 1, start + shown, total, handle);
            Some(paging)
        } else {
            None
        };
        let mut parts = vec![header.clone()];
        parts.extend(lines[start..start + shown].iter().map(|l| (*l).to_owned()));
        if let Some(paging) = paging {
            parts.push(paging);
        }
        let text = parts.join("\n");
        if shown == 0 || text.len() + reserve <= DEFAULT_OUTPUT_CAP {
            break text;
        }
        shown -= 1;
    };
    Ok(match marker {
        Some(marker) => format!("{text}\n{marker}"),
        None => text,
    })
}

/// `(lines {a}-{b} of {N} shown; recall {seq} with from_line {b+1} for more)`
fn paging_line(a: usize, b: usize, total: usize, seq: u64) -> String {
    format!(
        "(lines {a}-{b} of {total} shown; recall {seq} with from_line {} for more)",
        b + 1
    )
}

/// The range's blocks, each marked when its own event belongs to another
/// project. Without a marker to add this is `render_range`'s own text.
fn range_output(events: &[Event], from: u64, to: u64, current: &Option<String>) -> String {
    let range: Vec<&Event> = events
        .iter()
        .filter(|e| from <= e.seq && e.seq <= to)
        .collect();
    let shown = range.len().min(RANGE_MAX_EVENTS);
    let marked: Vec<Option<String>> = range[..shown]
        .iter()
        .map(|e| read_only_marker(events, e.seq, current))
        .collect();
    if marked.iter().all(Option::is_none) {
        return render_range(events, from, to, RANGE_MAX_EVENTS);
    }
    let mut blocks: Vec<String> = range[..shown]
        .iter()
        .zip(&marked)
        .map(|(e, marker)| {
            let block = render_range(events, e.seq, e.seq, 1);
            match marker {
                // The block's first line is its header.
                Some(marker) => match block.split_once('\n') {
                    Some((head, rest)) => format!("{head} {marker}\n{rest}"),
                    None => format!("{block} {marker}"),
                },
                None => block,
            }
        })
        .collect();
    if range.len() > shown {
        blocks.push(format!(
            "({} more events in this range not shown)",
            range.len() - shown
        ));
    }
    blocks.join("\n\n")
}

/// One hit per line, `{seq} · {kind} · {snippet}`, each marked when it
/// belongs to another project.
fn query_output(events: &[Event], query: &str, limit: usize, current: &Option<String>) -> String {
    let hits = search(events, query, limit);
    if hits.is_empty() {
        return format!("no matches for \"{query}\"");
    }
    hits.iter()
        .map(|hit| {
            let line = format!("{} · {} · {}", hit.seq, kind_name(hit.kind), hit.snippet);
            match read_only_marker(events, hit.seq, current) {
                Some(marker) => format!("{line} {marker}"),
                None => line,
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The read-only marker for a seq another project owns, or `None` when
/// the seq is the thread's current project. A seq outside any project
/// says so rather than naming one.
fn read_only_marker(events: &[Event], seq: u64, current: &Option<String>) -> Option<String> {
    let project = project_at(events, seq);
    if &project == current {
        return None;
    }
    Some(match project {
        Some(p) => format!(
            "[from project {p}: read-only here; {p}'s instructions and files no longer apply]"
        ),
        None => "[from outside any project: read-only here]".to_owned(),
    })
}
