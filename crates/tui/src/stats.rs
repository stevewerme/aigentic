//! `aigentic stats`: what the thread logs already know about spend
//! (issue #31). Read straight from the logs — the daemon is not
//! involved — so a month of threads costs one walk of the directory and
//! no model calls.
//!
//! Every figure is a projection of the `usage` lines the runtime stamps:
//! price, model, context size and retries. A log written before those
//! fields existed still counts its calls; it just has nothing to price.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, ThreadLog, ThreadRenamedPayload, TurnEndedPayload, UserMessagePayload,
};
use anyhow::{Context, bail};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ulid::Ulid;

use crate::project_cmd::first_line_of;

/// The whole report, and what `--json` prints.
#[derive(Debug, Default, Serialize)]
pub struct Stats {
    /// The window's start, RFC 3339, when `--since` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Every day in the window, oldest first, keyed `YYYY-MM-DD`.
    pub days: Vec<DayStats>,
    /// Every project in the window, by name.
    pub projects: Vec<ProjectStats>,
    /// Up to five threads by spend, dearest first.
    pub threads: Vec<ThreadSpend>,
    /// Threads whose files could not be read; counted so the totals are
    /// never silently short.
    pub unreadable: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct DayStats {
    pub day: String,
    pub calls: u32,
    pub priced_calls: u32,
    pub unpriced_calls: u32,
    /// Sum of `cost_usd` over the day's priced calls.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    /// The largest context any single call in the day carried.
    pub peak_context: u64,
    /// Context tokens summed over the day's calls, over which the mean
    /// and the hit rate are taken.
    pub context_total: u64,
    pub mean_context: u64,
    pub cache_read: u64,
    /// `cache_read / context`, 0 when the day held no calls.
    pub hit_rate: f64,
    /// Turns by their first reason segment (`done`, `provider_error`, …).
    pub turns: BTreeMap<String, u32>,
    pub retries: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct ProjectStats {
    pub project: String,
    pub calls: u32,
    pub priced_calls: u32,
    pub unpriced_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    pub peak_context: u64,
    pub context_total: u64,
    pub mean_context: u64,
    pub cache_read: u64,
    pub hit_rate: f64,
    pub turns: BTreeMap<String, u32>,
    pub retries: u32,
}

#[derive(Debug, Default, Serialize)]
pub struct ThreadSpend {
    pub id: String,
    pub project: String,
    pub title: String,
    pub calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
}

/// `aigentic stats --since 7d --json`. `base` is the threads directory
/// before the per-project split; `project` narrows to one project (the
/// global `--project`), `None` covering every project on the machine.
pub fn run(
    base: &Path,
    project: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let stats = collect(base, project, cutoff)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        print!("{}", render(&stats));
    }
    Ok(())
}

/// `Nd` is now minus N·24h; `YYYY-MM-DD` is that day from midnight UTC.
pub fn parse_since(arg: &str, now: OffsetDateTime) -> anyhow::Result<OffsetDateTime> {
    if let Some(days) = arg.strip_suffix('d') {
        let n: i64 = days
            .parse()
            .with_context(|| format!("--since {arg}: expected Nd or YYYY-MM-DD"))?;
        if n < 0 {
            bail!("--since {arg}: a negative window is not a window");
        }
        return Ok(now - time::Duration::days(n));
    }
    let date = time::Date::parse(
        arg,
        &time::macros::format_description!("[year]-[month]-[day]"),
    )
    .with_context(|| format!("--since {arg}: expected Nd or YYYY-MM-DD"))?;
    Ok(date.midnight().assume_utc())
}

/// Walk every project directory under `base` and fold each thread's
/// events into the day, project and top-thread totals.
pub fn collect(
    base: &Path,
    project: Option<&str>,
    cutoff: Option<OffsetDateTime>,
) -> anyhow::Result<Stats> {
    let mut stats = Stats {
        since: cutoff.map(|c| c.format(&Rfc3339).unwrap_or_default()),
        ..Stats::default()
    };
    // `_none` is the directory for work outside a project: it is a real
    // group, not something to hide.
    let mut groups: Vec<(String, PathBuf)> = Vec::new();
    match std::fs::read_dir(base) {
        Ok(entries) => {
            for entry in entries.filter_map(Result::ok) {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if project.is_some_and(|want| want != name) {
                    continue;
                }
                groups.push((name.to_owned(), path));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", base.display())),
    }
    if let Some(want) = project
        && !groups.iter().any(|(name, _)| name == want)
    {
        // A named project with no threads is an empty report, not an
        // error.
        groups.push((want.to_owned(), base.join(want)));
    }
    groups.sort();

    let mut days: BTreeMap<String, Accum> = BTreeMap::new();
    let mut projects: Vec<ProjectStats> = Vec::new();
    let mut threads: Vec<ThreadSpend> = Vec::new();

    for (name, dir) in groups {
        let mut acc = Accum::default();
        for id in thread_ids(&dir) {
            match ThreadLog::open(&dir, id).and_then(|log| log.read_all()) {
                Ok(events) => {
                    let mut spend = ThreadSpend {
                        id: id.to_string(),
                        project: name.clone(),
                        ..ThreadSpend::default()
                    };
                    absorb(
                        &mut acc,
                        &events,
                        cutoff,
                        &mut days,
                        name.as_str(),
                        &mut spend,
                    );
                    // #40: a thread with no call inside the window is not
                    // a row. Its title is a label, but a row is a total.
                    if spend.calls > 0 {
                        threads.push(spend);
                    }
                }
                Err(_) => stats.unreadable += 1,
            }
        }
        projects.push(ProjectStats {
            project: name,
            ..acc.into_project()
        });
    }

    // Dearest first, and a priced thread outranks an unpriced one. A
    // thread with no price (an endpoint with no price table) is ranked
    // by its call count instead: on an unpriced setup "the dearest
    // threads" would otherwise be five arbitrary ones. The id settles
    // the last tie, so the order is the same on every run.
    threads.sort_by(|a, b| {
        match (a.spent, b.spent) {
            (Some(x), Some(y)) => y
                .partial_cmp(&x)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.calls.cmp(&a.calls))
                .then_with(|| a.id.cmp(&b.id)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            // Neither priced: the busier thread is the more interesting
            // one, and the reference check for #31 (224 calls, ~43.1M
            // context tokens) counts on this thread being listed.
            (None, None) => b.calls.cmp(&a.calls).then_with(|| a.id.cmp(&b.id)),
        }
    });
    threads.truncate(5);
    stats.threads = threads;
    stats.projects = projects;
    stats.days = days.into_iter().map(|(day, a)| a.into_day(day)).collect();
    Ok(stats)
}

fn thread_ids(dir: &Path) -> Vec<Ulid> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<Ulid> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|p| p.file_stem()?.to_str()?.parse().ok())
        .collect();
    ids.sort_unstable_by(|a, b| b.cmp(a));
    ids
}

/// One group's running totals — a day or a project, folded the same way.
#[derive(Default)]
struct Accum {
    calls: u32,
    priced_calls: u32,
    unpriced_calls: u32,
    spent: Option<f64>,
    peak_context: u64,
    context_total: u64,
    cache_read: u64,
    turns: BTreeMap<String, u32>,
    retries: u32,
}

impl Accum {
    fn add_call(&mut self, context: u64, cache_read: u64, cost: Option<f64>) {
        self.calls += 1;
        self.context_total += context;
        self.cache_read += cache_read;
        self.peak_context = self.peak_context.max(context);
        match cost {
            Some(usd) => {
                self.priced_calls += 1;
                self.spent = Some(self.spent.unwrap_or(0.0) + usd);
            }
            None => self.unpriced_calls += 1,
        }
    }

