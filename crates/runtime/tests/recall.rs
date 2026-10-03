//! Issue #75, the `recall` harness tool: T8, T9 and T10.
//!
//! T8 drives scripted turns (the `ask_human.rs`/`evict.rs` style) whose
//! model replies are `recall` calls, then reads the results back out of
//! the log: by handle, by range, by query, and every error. T9 does the
//! same over a thread that has changed project. T10 reads the advertised
//! spec.
//!
//! The fixture logs are written by hand, before the runtime opens them, so
//! every seq and every line count is the test's own.

use aigentic_core::{
    AgentId, Author, ContentBlock, Event, EventKind, ProviderEvent, RiskClass, ToolCall, UserId,
};
use aigentic_log::{
    ContextEvictedPayload, NewEvent, PolicyRecord, ProjectSwitchedPayload, ThreadLog,
    ThreadStartedPayload, ToolResultPayload, kind_name, project, render_range, short_args,
};
use aigentic_runtime::harness_tools::{
    HARNESS_CLASS, RECALL, RECALL_DESCRIPTION, harness_names, harness_specs, is_harness_tool,
};
use aigentic_runtime::recall_tool::FORM_ERROR;
use aigentic_runtime::{Answer, Approver, Runtime};
use aigentic_tools::{DEFAULT_OUTPUT_CAP, ToolRegistry, truncate_output};
use serde_json::{Value, json};
use ulid::Ulid;

mod common;

/// Allows every prompt: the tools here are harness ones, whose class is
/// safe, so nothing asks.
struct Yes;

impl Approver for Yes {
    fn author(&self) -> Author {
        Author::User(UserId("steve".into()))
    }
    fn ask(&mut self, _: &aigentic_log::PermissionRequestedPayload) -> Answer {
        Answer::Allow
    }
    fn ask_human(&mut self, _: &str) -> Option<String> {
        None
    }
}

// --------------------------------------------------------------- fixtures

/// The lines of the long result: numbered, so a test can name any of them.
const LONG_LINES: usize = 500;