    fn into_project(self) -> ProjectStats {
        ProjectStats {
            calls: self.calls,
            priced_calls: self.priced_calls,
            unpriced_calls: self.unpriced_calls,
            spent: self.spent,
            peak_context: self.peak_context,
            mean_context: mean(self.context_total, self.calls),
            hit_rate: hit_rate(self.cache_read, self.context_total),
            context_total: self.context_total,
            cache_read: self.cache_read,
            turns: self.turns,
            retries: self.retries,
            project: String::new(),
        }
    }

    fn into_day(self, day: String) -> DayStats {
        DayStats {
            calls: self.calls,
            priced_calls: self.priced_calls,
            unpriced_calls: self.unpriced_calls,
            spent: self.spent,
            peak_context: self.peak_context,
            mean_context: mean(self.context_total, self.calls),
            hit_rate: hit_rate(self.cache_read, self.context_total),
            context_total: self.context_total,
            cache_read: self.cache_read,
            turns: self.turns,
            retries: self.retries,
            day,
        }
    }
}

fn mean(total: u64, calls: u32) -> u64 {
    if calls == 0 {
        0
    } else {
        total / u64::from(calls)
    }
}

fn hit_rate(cache_read: u64, context_total: u64) -> f64 {
    if context_total == 0 {
        0.0
    } else {
        cache_read as f64 / context_total as f64
    }
}

/// Fold one thread's events into its project's totals and, for events in
/// the window, its day's totals too. Since #40 the window gates the
/// project and the thread as well; only a thread's title and profile are
/// read from outside it, because those are labels, not totals.
fn absorb(
    project: &mut Accum,
    events: &[aigentic_runtime::aigentic_core::Event],
    cutoff: Option<OffsetDateTime>,
    days: &mut BTreeMap<String, Accum>,
    project_name: &str,
    spend: &mut ThreadSpend,
) {
    let mut renamed: Option<String> = None;
    let mut first_line = String::new();
    for event in events {
        let day = event
            .created_at
            .format(&Rfc3339)
            .ok()
            .map(|s| s[..10].to_owned())
            .unwrap_or_default();
        let in_window = cutoff.is_none_or(|c| event.created_at >= c);
        match event.kind {
            EventKind::AssistantMessage => {
                // #40: the window gates every table, not just the days.
                // A call outside it is invisible to the projects and the
                // top threads too, so all three sums agree.
                if !in_window {
                    continue;
                }
                let Ok(AssistantMessagePayload { usage: Some(u), .. }) =
                    serde_json::from_value(event.payload.clone())
                else {
                    continue;
                };
                // The context of one call, the `reports.rs` formula:
                // what went in, cached or not.
                let context = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
                let cost = if u.estimated { None } else { u.cost_usd };
                project.add_call(context, u.cache_read_tokens, cost);
                spend.calls += 1;
                if let Some(usd) = cost {
                    spend.spent = Some(spend.spent.unwrap_or(0.0) + usd);
                }
                days.entry(day)
                    .or_default()
                    .add_call(context, u.cache_read_tokens, cost);
            }
            EventKind::ProviderRetried => {
                if in_window {
                    project.retries += 1;
                    days.entry(day).or_default().retries += 1;
                }
            }
            EventKind::TurnEnded => {
                if !in_window {
                    continue;
                }
                let Ok(p) = serde_json::from_value::<TurnEndedPayload>(event.payload.clone())
                else {
                    continue;
                };
                // Turns group by the reason's head: `provider_error:
                // http 503` is a `provider_error` turn.
                let head = p.reason.split(':').next().unwrap_or("").to_owned();
                *project.turns.entry(head.clone()).or_default() += 1;
                *days.entry(day).or_default().turns.entry(head).or_default() += 1;
            }
            EventKind::ThreadRenamed => {
                if let Ok(p) = serde_json::from_value::<ThreadRenamedPayload>(event.payload.clone())
                {
                    renamed = Some(p.title);
                }
            }
            EventKind::UserMessage if first_line.is_empty() => {
                // Our own posts only; another user's line is not this
                // thread's opening prompt. `author` carries the user.
                if matches!(event.author, Author::User(_))
                    && let Ok(p) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                {
                    first_line = p
                        .blocks
                        .into_iter()
                        .find_map(|b| match b {
                            ContentBlock::Text(t) => Some(first_line_of(&t)),
                            _ => None,
                        })
                        .unwrap_or_default();
                }
            }
            _ => {}
        }
    }
    spend.title = renamed.unwrap_or(first_line);
    spend.project = project_name.to_owned();
}

/// The text report: a `Cost`-style table of days, then projects, then
/// the dearest threads.
pub fn render(stats: &Stats) -> String {
    let mut out = String::new();
    match &stats.since {
        Some(since) => out.push_str(&format!("since {since}\n")),
        None => out.push_str("all threads\n"),
    }
    let total: f64 = stats.days.iter().filter_map(|d| d.spent).sum();
    let calls: u32 = stats.days.iter().map(|d| d.calls).sum();
    let priced: u32 = stats.days.iter().map(|d| d.priced_calls).sum();
    let unpriced: u32 = stats.days.iter().map(|d| d.unpriced_calls).sum();
    let context: u64 = stats.days.iter().map(|d| d.context_total).sum();
    let cache_read: u64 = stats.days.iter().map(|d| d.cache_read).sum();
    let retries: u32 = stats.days.iter().map(|d| d.retries).sum();
    if priced > 0 {
        out.push_str(&format!(
            "cost       ${total:.4} ({priced} of {} calls priced)\n",
            priced + unpriced
        ));
    } else {
        out.push_str(&format!("cost       unpriced ({unpriced} calls)\n"));
    }
    out.push_str(&format!("calls      {calls}   retries {retries}\n"));
    out.push_str(&format!(
        "context    peak {:>10}   mean {:>10}   cache hit {:.0}%\n",
        stats.days.iter().map(|d| d.peak_context).max().unwrap_or(0),
        mean(context, calls),
        hit_rate(cache_read, context) * 100.0
    ));
    if !stats.days.is_empty() {
        out.push_str("\nday          calls     priced        cost      peak     hit\n");
        for d in &stats.days {
            let cost = match d.spent {
                Some(usd) => format!("${usd:.4}"),
                None => "-".into(),
            };
            out.push_str(&format!(
                "{:<12} {:>5} {:>10} {:>11} {:>9} {:>6.0}%\n",
                d.day,
                d.calls,
                d.priced_calls,
                cost,
                d.peak_context,
                d.hit_rate * 100.0
            ));
        }
    }
    if !stats.projects.is_empty() {
        out.push_str(
            "\nproject                          calls     priced        cost      peak     hit\n",
        );
        for p in &stats.projects {
            let cost = match p.spent {
                Some(usd) => format!("${usd:.4}"),
                None => "-".into(),
            };
            out.push_str(&format!(
                "{:<32} {:>5} {:>10} {:>11} {:>9} {:>6.0}%\n",
                p.project,
                p.calls,
                p.priced_calls,
                cost,
                p.peak_context,
                p.hit_rate * 100.0
            ));
        }
    }
    if !stats.threads.is_empty() {
        out.push_str("\ndearest threads\n");
        for t in &stats.threads {
            let cost = match t.spent {
                Some(usd) => format!("${usd:.4}"),
                None => "-".into(),
            };
            out.push_str(&format!(
                "  {:<26} {:<14} {:>5} calls {:>11}  {}\n",
                t.id, t.project, t.calls, cost, t.title
            ));
        }
    }
    if stats.unreadable > 0 {
        out.push_str(&format!("\n{} thread(s) unreadable\n", stats.unreadable));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use time::macros::datetime;

    /// A log written straight to disk: `ThreadLog::append` stamps
    /// `created_at` itself, and these fixtures need two days in one file.
    fn write_thread(dir: &Path, id: Ulid, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(dir).unwrap();
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
        std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
    }

    fn user(at: &str, text: &str) -> serde_json::Value {
        json!({
            "kind": "user_message",
            "author": {"kind": "user", "id": "steve"},
            "payload": {"blocks": [{"type": "text", "text": text}]},
            "created_at": at,
        })
    }

    fn call(
        at: &str,
        input: u64,
        cache_read: u64,
        output: u64,
        cost: Option<f64>,
        model: Option<&str>,
    ) -> serde_json::Value {
        json!({
            "kind": "assistant_message",
            "author": {"kind": "agent", "id": "assistant"},
            "payload": {
                "blocks": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": cache_read,
                    "cache_write_tokens": 0,
                    "estimated": false,
                    "cost_usd": cost,
                    "model": model,
                }
            },
            "created_at": at,
        })
    }

    fn retried(at: &str) -> serde_json::Value {
        json!({
            "kind": "provider_retried",
            "author": {"kind": "system"},
            "payload": {"attempt": 1, "retries": 3, "reason": "overloaded", "wait_ms": 1000},
            "created_at": at,
        })
    }

    fn turn_ended(at: &str, reason: &str) -> serde_json::Value {
        json!({
            "kind": "turn_ended",
            "author": {"kind": "system"},
            "payload": {"reason": reason, "iterations": 1, "tokens": 0, "touched": []},
            "created_at": at,
        })
    }

    fn renamed(at: &str, title: &str) -> serde_json::Value {
        json!({
            "kind": "thread_renamed",
            "author": {"kind": "system"},
            "payload": {"title": title},
            "created_at": at,
        })
    }

    /// Two projects, two days, priced and unpriced calls, retries, a
    /// done and a `provider_error:` turn, a renamed thread. Every
    /// expectation below is recomputed from these lines, not written out.
    struct Fixture {
        dir: tempfile::TempDir,
        /// Every line written, in order, so an expectation can filter the
        /// fixture the way the code does (#40).
        events: Vec<serde_json::Value>,
    }

    impl Fixture {
        /// The fixture's `assistant_message` lines at or after `cutoff` —
        /// what `--since` says is in the window.
        fn calls_since(&self, cutoff: OffsetDateTime) -> u32 {
            self.events
                .iter()
                .filter(|e| {
                    e["kind"] == "assistant_message"
                        && e["created_at"]
                            .as_str()
                            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                            .is_some_and(|t| t >= cutoff)
                })
                .count() as u32
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let one = Ulid::generate();
        let two = Ulid::generate();
        let mut lines = vec![
            user("2026-09-22T09:00:00Z", "first thread please"),
            call(
                "2026-09-22T09:00:01Z",
                100,
                900,
                10,
                Some(0.25),
                Some("gpt-4o"),
            ),
            call(
                "2026-09-22T09:00:02Z",
                200,
                800,
                20,
                Some(0.5),
                Some("gpt-4o"),
            ),
            retried("2026-09-22T09:00:03Z"),
            turn_ended("2026-09-22T09:00:04Z", "done"),
        ];
        let mut events = lines.clone();
        write_thread(&dir.path().join("alpha"), one, &lines);
        lines = vec![
            user("2026-09-23T10:00:00Z", "second thread"),
            renamed("2026-09-23T10:00:01Z", "a named thread"),
            call("2026-09-23T10:00:02Z", 50, 50, 5, None, None),
            retried("2026-09-23T10:00:03Z"),
            turn_ended("2026-09-23T10:00:04Z", "provider_error: http 503"),
        ];
        events.extend(lines.clone());
        write_thread(&dir.path().join("beta"), two, &lines);
        Fixture { dir, events }
    }

    #[test]
    fn totals_are_the_fixture_recomputed() {
        let f = fixture();
        let base = f.dir.path();
        // Only the first thread is priced, so the check has a spent sum
        // and a mixture to prove both branches.
        let stats = collect(base, None, None).unwrap();

        let calls: u32 = stats.days.iter().map(|d| d.calls).sum();
        let priced: u32 = stats.days.iter().map(|d| d.priced_calls).sum();
        let unpriced: u32 = stats.days.iter().map(|d| d.unpriced_calls).sum();
        assert_eq!(calls, 3, "{stats:?}");
        assert_eq!((priced, unpriced), (2, 1), "{stats:?}");

        // Peak and mean come from the same contexts the fixture holds.
        let contexts = [1000u64, 1000, 100];
        let peak = *contexts.iter().max().unwrap();
        let ctx: u64 = contexts.iter().sum();
        assert_eq!(stats.days.iter().map(|d| d.peak_context).max(), Some(peak));
        assert_eq!(
            stats.days.iter().map(|d| d.context_total).sum::<u64>(),
            ctx,
            "{stats:?}"
        );
        let days = stats.days.len() as u32;
        assert_eq!(
            stats.days.iter().map(|d| d.mean_context).sum::<u64>(),
            contexts
                .chunks(2)
                .map(|c| c.iter().sum::<u64>() / c.len() as u64)
                .sum::<u64>()
        );

        // Retries and the turn reasons, grouped by head.
        assert_eq!(stats.days.iter().map(|d| d.retries).sum::<u32>(), 2);
        let done: u32 = stats
            .days
            .iter()
            .map(|d| d.turns.get("done").copied().unwrap_or(0))
            .sum();
        let errors: u32 = stats
            .days
            .iter()
            .map(|d| d.turns.get("provider_error").copied().unwrap_or(0))
            .sum();
        assert_eq!((done, errors), (1, 1), "{stats:?}");

        // Spent is the sum of the priced `cost_usd`, hit rate the read
        // share of the context, both recomputed here.
        let spent: f64 = stats.days.iter().filter_map(|d| d.spent).sum();
        assert!((spent - 0.75).abs() < 1e-9, "{spent}");
        let cache_read: u64 = stats.days.iter().map(|d| d.cache_read).sum();
        assert_eq!(cache_read, 1750);
        let hit = cache_read as f64 / ctx as f64;
        let reported: f64 = stats.days.iter().map(|d| d.context_total).sum::<u64>() as f64;
        assert!(
            (cache_read as f64 / reported - hit).abs() < 1e-9,
            "{hit} != the reported totals"
        );
        let _ = days;

        // Projects come out in name order with the same arithmetic.
        let names: Vec<&str> = stats.projects.iter().map(|p| p.project.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        let alpha = stats
            .projects
            .iter()
            .find(|p| p.project == "alpha")
            .unwrap();
        let beta = stats.projects.iter().find(|p| p.project == "beta").unwrap();
        assert_eq!((alpha.calls, beta.calls), (2, 1));
        assert!((alpha.spent.unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(beta.spent, None);
        assert_eq!(alpha.hit_rate, 1700.0 / 2000.0);
    }

    #[test]
    fn since_filters_every_table_by_the_fixture_dates() {
        let f = fixture();
        // The fixture's second day is 2026-09-23: a cutoff inside it
        // drops 2026-09-22's day.
        let cutoff = parse_since("2026-09-23", datetime!(2026-09-24 00:00:00 UTC)).unwrap();
        let stats = collect(f.dir.path(), None, Some(cutoff)).unwrap();
        let days: Vec<&str> = stats.days.iter().map(|d| d.day.as_str()).collect();
        assert_eq!(days, vec!["2026-09-23"], "{stats:?}");
        assert_eq!(stats.days[0].calls, 1);
        // #40 flips this: the window gates the projects and the threads
        // too, so all three sums are the same window. The old comment
        // ("the whole thread is still worth knowing about") is overruled
        // by the issue: a project row that counted 3254 all-time calls
        // beside a 2457-call window is exactly the bug.
        let in_window = f.calls_since(cutoff);
        assert_eq!(in_window, 1, "the fixture filtered as the code should");
        assert_eq!(
            stats.projects.iter().map(|p| p.calls).sum::<u32>(),
            in_window
        );
        assert_eq!(
            stats.threads.iter().map(|t| t.calls).sum::<u32>(),
            in_window
        );
        // beta is the only thread with a call in the window; alpha, whose
        // two calls predate it, leaves no row at all.
        assert_eq!(
            stats
                .threads
                .iter()
                .map(|t| t.project.as_str())
                .collect::<Vec<_>>(),
            vec!["beta"]
        );

        // `Nd` is now minus N whole days; a week back keeps both days.
        let week = parse_since("7d", datetime!(2026-09-24 12:00:00 UTC)).unwrap();
        assert_eq!(week, datetime!(2026-09-17 12:00:00 UTC));
        let stats = collect(f.dir.path(), None, Some(week)).unwrap();
        assert_eq!(stats.days.len(), 2);
        assert_eq!(
            stats.projects.iter().map(|p| p.calls).sum::<u32>(),
            f.calls_since(week)
        );
    }

    #[test]
    fn parse_since_rejects_nonsense() {
        let now = datetime!(2026-09-24 12:00:00 UTC);
        assert!(parse_since("yesterday", now).is_err());
        assert!(parse_since("-1d", now).is_err());
        assert_eq!(
            parse_since("2d", now).unwrap(),
            datetime!(2026-09-22 12:00:00 UTC)
        );
    }

    #[test]
    fn dearest_threads_rank_by_spend_then_calls_with_titles() {
        let f = fixture();
        let stats = collect(f.dir.path(), None, None).unwrap();
        // The fixture sorted in-test by spend, unpriced last.
        let mut expected: Vec<(String, Option<f64>, u32)> = vec![
            ("alpha".to_owned(), Some(0.75), 2),
            ("beta".to_owned(), None, 1),
        ];
        expected.sort_by(|a, b| {
            b.1.unwrap_or(-1.0)
                .partial_cmp(&a.1.unwrap_or(-1.0))
                .unwrap()
                .then_with(|| b.2.cmp(&a.2))
        });
        let got: Vec<(String, Option<f64>, u32)> = stats
            .threads
            .iter()
            .map(|t| (t.project.clone(), t.spent, t.calls))
            .collect();
        assert_eq!(got, expected, "{:?}", stats.threads);
        // The renamed thread takes the rename; the other its first line.
        let alpha = stats.threads.iter().find(|t| t.project == "alpha").unwrap();
        assert_eq!(alpha.title, "first thread please");
        let beta = stats.threads.iter().find(|t| t.project == "beta").unwrap();
        assert_eq!(beta.title, "a named thread");
    }

    #[test]
    fn json_round_trips_the_same_totals() {
        let f = fixture();
        let stats = collect(f.dir.path(), None, None).unwrap();
        let text = serde_json::to_string(&stats).unwrap();
        let back: serde_json::Value = serde_json::from_str(&text).unwrap();
        let day = &back["days"][0];
        assert_eq!(day["calls"], json!(stats.days[0].calls));
        assert_eq!(day["day"], json!(stats.days[0].day));
        let total: u32 = back["days"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["calls"].as_u64().unwrap() as u32)
            .sum();
        assert_eq!(total, stats.days.iter().map(|d| d.calls).sum::<u32>());
        // The text report holds the same figure.
        assert!(render(&stats).contains("calls      3"));
    }

    #[test]
    fn one_project_narrows_the_walk() {
        let f = fixture();
        let stats = collect(f.dir.path(), Some("beta"), None).unwrap();
        assert_eq!(stats.projects.len(), 1);
        assert_eq!(stats.projects[0].project, "beta");
        assert_eq!(stats.projects[0].calls, 1);
        // A project with no threads is an empty report, not an error.
        let none = collect(f.dir.path(), Some("gamma"), None).unwrap();
        assert_eq!(none.projects[0].calls, 0);
    }

    #[test]
    fn an_unreadable_thread_is_counted_not_swallowed() {
        let f = fixture();
        let dir = f.dir.path().join("alpha");
        std::fs::write(
            dir.join(format!("{}.jsonl", Ulid::generate())),
            b"{not json",
        )
        .unwrap();
        let stats = collect(f.dir.path(), None, None).unwrap();
        assert_eq!(stats.unreadable, 1, "{stats:?}");
        // And the readable thread is still fully counted.
        assert_eq!(stats.projects.iter().map(|p| p.calls).sum::<u32>(), 3);
    }
}