fn long_content() -> String {
    (1..=LONG_LINES)
        .map(|i| format!("line {i:04} {}", "x".repeat(78)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn event(kind: EventKind, author: Author, payload: Value) -> NewEvent {
    NewEvent {
        kind,
        author,
        payload,
        parent_event: None,
    }
}

fn user(text: &str) -> NewEvent {
    event(
        EventKind::UserMessage,
        Author::User(UserId("steve".into())),
        json!({"blocks": [{"type": "text", "text": text}]}),
    )
}

fn call(call_id: &str, name: &str, args: Value) -> NewEvent {
    event(
        EventKind::AssistantMessage,
        Author::Agent(AgentId("worker".into())),
        json!({"blocks": [{"type": "tool_call", "id": call_id, "name": name, "args": args}]}),
    )
}

fn result(call_id: &str, content: &str) -> NewEvent {
    let payload = ToolResultPayload {
        result: aigentic_core::ToolResult {
            id: call_id.into(),
            content: content.into(),
            is_error: false,
        },
        policy: Some(PolicyRecord::rule("test", "allow")),
    };
    event(
        EventKind::ToolResult,
        Author::Agent(AgentId("worker".into())),
        serde_json::to_value(payload).unwrap(),
    )
}

fn started(project: Option<&str>) -> NewEvent {
    let payload = ThreadStartedPayload {
        project: project.map(str::to_owned),
        root: std::path::PathBuf::from("/tmp/fixture"),
        created_by: Author::User(UserId("steve".into())),
        parent_thread: None,
        step: None,
    };
    event(
        EventKind::ThreadStarted,
        Author::System,
        serde_json::to_value(payload).unwrap(),
    )
}

fn switched(from: Option<&str>, to: Option<&str>) -> NewEvent {
    let payload = ProjectSwitchedPayload {
        from: from.map(str::to_owned),
        to: to.map(str::to_owned),
        root: std::path::PathBuf::from("/tmp/fixture"),
        workspace: None,
    };
    event(
        EventKind::ProjectSwitched,
        Author::System,
        serde_json::to_value(payload).unwrap(),
    )
}

fn evicted(through_seq: u64) -> NewEvent {
    let payload = ContextEvictedPayload {
        through_seq,
        ratio: None,
    };
    event(
        EventKind::ContextEvicted,
        Author::System,
        serde_json::to_value(payload).unwrap(),
    )
}

/// The thread every fixture logs into, so a later test can reopen the same
/// file and continue the same thread.
fn thread_id() -> Ulid {
    Ulid::from(7_u128)
}

/// Writes `events` into a fresh log and returns it, with the tempdir that
/// holds it and the log's own view of what was written. The tempdir is
/// returned, not dropped: the log writes into it.
fn fixture(events: Vec<NewEvent>) -> (tempfile::TempDir, ThreadLog, Vec<Event>) {
    let dir = tempfile::tempdir().unwrap();
    let mut log = ThreadLog::open(dir.path(), thread_id()).unwrap();
    for e in events {
        log.append(e).unwrap();
    }
    let written = log.read_all().unwrap();
    (dir, log, written)
}

// --------------------------------------------------------------- the turn

/// A scripted reply that makes `calls` and then stops, followed by the
/// turn's own stop.
fn reply(calls: &[(&str, Value)]) -> Vec<Vec<ProviderEvent>> {
    vec![
        calls
            .iter()
            .map(|(id, args)| {
                ProviderEvent::ToolCall(ToolCall {
                    id: (*id).into(),
                    name: RECALL.into(),
                    args: args.clone(),
                })
            })
            .chain(std::iter::once(common::done("tool_calls")))
            .collect::<Vec<_>>(),
        vec![
            ProviderEvent::TextDelta("done".into()),
            common::done("stop"),
        ],
    ]
}

/// The harness the tests drive: a scripted provider, no tools of its own,
/// `log` as its thread.
fn drive(script: Vec<Vec<ProviderEvent>>, log: ThreadLog) -> Runtime {
    let registry: ToolRegistry = Vec::new().into();
    Runtime::new(
        common::scripted(script).0,
        registry,
        log,
        AgentId("worker".into()),
    )
    .with_approver(Box::new(Yes))
}

/// Runs one scripted turn to `done`.
async fn turn(runtime: &mut Runtime, text: &str) {
    let outcome = runtime
        .run_turn(
            Author::User(UserId("steve".into())),
            vec![ContentBlock::Text(text.into())],
            &mut |_| {},
        )
        .await
        .expect("the turn runs");
    assert_eq!(outcome.reason, "done", "{} iterations", outcome.iterations);
}

/// Every `recall` result the log holds, in call order: the call's
/// arguments, the result's text, and whether it was an error.
fn recalls(events: &[Event]) -> Vec<(Value, String, bool)> {
    let mut args: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    for e in events
        .iter()
        .filter(|e| e.kind == EventKind::AssistantMessage)
    {
        let p: aigentic_log::AssistantMessagePayload =
            serde_json::from_value(e.payload.clone()).expect("an assistant message");
        for b in p.blocks {
            if let ContentBlock::ToolCall(c) = b
                && c.name == RECALL
            {
                args.insert(c.id, c.args);
            }
        }
    }
    events
        .iter()
        .filter(|e| e.kind == EventKind::ToolResult)
        .filter_map(|e| {
            let p: ToolResultPayload = serde_json::from_value(e.payload.clone()).ok()?;
            let call = args.get(&p.result.id)?;
            Some((call.clone(), p.result.content, p.result.is_error))
        })
        .collect()
}

/// The numbers in the paging line, parsed from the line itself rather than
/// written by hand: `(lines {a}-{b} of {n} shown; recall {seq} with
/// from_line {next} for more)`.
struct Paging {
    a: usize,
    b: usize,
    n: usize,
    seq: u64,
    next: usize,
}

fn paging(text: &str) -> Option<Paging> {
    let inner = text
        .lines()
        .last()?
        .strip_prefix("(lines ")?
        .strip_suffix(" for more)")?;
    let (range, rest) = inner.split_once(" shown; recall ")?;
    let (a, after) = range.split_once('-')?;
    let (b, n) = after.split_once(" of ")?;
    let (seq, next) = rest.split_once(" with from_line ")?;
    Some(Paging {
        a: a.parse().ok()?,
        b: b.parse().ok()?,
        n: n.parse().ok()?,
        seq: seq.parse().ok()?,
        next: next.parse().ok()?,
    })
}

/// The text of every block the projection of `events` holds, so a test can
/// see that a result was stubbed before `recall` brought it back.
fn projected(events: &[Event]) -> String {
    let p = project(events).expect("the fixture projects");
    p.body
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Text(t) => t.clone(),
            ContentBlock::ToolResult(r) => r.content.clone(),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------- T8

/// T8: `recall` by handle returns the original result in full after the
/// projection stubbed it; over the cap it returns its head, the paging
/// line, and `from_line` returns the next part; a missing call says so.
#[tokio::test]
async fn recall_by_handle_returns_the_original_after_the_projection_stubbed_it() {
    let (_dir, log, written) = fixture(vec![
        started(Some("alpha")),
        user("please read the files"),
        call(
            "c1",
            "bash",
            json!({"command": "cat small.txt", "timeout_secs": 30}),
        ),
        result("c1", "one\ntwo\nthree"),
        call("c2", "bash", json!({"command": "seq 1 500"})),
        result("c2", &long_content()),
        result("ghost", "no call made this"),
        evicted(6),
    ]);
    // The premise: the sweep's own event stubs the small result, and the
    // log still holds it in full.
    assert!(
        projected(&written).contains("[result 3 · "),
        "the premise: the projection stubs result 3"
    );
    assert!(written[3].payload.to_string().contains("two"));

    let mut harness = drive(
        reply(&[
            ("r1", json!({"handle": 3})),
            ("r2", json!({"handle": 5})),
            ("r3", json!({"handle": 5, "from_line": 1})),
            ("r4", json!({"handle": 5, "from_line": LONG_LINES})),
            ("r5", json!({"handle": 6})),
        ]),
        log,
    );
    turn(&mut harness, "go").await;
    let events = harness.log().read_all().unwrap();
    let got = recalls(&events);
    assert_eq!(got.len(), 5, "{got:#?}");

    let expect_header = format!(
        "[recalled result 3 · bash {}]",
        short_args(&json!({"command": "cat small.txt", "timeout_secs": 30}))
    );
    let (_, text, is_error) = &got[0];
    assert!(!is_error, "{text}");
    assert_eq!(text, &format!("{expect_header}\none\ntwo\nthree"));
    // In full: the original lines, not the stub's.
    assert!(!text.contains("dropped from context"), "{text}");

    // The long result: whole lines to the cap, then the paging line.
    let lines: Vec<String> = long_content().lines().map(str::to_owned).collect();
    let (_, page1, is_error) = &got[1];
    assert!(!is_error, "{page1}");
    assert!(page1.starts_with("[recalled result 5 · bash"), "{page1}");
    let p = paging(page1).expect("the head is cut and pages");
    assert_eq!(p.n, LONG_LINES);
    assert_eq!(p.a, 1);
    assert_eq!(p.seq, 5);
    assert_eq!(
        page1.lines().last().unwrap(),
        format!(
            "(lines 1-{} of {LONG_LINES} shown; recall 5 with from_line {} for more)",
            p.b, p.next
        )
    );
    assert_eq!(p.next, p.b + 1);
    // The header, the whole lines, and the paging line: nothing else.
    assert_eq!(page1.lines().count(), p.b + 2, "{page1}");
    assert_eq!(
        page1.lines().skip(1).take(p.b).collect::<Vec<_>>(),
        lines[..p.b].iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(page1.len() <= DEFAULT_OUTPUT_CAP, "{}", page1.len());
    // The head fills the cap: one more line would not have fitted.
    assert!(
        page1.len() + lines[p.b].len() + 1 > DEFAULT_OUTPUT_CAP,
        "one more line would fit: {} + {}",
        page1.len(),
        lines[p.b].len()
    );

    // `from_line` 1 is the head again, so the head is deterministic.
    assert_eq!(&got[2].1, page1);

    // The last line alone, with no paging line.
    let (_, tail, is_error) = &got[3];
    assert!(!is_error, "{tail}");
    assert_eq!(tail.lines().count(), 2, "{tail}");
    assert_eq!(tail.lines().nth(1).unwrap(), lines[LONG_LINES - 1]);
    assert!(paging(tail).is_none(), "{tail}");

    // A result whose call is not in the log.
    let (_, ghost, is_error) = &got[4];
    assert!(!is_error, "{ghost}");
    assert_eq!(
        ghost,
        "[recalled result 6 · call not found]\nno call made this"
    );
}

/// T8: `from_line` returns the next part after the paging line, which the
/// call before it named: the model's own next step, on the same thread.
#[tokio::test]
async fn recall_from_line_returns_the_next_part() {
    let (dir, log, _) = fixture(vec![
        started(Some("alpha")),
        user("please read the file"),
        call("c2", "bash", json!({"command": "seq 1 500"})),
        result("c2", &long_content()),
    ]);
    let mut harness = drive(reply(&[("r1", json!({"handle": 3}))]), log);
    turn(&mut harness, "go").await;
    let events = harness.log().read_all().unwrap();
    let (_, head, is_error) = recalls(&events)[0].clone();
    assert!(!is_error, "{head}");
    let p = paging(&head).expect("the head is cut and pages");

    // The second turn, asking for the line the paging line named, on the
    // thread's own file.
    drop(harness);
    let log = ThreadLog::open(dir.path(), thread_id()).unwrap();
    let mut harness = drive(
        reply(&[("r2", json!({"handle": 3, "from_line": p.next}))]),
        log,
    );
    turn(&mut harness, "again").await;
    let events = harness.log().read_all().unwrap();
    let (_, next, is_error) = recalls(&events).last().cloned().unwrap();
    assert!(!is_error, "{next}");

    let lines: Vec<String> = long_content().lines().map(str::to_owned).collect();
    assert_eq!(
        next.lines().skip(1).collect::<Vec<_>>(),
        lines[p.next - 1..]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        "the next part continues where the head stopped"
    );
    assert!(
        !next.contains("for more)"),
        "the rest of the result fits: {next}"
    );
}

// ---------------------------------------------------------------- T8 range

/// T8: a range is `render_range`'s own text, through the cap.
#[tokio::test]
async fn recall_by_range_is_the_renderers_text_through_the_cap() {
    let (_dir, log, written) = fixture(vec![
        started(Some("alpha")),
        user("please read the files"),
        call("c1", "bash", json!({"command": "cat small.txt"})),
        result("c1", "one\ntwo\nthree"),
    ]);
    let expect = render_range(&written, 1, 3, 50);
    assert!(expect.len() <= DEFAULT_OUTPUT_CAP, "{}", expect.len());
    let mut harness = drive(
        reply(&[
            ("r1", json!({"from": 1, "to": 3})),
            ("r2", json!({"from": 1, "to": 3, "handle": 3})),
            ("r3", json!({"from": 1})),
            ("r4", json!({"from_line": 2})),
            ("r5", json!({"limit": 5})),
            ("r6", json!({"handle": 3, "bogus": 1})),
            ("r7", json!({"handle": 999})),
            ("r8", json!({"handle": 0})),
        ]),
        log,
    );
    turn(&mut harness, "go").await;
    let events = harness.log().read_all().unwrap();
    let got = recalls(&events);
    assert_eq!(got.len(), 8, "{got:#?}");

    assert!(!got[0].2, "{}", got[0].1);
    assert_eq!(got[0].1, truncate_output(&expect, DEFAULT_OUTPUT_CAP));
    assert!(got[0].1.contains("please read the files"), "{}", got[0].1);
    assert!(got[0].1.contains("one\ntwo\nthree"), "{}", got[0].1);

    // Every invalid combination, and an unknown key, say why.
    for (_, text, is_error) in &got[1..5] {
        assert!(is_error, "{text}");
        assert_eq!(text, FORM_ERROR);
    }
    assert!(got[5].1.starts_with("invalid arguments: "), "{}", got[5].1);
    assert!(got[5].1.contains("bogus"), "{}", got[5].1);
    // A seq the log does not hold.
    assert!(got[6].2, "{}", got[6].1);
    assert_eq!(got[6].1, "no event at seq 999 in this thread");
    // A seq that isn't a tool result.
    assert!(got[7].2, "{}", got[7].1);
    assert_eq!(
        got[7].1,
        format!(
            "{} is a {}, not a tool result; recall it with from and to",
            0,
            kind_name(EventKind::ThreadStarted)
        )
    );
}

// ---------------------------------------------------------------- T8 query

/// T8: a query is `search`'s output through the cap; no hits says so.
#[tokio::test]
async fn recall_by_query_returns_hits_and_says_when_there_are_none() {
    let (_dir, log, _) = fixture(vec![
        started(Some("alpha")),
        user("please read the files"),
        call("c1", "bash", json!({"command": "cat small.txt"})),
        result("c1", "one\ntwo\nthree"),
    ]);
    let mut harness = drive(
        reply(&[
            ("r1", json!({"query": "THREE"})),
            ("r2", json!({"query": "zzz-nothing-matches"})),
        ]),
        log,
    );
    turn(&mut harness, "go").await;
    let events = harness.log().read_all().unwrap();
    let got = recalls(&events);
    assert_eq!(got.len(), 2, "{got:#?}");

    // The matching line, trimmed, with the kind by its serde name: the
    // fixture's own text, found case-insensitively. The first recall's
    // own result holds "three" too, and is not a hit.
    assert!(!got[0].2, "{}", got[0].1);
    assert!(got[0].1.contains("three"), "{}", got[0].1);
    assert_eq!(
        got[0].1,
        truncate_output(
            &format!("3 · {} · three", kind_name(EventKind::ToolResult)),
            DEFAULT_OUTPUT_CAP
        )
    );

    assert!(!got[1].2, "{}", got[1].1);
    assert_eq!(got[1].1, "no matches for \"zzz-nothing-matches\"");
}

// ------------------------------------------------------------------- T9

/// T9: a recalled result, range or hit from another project is read-only,
/// one from the current project is not, and one from outside any project
/// says that.
#[tokio::test]
async fn recall_marks_what_belongs_to_another_project() {
    let (_dir, log, written) = fixture(vec![
        started(Some("alpha")),
        user("start in alpha"),
        call("c1", "bash", json!({"command": "cat alpha.txt"})),
        result("c1", "alpha result"),
        switched(Some("alpha"), None),
        user("now outside"),
        call("c2", "bash", json!({"command": "cat outside.txt"})),
        result("c2", "outside result"),
        switched(None, Some("beta")),
        user("now in beta"),
        call("c3", "bash", json!({"command": "cat beta.txt"})),
        result("c3", "beta result"),
    ]);
    let alpha =
        "[from project alpha: read-only here; alpha's instructions and files no longer apply]";
    let outside = "[from outside any project: read-only here]";
    let mut harness = drive(
        reply(&[
            ("r1", json!({"handle": 3})),
            ("r2", json!({"handle": 7})),
            ("r3", json!({"handle": 11})),
            ("r4", json!({"from": 3, "to": 3})),
            ("r5", json!({"from": 7, "to": 7})),
            ("r6", json!({"from": 11, "to": 11})),
            ("r7", json!({"query": "result"})),
        ]),
        log,
    );
    turn(&mut harness, "go").await;
    let events = harness.log().read_all().unwrap();
    let got = recalls(&events);
    assert_eq!(got.len(), 7, "{got:#?}");

    // A recalled result from a project the thread has left ...
    let (_, alpha_result, _) = &got[0];
    assert_eq!(
        alpha_result,
        &format!(
            "[recalled result 3 · bash {}]\nalpha result\n{alpha}",
            short_args(&json!({"command": "cat alpha.txt"}))
        )
    );
    // ... one from outside any project ...
    assert_eq!(
        got[1].1,
        format!(
            "[recalled result 7 · bash {}]\noutside result\n{outside}",
            short_args(&json!({"command": "cat outside.txt"}))
        )
    );
    // ... and the current project's own result is not marked.
    assert_eq!(
        got[2].1,
        format!(
            "[recalled result 11 · bash {}]\nbeta result",
            short_args(&json!({"command": "cat beta.txt"}))
        )
    );

    // A one-event range is the renderer's own block, marked.
    let expected_range = |seq: u64, marker: Option<&str>| {
        let block = render_range(&written, seq, seq, 1);
        match marker {
            Some(marker) => match block.split_once('\n') {
                Some((head, rest)) => format!("{head} {marker}\n{rest}"),
                None => format!("{block} {marker}"),
            },
            None => block,
        }
    };
    assert_eq!(got[3].1, expected_range(3, Some(alpha)));
    assert_eq!(got[4].1, expected_range(7, Some(outside)));
    assert_eq!(got[5].1, expected_range(11, None));

    // A query: newest first, so beta, outside, alpha; the two old ones
    // carry their marker on the hit's own line.
    let hits: Vec<&str> = got[6].1.lines().collect();
    assert_eq!(hits.len(), 3, "{hits:#?}");
    assert_eq!(
        hits[0],
        format!("11 · {} · beta result", kind_name(EventKind::ToolResult))
    );
    assert_eq!(
        hits[1],
        format!(
            "7 · {} · outside result {outside}",
            kind_name(EventKind::ToolResult)
        )
    );
    assert_eq!(
        hits[2],
        format!(
            "3 · {} · alpha result {alpha}",
            kind_name(EventKind::ToolResult)
        )
    );
}

// ------------------------------------------------------------------ T10

/// T10: `recall` is offered, named, safe, and its schema is the three
/// forms and nothing else.
#[test]
fn recall_is_offered_with_a_closed_three_form_schema() {
    let specs = harness_specs(true, true);
    let spec = specs
        .iter()
        .find(|s| s.name == RECALL)
        .expect("recall is offered");
    assert_eq!(spec.description, RECALL_DESCRIPTION);
    assert!(harness_names().contains(&RECALL.to_owned()));
    assert!(is_harness_tool(RECALL));
    assert_eq!(HARNESS_CLASS, RiskClass::Safe);

    let forms = spec.schema["anyOf"].as_array().expect("an anyOf of forms");
    assert_eq!(forms.len(), 3);
    let keys: Vec<Vec<String>> = forms
        .iter()
        .map(|f| {
            assert_eq!(f["additionalProperties"], json!(false), "{f}");
            assert_eq!(f["type"], json!("object"), "{f}");
            let mut names: Vec<String> = f["properties"]
                .as_object()
                .expect("properties")
                .keys()
                .cloned()
                .collect();
            names.sort();
            let required: Vec<&str> = f["required"]
                .as_array()
                .expect("required")
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert!(
                required.iter().all(|r| names.iter().any(|n| n == r)),
                "every required key is one of the form's own: {f}"
            );
            names
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            vec!["from_line".to_owned(), "handle".to_owned()],
            vec!["from".to_owned(), "to".to_owned()],
            vec!["limit".to_owned(), "query".to_owned()],
        ]
    );
}
