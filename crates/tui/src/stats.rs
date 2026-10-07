//! `aigentic stats`: what the thread logs already know about spend
//! (issue #31). Read straight from the logs — the daemon is not
//! involved — so a month of threads costs one walk of the directory and
//! no model calls.
//!
//! Every figure is a projection of the `usage` lines the runtime stamps:
//! price, model, context size and retries. A log written before those
//! fields existed still counts its calls; it just has nothing to price.
//! Since #40 the prices come from the config file too: an unpriced
//! call's model or profile is looked up in the current `[prices]` and
//! the result marked estimated, so a retro can put dollars on old work.
//! Since #46 the memory extractions are a line of their own, priced from
//! their `memory_extracted` usage, so the agent's turns keep their own
//! calls and context. Since #49 a utility title call is on that same
//! line, priced from its stamped `thread_renamed` usage.
//!
//! Since #83 the walk follows #9's layout: every log is
//! `threads/<id>.jsonl`, and a legacy `threads/<project>/<id>.jsonl` is
//! still read, with the thread attributed to its project from its own
//! lines (`crate::threads_index`).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use aigentic_runtime::Prices;
use aigentic_runtime::aigentic_core::{Author, ContentBlock, Event, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, DecisionAnswer, DecisionKind, DecisionRecord, Invoker,
    MemoryExtractedPayload, RunStartedPayload, SkillLoadedPayload, ThreadRenamedPayload,
    ThreadStartedPayload, ToolResultPayload, TurnEndedPayload, Usage, UserMessagePayload,
    decision_records,
};
use aigentic_server::config::Config;
use aigentic_server::workspaces::Workspace;
use anyhow::{Context, bail};
use serde::Serialize;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use ulid::Ulid;

use crate::project_cmd::first_line_of;
use crate::threads_index::{self, Found, NO_PROJECT};

/// The whole report, and what `--json` prints.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Stats {
    /// The window's start, RFC 3339, when `--since` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Every day in the window, oldest first, keyed `YYYY-MM-DD`.
    pub days: Vec<DayStats>,
    /// Every project in the window, by name.
    pub projects: Vec<ProjectStats>,
    /// Up to five threads by effective spend, costliest first.
    pub threads: Vec<ThreadSpend>,
    /// Threads whose files could not be read; counted so the totals are
    /// never silently short.
    pub unreadable: u32,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct DayStats {
    pub day: String,
    pub calls: u32,
    /// Calls priced from their `cost_usd` stamp.
    pub priced_calls: u32,
    /// Calls priced now from the config's table because they had no
    /// stamp (issue #40).
    pub price_estimated_calls: u32,
    pub unpriced_calls: u32,
    /// Calls with neither a `model` nor a `profile`: they predate the
    /// stamping of them, and `--assume-profile` is the only way to price
    /// them. A subset of `unpriced_calls` (issue #40).
    pub unstamped_calls: u32,
    /// Sum of the priced calls' stamps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    /// Sum of the retro-priced calls' dollars, kept apart from `spent`
    /// so a guessed dollar never looks like a measured one (issue #40).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_estimated_spent: Option<f64>,
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
    /// The side jobs — `memory_extracted` and utility-titled
    /// `thread_renamed` lines — counted and priced apart from the calls
    /// (issues #46, #49). Folding them into the calls would make the hit
    /// rate fiction and hide the loop's own cost. `extractions` and
    /// `titles` name the parts of `job_calls`.
    pub job_calls: u32,
    pub job_priced_calls: u32,
    pub job_price_estimated_calls: u32,
    pub job_unpriced_calls: u32,
    pub extractions: u32,
    pub titles: u32,
    /// Tokens the side jobs sent and produced, also apart: folding them
    /// into the calls' context would make the hit rate fiction.
    pub job_context: u64,
    pub job_output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price_estimated_spent: Option<f64>,
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ProjectStats {
    pub project: String,
    pub calls: u32,
    pub priced_calls: u32,
    pub price_estimated_calls: u32,
    pub unpriced_calls: u32,
    pub unstamped_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_estimated_spent: Option<f64>,
    pub peak_context: u64,
    pub context_total: u64,
    pub mean_context: u64,
    pub cache_read: u64,
    pub hit_rate: f64,
    pub turns: BTreeMap<String, u32>,
    pub retries: u32,
    pub job_calls: u32,
    pub job_priced_calls: u32,
    pub job_price_estimated_calls: u32,
    pub job_unpriced_calls: u32,
    pub extractions: u32,
    pub titles: u32,
    pub job_context: u64,
    pub job_output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price_estimated_spent: Option<f64>,
}

/// The sum of two optional dollar figures: `Some` when either is,
/// `None` when neither is. The two sides are the calls' dollars and the
/// side jobs' (issues #46, #49), added only where a single figure is
/// wanted.
fn add(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (None, None) => None,
        (x, y) => Some(x.unwrap_or(0.0) + y.unwrap_or(0.0)),
    }
}

/// What a thread's dollars really are (#40): stamped plus retro-priced,
/// `Some` when either exists, `None` when neither does.
fn effective(t: &ThreadSpend) -> Option<f64> {
    add(t.spent, t.price_estimated_spent)
}

/// The same sum for the drill-downs, which carry a `ThreadReport`.
fn effective_report(t: &ThreadReport) -> Option<f64> {
    add(t.spent, t.price_estimated_spent)
}

#[derive(Debug, Default, PartialEq, Serialize)]
pub struct ThreadSpend {
    pub id: String,
    pub project: String,
    pub title: String,
    pub calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_estimated_spent: Option<f64>,
}

/// One thread read on its own (`stats --thread <id>`, issue #40): the
/// same totals as a day or a project, plus what only a single thread
/// knows.
#[derive(Debug, Serialize)]
pub struct ThreadReport {
    pub id: String,
    pub project: String,
    pub title: String,
    /// The last profile any of the thread's usage lines carried; empty
    /// when none did.
    pub profile: String,
    pub calls: u32,
    pub priced_calls: u32,
    pub price_estimated_calls: u32,
    pub unpriced_calls: u32,
    pub unstamped_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_estimated_spent: Option<f64>,
    pub peak_context: u64,
    pub context_total: u64,
    pub mean_context: u64,
    pub cache_read: u64,
    pub hit_rate: f64,
    /// `context_evicted` lines in the window: how often old context was
    /// swept out.
    pub sweeps: u32,
    /// `tool_result` lines whose payload says `is_error`.
    pub tool_errors: u32,
    pub turns: BTreeMap<String, u32>,
    pub retries: u32,
    /// The thread's side jobs (issues #46, #49), the same
    /// fields a day and a project carry.
    pub job_calls: u32,
    pub job_priced_calls: u32,
    pub job_price_estimated_calls: u32,
    pub job_unpriced_calls: u32,
    pub extractions: u32,
    pub titles: u32,
    pub job_context: u64,
    pub job_output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price_estimated_spent: Option<f64>,
    /// Summed `wall_secs` over the thread's measured `turn_ended` lines,
    /// and how much of that the machine spent asleep (issue #47). Both
    /// come from the lines' own numbers, never from `created_at`, which
    /// counts the idle time between two turns as turn time. Absent when
    /// no line carried one — an old log, or a thread that slept nowhere
    /// — so nothing is invented for a log that never measured itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slept_secs: Option<u64>,
}

/// `stats --issue <n>`: threads that name the issue or belong to its run.
#[derive(Debug, Serialize)]
pub struct IssueReport {
    pub issue: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Dearest first, the same ordering as the top-thread table.
    pub threads: Vec<ThreadReport>,
    /// The aggregate over the matched threads.
    pub total: IssueTotals,
}

#[derive(Debug, Default, Serialize)]
pub struct IssueTotals {
    pub threads: u32,
    pub calls: u32,
    pub priced_calls: u32,
    pub price_estimated_calls: u32,
    pub unpriced_calls: u32,
    pub unstamped_calls: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_estimated_spent: Option<f64>,
    pub context_total: u64,
    pub cache_read: u64,
    pub hit_rate: f64,
    pub sweeps: u32,
    pub tool_errors: u32,
    pub turns: BTreeMap<String, u32>,
    pub retries: u32,
    pub job_calls: u32,
    pub job_priced_calls: u32,
    pub job_price_estimated_calls: u32,
    pub job_unpriced_calls: u32,
    pub extractions: u32,
    pub titles: u32,
    pub job_context: u64,
    pub job_output: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_spent: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub job_price_estimated_spent: Option<f64>,
}

impl IssueTotals {
    fn of(threads: &[ThreadReport]) -> Self {
        let mut total = Self {
            threads: threads.len() as u32,
            ..Self::default()
        };
        let sum = |slot: &mut Option<f64>, value: Option<f64>| {
            if let Some(v) = value {
                *slot = Some(slot.unwrap_or(0.0) + v);
            }
        };
        for t in threads {
            total.calls += t.calls;
            total.priced_calls += t.priced_calls;
            total.price_estimated_calls += t.price_estimated_calls;
            total.unpriced_calls += t.unpriced_calls;
            total.unstamped_calls += t.unstamped_calls;
            sum(&mut total.spent, t.spent);
            sum(&mut total.price_estimated_spent, t.price_estimated_spent);
            total.context_total += t.context_total;
            total.cache_read += t.cache_read;
            total.sweeps += t.sweeps;
            total.tool_errors += t.tool_errors;
            total.retries += t.retries;
            total.job_calls += t.job_calls;
            total.job_priced_calls += t.job_priced_calls;
            total.job_price_estimated_calls += t.job_price_estimated_calls;
            total.job_unpriced_calls += t.job_unpriced_calls;
            total.extractions += t.extractions;
            total.titles += t.titles;
            total.job_context += t.job_context;
            total.job_output += t.job_output;
            sum(&mut total.job_spent, t.job_spent);
            sum(
                &mut total.job_price_estimated_spent,
                t.job_price_estimated_spent,
            );
            for (head, n) in &t.turns {
                *total.turns.entry(head.clone()).or_default() += n;
            }
        }
        total.hit_rate = hit_rate(total.cache_read, total.context_total);
        total
    }
}

/// What one call's dollars are (issue #40).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Cost {
    /// The runtime's `cost_usd` stamp, exact.
    Stamped(f64),
    /// No stamp, but the config's table knows the call's model or
    /// profile: dollars computed now, marked estimated.
    Retro(f64),
    /// Nothing to price it with.
    Unpriced,
}

/// The price tables `stats` reads: the config's `[profiles.*.prices]`,
/// keyed by profile name and by model, plus an optional
/// `--assume-profile` table for calls that carry neither.
#[derive(Debug, Default, Clone)]
pub struct PriceBook {
    by_profile: BTreeMap<String, Prices>,
    by_model: BTreeMap<String, Prices>,
    /// `(name, prices)` of `--assume-profile`, for a call with neither
    /// `model` nor `profile`.
    assumed: Option<(String, Prices)>,
}

impl PriceBook {
    /// Read the price tables out of the config. `assume`, when given,
    /// names the profile to price unstamped calls with; an unknown name,
    /// or a profile without a `[prices]` block, is an error naming it
    /// (issue #40).
    pub fn from_config(config: &Config, assume: Option<&str>) -> anyhow::Result<Self> {
        let mut by_profile = BTreeMap::new();
        let mut by_model = BTreeMap::new();
        for (name, profile) in &config.profiles {
            let Some(prices) = profile.prices.as_ref().map(|pc| pc.prices()) else {
                continue;
            };
            by_profile.insert(name.clone(), prices);
            // The profiles are a BTreeMap, so the first name wins a
            // model collision on every run: deterministic, not
            // arbitrary.
            by_model.entry(profile.model.clone()).or_insert(prices);
        }
        let assumed = match assume {
            None => None,
            Some(name) => {
                let (name, profile) = config
                    .select(Some(name))
                    .map_err(|e| anyhow::anyhow!("--assume-profile: {e}"))?;
                let prices = profile
                    .prices
                    .as_ref()
                    .map(|pc| pc.prices())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "--assume-profile {name}: profile {name} has no [prices] block"
                        )
                    })?;
                Some((name.to_owned(), prices))
            }
        };
        Ok(Self {
            by_profile,
            by_model,
            assumed,
        })
    }

    /// The table for one call: its `model` (exact) first, then its
    /// `profile` name, then — only for a call that carries neither — the
    /// `--assume-profile` table. `None` means unpriced.
    pub(crate) fn table(&self, u: &Usage) -> Option<Prices> {
        if let Some(model) = u.model.as_deref()
            && let Some(prices) = self.by_model.get(model)
        {
            return Some(*prices);
        }
        if let Some(profile) = u.profile.as_deref()
            && let Some(prices) = self.by_profile.get(profile)
        {
            return Some(*prices);
        }
        if u.model.is_none() && u.profile.is_none() {
            return self.assumed.as_ref().map(|(_, prices)| *prices);
        }
        None
    }
}

/// Classify one usage line. A runtime-estimated line is never priced:
/// dollars from guessed tokens are fiction, whatever the table says. A
/// stamped cost is kept exactly as it is. Otherwise the config's current
/// table prices the call, and the result is marked estimated.
pub(crate) fn classify_cost(u: &Usage, book: &PriceBook) -> Cost {
    if u.estimated {
        return Cost::Unpriced;
    }
    if let Some(usd) = u.cost_usd {
        return Cost::Stamped(usd);
    }
    match book.table(u) {
        Some(prices) => Cost::Retro(prices.cost_usd(u)),
        None => Cost::Unpriced,
    }
}

/// A call with neither a `model` nor a `profile` predates the stamping
/// of them (issue #40): without `--assume-profile` it cannot be priced,
/// and the report says how many such calls there are.
fn is_unstamped(u: &Usage) -> bool {
    !u.estimated && u.model.is_none() && u.profile.is_none()
}

/// `aigentic stats --since 7d --json`. `base` is the threads directory
/// before the per-project split; `project` narrows to one project (the
/// global `--project`), `None` covering every project on the machine.
/// `book` is the config's price tables (issue #40).
pub fn run(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    since: Option<&str>,
    json: bool,
    book: &PriceBook,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let stats = collect(base, workspaces, project, cutoff, book)?;
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

/// Walk every log under `base` — the flat directory and any legacy
/// project directory a migration left behind (#9, #83) — and fold each
/// thread into the totals of the project its own lines name. A log that
/// can be read but says nothing about its project is `(no project)`:
/// a real group, not something to hide.
pub fn collect(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<Stats> {
    let mut stats = Stats {
        since: cutoff.map(|c| c.format(&Rfc3339).unwrap_or_default()),
        ..Stats::default()
    };

    let mut days: BTreeMap<String, Accum> = BTreeMap::new();
    let mut by_project: BTreeMap<String, Accum> = BTreeMap::new();
    let mut threads: Vec<ThreadSpend> = Vec::new();

    for found in threads_index::catalogue(base) {
        let Ok(events) = found.read() else {
            // An unreadable log has no lines to attribute it by, so it
            // counts whenever the filter cannot be shown to exclude it:
            // with no `--project`, or when its legacy directory is the
            // requested project.
            if project.is_none() || found.legacy.as_deref() == project {
                stats.unreadable += 1;
            }
            continue;
        };
        let name = attributed(&found, &events, workspaces);
        if project.is_some_and(|want| want != name) {
            continue;
        }
        let acc = by_project.entry(name.clone()).or_default();
        let mut thread = Accum::default();
        let meta = absorb(acc, &mut thread, &events, cutoff, &mut days, book);
        // #40: a thread with no call inside the window is not a row. Its
        // title is a label, but a row is a total. #46: an extraction
        // inside the window is a total too, so a thread that only
        // extracted is still a row — with 0 calls and its side-job money.
        if thread.calls > 0 || thread.job_calls > 0 {
            threads.push(thread.into_spend(found.id.to_string(), &name, meta.title));
        }
    }
    // A named project with no threads is an empty group, not an error.
    if let Some(want) = project {
        by_project.entry(want.to_owned()).or_default();
    }

    // Dearest first on the *effective* spend (stamped + retro-priced),
    // and a priced thread outranks an unpriced one. A thread with no
    // price at all (an endpoint with no price table) is ranked by its
    // call count instead: on an unpriced setup "the costliest threads"
    // would otherwise be five arbitrary ones. The id settles the last
    // tie, so the order is the same on every run.
    threads.sort_by(|a, b| {
        match (effective(a), effective(b)) {
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
    stats.projects = project_rows(by_project);
    stats.days = days.into_iter().map(|(day, a)| a.into_day(day)).collect();
    Ok(stats)
}

/// The project name a found log's events give it, as the label: never an
/// empty string.
fn attributed(found: &Found, events: &[Event], workspaces: &[Workspace]) -> String {
    let name = threads_index::project_of(events, found.legacy.as_deref(), workspaces);
    threads_index::label(name.as_deref()).to_owned()
}

/// One row per project, by name, with `(no project)` last.
fn project_rows(by_project: BTreeMap<String, Accum>) -> Vec<ProjectStats> {
    let mut rows: Vec<(String, Accum)> = by_project.into_iter().collect();
    rows.sort_by(|(a, _), (b, _)| {
        (a == NO_PROJECT)
            .cmp(&(b == NO_PROJECT))
            .then_with(|| a.cmp(b))
    });
    rows.into_iter()
        .map(|(name, acc)| ProjectStats {
            project: name,
            ..acc.into_project()
        })
        .collect()
}

pub fn run_thread(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    id: Ulid,
    since: Option<&str>,
    json: bool,
    book: &PriceBook,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let report = collect_thread(base, workspaces, project, id, cutoff, book)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_thread(&report));
    }
    Ok(())
}

/// `aigentic stats --issue <n>`: manually named threads, plus matching
/// run leads and their step threads, costliest first, with a total.
pub fn run_issue(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    issue: u64,
    since: Option<&str>,
    json: bool,
    book: &PriceBook,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let report = collect_issue(base, workspaces, project, issue, cutoff, book)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_issue(&report));
    }
    Ok(())
}

/// Read one thread's log and fold it: the id is looked for flat first,
/// then in the legacy project directories. An id no log holds is an
/// error naming it, so a typo never reads as an empty report. A thread
/// attributed to another project than `--project` asks for is an error
/// naming both.
fn collect_thread(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    id: Ulid,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<ThreadReport> {
    for found in threads_index::catalogue(base) {
        if found.id != id {
            continue;
        }
        let events = found
            .read()
            .with_context(|| format!("reading {}", found.path().display()))?;
        let name = attributed(&found, &events, workspaces);
        if let Some(want) = project
            && want != name
        {
            bail!("thread {id} is in {name}, not {want}");
        }
        let mut thread = Accum::default();
        let mut dropped = Accum::default();
        let mut dropped_days = BTreeMap::new();
        let meta = absorb(
            &mut dropped,
            &mut thread,
            &events,
            cutoff,
            &mut dropped_days,
            book,
        );
        return Ok(thread.into_report(id.to_string(), &name, meta));
    }
    bail!("no thread {id} found under {}", base.display())
}

/// Every manually named thread, run lead, and child of a matching run,
/// folded with the same totals as the other stats views.
fn collect_issue(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    issue: u64,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<IssueReport> {
    let mut threads: Vec<ThreadReport> = Vec::new();
    let mut run_leads = HashSet::new();
    let mut candidates = Vec::new();
    for found in threads_index::catalogue(base) {
        let Ok(events) = found.read() else {
            continue;
        };
        let run_issue = events.iter().find_map(|event| {
            (event.kind == EventKind::RunStarted)
                .then(|| serde_json::from_value::<RunStartedPayload>(event.payload.clone()).ok())
                .flatten()
                .map(|payload| payload.issue)
        });
        if run_issue == Some(issue) {
            run_leads.insert(found.id);
        }
        let parent_thread = events.iter().find_map(|event| {
            (event.kind == EventKind::ThreadStarted)
                .then(|| serde_json::from_value::<ThreadStartedPayload>(event.payload.clone()).ok())
                .flatten()
                .and_then(|payload| payload.parent_thread)
        });
        let name = attributed(&found, &events, workspaces);
        if project.is_some_and(|want| want != name) {
            continue;
        }
        let mut thread = Accum::default();
        let mut dropped = Accum::default();
        let mut dropped_days = BTreeMap::new();
        let meta = absorb(
            &mut dropped,
            &mut thread,
            &events,
            cutoff,
            &mut dropped_days,
            book,
        );
        let direct_match = run_issue == Some(issue) || names_issue(&meta, issue);
        if !direct_match && parent_thread.is_none() {
            continue;
        }
        // The window applies here too: a matched thread with no call
        // inside it has no row, exactly as in the main report (and,
        // since #46, no extraction inside it either).
        if thread.calls == 0 && thread.job_calls == 0 {
            continue;
        }
        candidates.push((
            thread.into_report(found.id.to_string(), &name, meta),
            parent_thread,
            direct_match,
        ));
    }
    for (thread, parent_thread, direct_match) in candidates {
        if direct_match || parent_thread.is_some_and(|parent| run_leads.contains(&parent)) {
            threads.push(thread);
        }
    }
    threads.sort_by(|a, b| match (effective_report(a), effective_report(b)) {
        (Some(x), Some(y)) => y
            .partial_cmp(&x)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.calls.cmp(&a.calls))
            .then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.calls.cmp(&a.calls).then_with(|| a.id.cmp(&b.id)),
    });
    let total = IssueTotals::of(&threads);
    Ok(IssueReport {
        issue,
        since: cutoff.map(|c| c.format(&Rfc3339).unwrap_or_default()),
        threads,
        total,
    })
}

/// The side jobs a log can carry (issues #46, #49): the line names its
/// parts, and both feed one count because both are calls the loop made
/// for its own bookkeeping, not steps of a turn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SideJob {
    Extraction,
    Title,
}

/// One group's running totals — a day or a project, folded the same way.
#[derive(Debug, Default)]
pub(crate) struct Accum {
    /// The thread's summed `wall_secs` and `slept_secs` over the
    /// `turn_ended` lines that carried them (issue #47).
    wall_secs: Option<u64>,
    slept_secs: Option<u64>,
    calls: u32,
    priced_calls: u32,
    price_estimated_calls: u32,
    unpriced_calls: u32,
    unstamped_calls: u32,
    spent: Option<f64>,
    price_estimated_spent: Option<f64>,
    peak_context: u64,
    context_total: u64,
    cache_read: u64,
    turns: BTreeMap<String, u32>,
    retries: u32,
    /// Side jobs (issues #46, #49). Their tokens stay out of
    /// `context_total`/`cache_read` — that arithmetic describes the
    /// agent's calls, and a side job sends a prompt with none of the
    /// call's cache behaviour.
    job_calls: u32,
    job_priced_calls: u32,
    job_price_estimated_calls: u32,
    job_unpriced_calls: u32,
    /// `job_calls` split by kind: how many extractions, how many titles.
    extractions: u32,
    titles: u32,
    job_context: u64,
    job_output: u64,
    job_spent: Option<f64>,
    job_price_estimated_spent: Option<f64>,
}

impl Accum {
    pub(crate) fn add_call(&mut self, context: u64, cache_read: u64, cost: Cost) {
        self.calls += 1;
        self.context_total += context;
        self.cache_read += cache_read;
        self.peak_context = self.peak_context.max(context);
        match cost {
            Cost::Stamped(usd) => {
                self.priced_calls += 1;
                self.spent = Some(self.spent.unwrap_or(0.0) + usd);
            }
            Cost::Retro(usd) => {
                self.price_estimated_calls += 1;
                self.price_estimated_spent = Some(self.price_estimated_spent.unwrap_or(0.0) + usd);
            }
            Cost::Unpriced => self.unpriced_calls += 1,
        }
    }

    /// A thread *row*'s dollars: the calls' plus the side jobs'
    /// (issues #46, #49). The thread table prints one figure per thread, so
    /// that figure is the thread's whole spend and "costliest threads"
    /// ranks by it. The day and project tables print the two separately,
    /// so their `spent` stays the calls' and a reader can still see what
    /// the main loop cost on its own.
    pub(crate) fn row_spent(&self) -> Option<f64> {
        add(self.spent, self.job_spent)
    }

    /// The same sum for the retro-priced dollars.
    pub(crate) fn row_estimated(&self) -> Option<f64> {
        add(self.price_estimated_spent, self.job_price_estimated_spent)
    }

    /// One side-job line — a `memory_extracted`, or a `thread_renamed`
    /// with a usage since #49 — the same three price outcomes as a call,
    /// into the job counters.
    pub(crate) fn add_job(&mut self, job: SideJob, context: u64, output: u64, cost: Cost) {
        self.job_calls += 1;
        match job {
            SideJob::Extraction => self.extractions += 1,
            SideJob::Title => self.titles += 1,
        }
        self.job_context += context;
        self.job_output += output;
        match cost {
            Cost::Stamped(usd) => {
                self.job_priced_calls += 1;
                self.job_spent = Some(self.job_spent.unwrap_or(0.0) + usd);
            }
            Cost::Retro(usd) => {
                self.job_price_estimated_calls += 1;
                self.job_price_estimated_spent =
                    Some(self.job_price_estimated_spent.unwrap_or(0.0) + usd);
            }
            Cost::Unpriced => self.job_unpriced_calls += 1,
        }
    }

    fn into_project(self) -> ProjectStats {
        ProjectStats {
            calls: self.calls,
            priced_calls: self.priced_calls,
            price_estimated_calls: self.price_estimated_calls,
            unpriced_calls: self.unpriced_calls,
            unstamped_calls: self.unstamped_calls,
            spent: self.spent,
            price_estimated_spent: self.price_estimated_spent,
            peak_context: self.peak_context,
            mean_context: mean(self.context_total, self.calls),
            hit_rate: hit_rate(self.cache_read, self.context_total),
            context_total: self.context_total,
            cache_read: self.cache_read,
            turns: self.turns,
            retries: self.retries,
            job_calls: self.job_calls,
            job_priced_calls: self.job_priced_calls,
            job_price_estimated_calls: self.job_price_estimated_calls,
            job_unpriced_calls: self.job_unpriced_calls,
            extractions: self.extractions,
            titles: self.titles,
            job_context: self.job_context,
            job_output: self.job_output,
            job_spent: self.job_spent,
            job_price_estimated_spent: self.job_price_estimated_spent,
            project: String::new(),
        }
    }

    fn into_day(self, day: String) -> DayStats {
        DayStats {
            calls: self.calls,
            priced_calls: self.priced_calls,
            price_estimated_calls: self.price_estimated_calls,
            unpriced_calls: self.unpriced_calls,
            unstamped_calls: self.unstamped_calls,
            spent: self.spent,
            price_estimated_spent: self.price_estimated_spent,
            peak_context: self.peak_context,
            mean_context: mean(self.context_total, self.calls),
            hit_rate: hit_rate(self.cache_read, self.context_total),
            context_total: self.context_total,
            cache_read: self.cache_read,
            turns: self.turns,
            retries: self.retries,
            job_calls: self.job_calls,
            job_priced_calls: self.job_priced_calls,
            job_price_estimated_calls: self.job_price_estimated_calls,
            job_unpriced_calls: self.job_unpriced_calls,
            extractions: self.extractions,
            titles: self.titles,
            job_context: self.job_context,
            job_output: self.job_output,
            job_spent: self.job_spent,
            job_price_estimated_spent: self.job_price_estimated_spent,
            day,
        }
    }

    /// The row the report's thread table shows. The row's dollars are the
    /// thread's whole spend — the calls' and the side jobs' — and its
    /// `calls` are the calls alone, because a memory extraction or a
    /// title call is not a turn's step (issues #46, #49). The side-jobs
    /// line beside it is where a reader sees how much of the money is
    /// side-job money.
    fn into_spend(self, id: String, project: &str, title: String) -> ThreadSpend {
        ThreadSpend {
            id,
            project: project.to_owned(),
            title,
            calls: self.calls,
            spent: self.row_spent(),
            price_estimated_spent: self.row_estimated(),
        }
    }

    /// The `--thread` and `--issue` shape: the same totals as a day or a
    /// project, plus what only a single thread knows (`sweeps`,
    /// `tool_errors`, its own labels).
    fn into_report(self, id: String, project: &str, meta: ThreadMeta) -> ThreadReport {
        ThreadReport {
            id,
            project: project.to_owned(),
            title: meta.title,
            profile: meta.profile,
            calls: self.calls,
            priced_calls: self.priced_calls,
            price_estimated_calls: self.price_estimated_calls,
            unpriced_calls: self.unpriced_calls,
            unstamped_calls: self.unstamped_calls,
            spent: self.spent,
            price_estimated_spent: self.price_estimated_spent,
            peak_context: self.peak_context,
            mean_context: mean(self.context_total, self.calls),
            hit_rate: hit_rate(self.cache_read, self.context_total),
            context_total: self.context_total,
            cache_read: self.cache_read,
            sweeps: meta.sweeps,
            tool_errors: meta.tool_errors,
            turns: self.turns,
            retries: self.retries,
            job_calls: self.job_calls,
            job_priced_calls: self.job_priced_calls,
            job_price_estimated_calls: self.job_price_estimated_calls,
            job_unpriced_calls: self.job_unpriced_calls,
            extractions: self.extractions,
            titles: self.titles,
            job_context: self.job_context,
            job_output: self.job_output,
            job_spent: self.job_spent,
            job_price_estimated_spent: self.job_price_estimated_spent,
            wall_secs: self.wall_secs,
            slept_secs: self.slept_secs,
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

/// What only one thread can tell the report: its labels, and the two
/// counters that are not calls.
#[derive(Debug, Default)]
struct ThreadMeta {
    title: String,
    /// The full text of the thread's first own-user message, text blocks
    /// joined — not the truncated display title. `--issue` matches it.
    first_message: String,
    /// A `skill_loaded` with `invoked_by: user` seen *before* the first
    /// own-user message: the thread began with a slash command, whose
    /// arguments are then the first message.
    skill_invoked_by_user: bool,
    /// The last `profile` any usage line in the thread carried.
    profile: String,
    sweeps: u32,
    tool_errors: u32,
}

/// Fold one thread's events into its own totals, its project's, and —
/// for events in the window — its day's. Since #40 the window gates the
/// project and the thread as well; only a thread's labels are read from
/// outside it, because those are labels, not totals.
fn absorb(
    project: &mut Accum,
    thread: &mut Accum,
    events: &[aigentic_runtime::aigentic_core::Event],
    cutoff: Option<OffsetDateTime>,
    days: &mut BTreeMap<String, Accum>,
    book: &PriceBook,
) -> ThreadMeta {
    let mut meta = ThreadMeta::default();
    let mut renamed: Option<String> = None;
    let mut own_user_seen = false;
    // The project and the days are the same window partitioned two ways,
    // so each accepted call is replayed into both once the thread is
    // fully folded. The side jobs — extractions and, since #49, titled
    // calls — are replayed the same way, with their own counters.
    let mut calls: Vec<(String, u64, u64, Cost, bool)> = Vec::new();
    let mut job_days: Vec<(String, u64, u64, Cost, SideJob)> = Vec::new();
    let mut retry_days: Vec<String> = Vec::new();
    let mut turn_days: Vec<(String, String)> = Vec::new();

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
                let cost = classify_cost(&u, book);
                let unstamped = matches!(cost, Cost::Unpriced) && is_unstamped(&u);
                if let Some(profile) = &u.profile {
                    meta.profile = profile.clone();
                }
                thread.add_call(context, u.cache_read_tokens, cost);
                if unstamped {
                    thread.unstamped_calls += 1;
                }
                calls.push((day, context, u.cache_read_tokens, cost, unstamped));
            }
            EventKind::MemoryExtracted => {
                // #46: the extraction is a priced call the log made on
                // its own, counted on its own line so the turns keep
                // their calls and their context arithmetic.
                if !in_window {
                    continue;
                }
                let Ok(p) = serde_json::from_value::<MemoryExtractedPayload>(event.payload.clone())
                else {
                    continue;
                };
                let mut usage = p.usage;
                // The payload's own `model` is the provider that ran the
                // extraction (issue #18), so a line written before the
                // runtime stamped a cost can still be priced by name.
                if usage.model.is_none() {
                    usage.model = Some(p.model);
                }
                let context =
                    usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
                let cost = classify_cost(&usage, book);
                thread.add_job(SideJob::Extraction, context, usage.output_tokens, cost);
                job_days.push((day, context, usage.output_tokens, cost, SideJob::Extraction));
            }
            EventKind::ProviderRetried => {
                if in_window {
                    thread.retries += 1;
                    retry_days.push(day);
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
                *thread.turns.entry(head.clone()).or_default() += 1;
                if let Some(wall) = p.wall_secs {
                    *thread.wall_secs.get_or_insert(0) += wall;
                }
                if let Some(slept) = p.slept_secs {
                    *thread.slept_secs.get_or_insert(0) += slept;
                }
                turn_days.push((day, head));
            }
            EventKind::ContextEvicted => {
                if in_window {
                    meta.sweeps += 1;
                }
            }
            EventKind::ToolResult => {
                if in_window
                    && serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                        .is_ok_and(|p| p.result.is_error)
                {
                    meta.tool_errors += 1;
                }
            }
            EventKind::ThreadRenamed => {
                let Ok(p) = serde_json::from_value::<ThreadRenamedPayload>(event.payload.clone())
                else {
                    continue;
                };
                // The title shows whatever wrote it, in or out of the
                // window; only the call behind it is counted.
                renamed = Some(p.title);
                // Issue #49: a line with a usage is a utility title
                // call. One without it — every line written before this,
                // and a person's `/rename` — is no call at all, so there
                // is nothing to count and nothing to price after the
                // fact: an old title and a personal rename look alike.
                if !in_window {
                    continue;
                }
                let Some(mut usage) = p.usage else { continue };
                if usage.model.is_none() {
                    usage.model = p.model;
                }
                let context =
                    usage.input_tokens + usage.cache_read_tokens + usage.cache_write_tokens;
                let cost = classify_cost(&usage, book);
                thread.add_job(SideJob::Title, context, usage.output_tokens, cost);
                job_days.push((day, context, usage.output_tokens, cost, SideJob::Title));
            }
            EventKind::SkillLoaded => {
                // Only a slash command in the client counts, and only
                // before the thread's opening prompt: a skill the model
                // loaded itself says nothing about the issue.
                if !own_user_seen
                    && let Ok(p) =
                        serde_json::from_value::<SkillLoadedPayload>(event.payload.clone())
                    && p.invoked_by == Invoker::User
                {
                    meta.skill_invoked_by_user = true;
                }
            }
            EventKind::UserMessage if !own_user_seen => {
                // Our own posts only; another user's line is not this
                // thread's opening prompt. `author` carries the user.
                if matches!(event.author, Author::User(_))
                    && let Ok(p) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                {
                    own_user_seen = true;
                    meta.first_message = p
                        .blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                }
            }
            _ => {}
        }
    }

    for (day, context, cache_read, cost, unstamped) in calls {
        project.add_call(context, cache_read, cost);
        if unstamped {
            project.unstamped_calls += 1;
        }
        let d = days.entry(day).or_default();
        d.add_call(context, cache_read, cost);
        if unstamped {
            d.unstamped_calls += 1;
        }
    }
    for (day, context, output, cost, job) in job_days {
        project.add_job(job, context, output, cost);
        days.entry(day)
            .or_default()
            .add_job(job, context, output, cost);
    }
    for day in retry_days {
        project.retries += 1;
        days.entry(day).or_default().retries += 1;
    }
    for (day, head) in turn_days {
        *project.turns.entry(head.clone()).or_default() += 1;
        *days.entry(day).or_default().turns.entry(head).or_default() += 1;
    }

    meta.title = renamed.unwrap_or_else(|| first_line_of(&meta.first_message));
    meta
}

/// A thread matches `#<n>` when its *first own-user message* (a) carries
/// the first `#` followed by digits as that number, with a boundary on
/// each side — so `#40`, `see #40.` and `#40's` match and `#400` does
/// not —
/// or (b) follows a user-invoked `skill_loaded` and its first
/// whitespace-separated token is the number, which is how a slash
/// command's arguments arrive.
fn names_issue(meta: &ThreadMeta, issue: u64) -> bool {
    hashed_issue_number(&meta.first_message) == Some(issue)
        || (meta.skill_invoked_by_user
            && meta
                .first_message
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<u64>().ok())
                == Some(issue))
}

/// The number of the *first* `#` in the text that is followed by digits,
/// `None` when there is none. The `#` must start the text or follow a
/// non-alphanumeric character. The digit run is taken whole, so `#400`
/// names 400, never 40; whatever follows it is not checked, so `#40x`
/// and `#40's` name 40. `a#40` names nothing.
fn hashed_issue_number(text: &str) -> Option<u64> {
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c != '#' {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j == i + 1 {
            // A `#` with no digits after it is not a reference; keep
            // looking for the first that is.
            continue;
        }
        let starts = i == 0 || !chars[i - 1].is_alphanumeric();
        if !starts {
            return None;
        }
        return chars[i + 1..j].iter().collect::<String>().parse().ok();
    }
    None
}

/// A cost cell: stamped, estimated, both, or `-`. A guessed dollar is
/// never dressed up as a measured one (`$` vs `~$`, issue #40).
pub(crate) fn money(stamped: Option<f64>, estimated: Option<f64>) -> String {
    match (stamped, estimated) {
        (Some(a), Some(b)) => format!("${a:.4}+~${b:.4}"),
        (Some(a), None) => format!("${a:.4}"),
        (None, Some(b)) => format!("~${b:.4}"),
        (None, None) => "-".into(),
    }
}

/// The side-jobs dollars cell of the day and project tables (issues
/// #46, #49): empty when the window held no side job at all, so the old
/// header and the old rows stand.
fn side_cell(show: bool, stamped: Option<f64>, estimated: Option<f64>) -> String {
    if show {
        format!(" {:>8}", money(stamped, estimated))
    } else {
        String::new()
    }
}

/// The cost line every report shares: what was measured and what was
/// guessed, never added together (issue #40).
fn cost_line(
    stamped: f64,
    estimated: f64,
    priced: u32,
    price_estimated: u32,
    unpriced: u32,
    calls: u32,
) -> String {
    let counts = format!(
        "{priced} priced, {price_estimated} estimated, {unpriced} unpriced of {calls} calls"
    );
    if priced > 0 && price_estimated > 0 {
        format!("cost       ${stamped:.4} + ~${estimated:.4} ({counts})")
    } else if priced > 0 {
        format!("cost       ${stamped:.4} ({counts})")
    } else if price_estimated > 0 {
        format!("cost       ~${estimated:.4} ({counts})")
    } else {
        format!("cost       unpriced ({unpriced} calls)")
    }
}

/// The side jobs' parts, in the line's own wording: `1 title`,
/// `2 extractions`, or both. Empty only when there are none — no caller
/// prints it then.
fn job_parts(extractions: u32, titles: u32) -> String {
    let mut parts = Vec::new();
    if extractions > 0 {
        parts.push(format!("{extractions} extraction{}", plural(extractions)));
    }
    if titles > 0 {
        parts.push(format!("{titles} title{}", plural(titles)));
    }
    parts.join(", ")
}

fn plural(n: u32) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The side-jobs line `stats` prints when the window held any (issues
/// #46, #49): the dollars by class, the priced/estimated/unpriced split
/// the calls' cost line uses, which parts the jobs were, and the tokens
/// they sent and produced. The two dollar classes are never added
/// together (#40's rule), and the side jobs are never folded into the
/// calls' figures.
#[allow(clippy::too_many_arguments)]
fn side_line(
    stamped: f64,
    estimated: f64,
    priced: u32,
    price_estimated: u32,
    unpriced: u32,
    extractions: u32,
    titles: u32,
    context: u64,
    output: u64,
) -> String {
    let detail = format!(
        "{priced} priced, {price_estimated} estimated, {unpriced} unpriced; {}, \
         in {context} out {output}",
        job_parts(extractions, titles),
    );
    if stamped > 0.0 && estimated > 0.0 {
        format!("side jobs  ${stamped:.4} + ~${estimated:.4} ({detail})")
    } else if stamped > 0.0 {
        format!("side jobs  ${stamped:.4} ({detail})")
    } else if estimated > 0.0 {
        format!("side jobs  ~${estimated:.4} ({detail})")
    } else {
        format!("side jobs  unpriced ({detail})")
    }
}

/// The whole-cost line under the calls' and the side jobs' lines: the
/// `cost_line` split applied to both, so a reader sees the two halves
/// while their sum is one figure (issues #46, #49).
fn total_line(stamped: f64, estimated: f64, calls: u32, jobs: u32) -> String {
    let counts = format!("{calls} calls + {jobs} side jobs");
    if stamped > 0.0 && estimated > 0.0 {
        format!("total      ${stamped:.4} + ~${estimated:.4} ({counts})")
    } else if stamped > 0.0 {
        format!("total      ${stamped:.4} ({counts})")
    } else if estimated > 0.0 {
        format!("total      ~${estimated:.4} ({counts})")
    } else {
        format!("total      unpriced ({counts})")
    }
}

/// `stats --thread`: one thread's money, context and turn reasons.
pub fn render_thread(t: &ThreadReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("thread     {}\n", t.id));
    out.push_str(&format!("project    {}\n", t.project));
    out.push_str(&format!("title      {}\n", t.title));
    let profile = if t.profile.is_empty() {
        "-"
    } else {
        t.profile.as_str()
    };
    out.push_str(&format!("profile    {profile}\n"));
    out.push_str(&format!(
        "{}\n",
        cost_line(
            t.spent.unwrap_or(0.0),
            t.price_estimated_spent.unwrap_or(0.0),
            t.priced_calls,
            t.price_estimated_calls,
            t.unpriced_calls,
            t.calls
        )
    ));
    out.push_str(&format!(
        "calls      {}   retries {}   sweeps {}   tool errors {}\n",
        t.calls, t.retries, t.sweeps, t.tool_errors
    ));
    // Only when the thread ran a side job: a thread without any prints
    // exactly what it printed before (issues #46, #49).
    if t.job_calls > 0 {
        out.push_str(&format!(
            "{}\n",
            side_line(
                t.job_spent.unwrap_or(0.0),
                t.job_price_estimated_spent.unwrap_or(0.0),
                t.job_priced_calls,
                t.job_price_estimated_calls,
                t.job_unpriced_calls,
                t.extractions,
                t.titles,
                t.job_context,
                t.job_output
            )
        ));
    }
    out.push_str(&format!(
        "context    peak {:>10}   mean {:>10}   cache hit {:.0}%\n",
        t.peak_context,
        t.mean_context,
        t.hit_rate * 100.0
    ));
    out.push_str(&format!("turns      {}\n", turns_line(&t.turns)));
    // Only when a logged turn measured sleep: a thread whose turns all
    // predate the field (or that slept nowhere) prints no `time` line at
    // all, so the old rendering is untouched.
    if let Some(line) = time_line(t) {
        out.push_str(&line);
        out.push('\n');
    }
    if t.unstamped_calls > 0 {
        out.push_str(&format!(
            "stamping   {} calls predate model stamping \
             (pass --assume-profile NAME to estimate them)\n",
            t.unstamped_calls
        ));
    }
    out
}

/// The `time` line of a single-thread report, or nothing when the
/// thread's turns never measured sleep (issue #47). Floor minutes, the
/// same rounding the turn line uses; the wall part is left out when the
/// log's lines were written before `wall_secs` existed.
fn time_line(s: &ThreadReport) -> Option<String> {
    let slept = s.slept_secs.filter(|secs| *secs > 0)?;
    Some(match s.wall_secs {
        Some(wall) => format!(
            "time       wall {} min   slept {} min",
            wall / 60,
            slept / 60
        ),
        None => format!("time       slept {} min", slept / 60),
    })
}

/// `stats --issue <n>`: the matched threads, costliest first, and the
/// total over them.
pub fn render_issue(report: &IssueReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("issue {}\n", report.issue));
    // `all threads` is the report's label for "no window"; under an issue
    // it would only repeat the header.
    if let Some(since) = &report.since {
        out.push_str(&format!("since {since}\n"));
    }
    let total = &report.total;
    out.push_str(&format!(
        "{}\n",
        cost_line(
            total.spent.unwrap_or(0.0),
            total.price_estimated_spent.unwrap_or(0.0),
            total.priced_calls,
            total.price_estimated_calls,
            total.unpriced_calls,
            total.calls
        )
    ));
    out.push_str(&format!(
        "threads    {}   calls {}   retries {}   sweeps {}   tool errors {}\n",
        total.threads, total.calls, total.retries, total.sweeps, total.tool_errors
    ));
    // A build cycle's side jobs — its extractions and its title calls —
    // are part of what it cost, so the totals block carries the same two
    // lines `stats` prints (issues #46, #49), and only when the window
    // held any.
    if total.job_calls > 0 {
        out.push_str(&format!(
            "{}\n",
            side_line(
                total.job_spent.unwrap_or(0.0),
                total.job_price_estimated_spent.unwrap_or(0.0),
                total.job_priced_calls,
                total.job_price_estimated_calls,
                total.job_unpriced_calls,
                total.extractions,
                total.titles,
                total.job_context,
                total.job_output
            )
        ));
        out.push_str(&format!(
            "{}\n",
            total_line(
                total.spent.unwrap_or(0.0) + total.job_spent.unwrap_or(0.0),
                total.price_estimated_spent.unwrap_or(0.0)
                    + total.job_price_estimated_spent.unwrap_or(0.0),
                total.calls,
                total.job_calls
            )
        ));
    }
    out.push_str(&format!(
        "context    total {:>12}   cache hit {:.0}%\n",
        total.context_total,
        total.hit_rate * 100.0
    ));
    out.push_str(&format!("turns      {}\n", turns_line(&total.turns)));
    if report.threads.is_empty() {
        out.push_str("\nno thread names this issue\n");
        return out;
    }
    out.push_str("\nthread                              calls               cost  title\n");
    for t in &report.threads {
        out.push_str(&format!(
            "  {:<26} {:<14} {:>5} calls {:>18}  {}\n",
            t.id,
            t.project,
            t.calls,
            money(t.spent, t.price_estimated_spent),
            t.title
        ));
    }
    out.push_str(&format!(
        "  total                      {:>5} calls {:>18}\n",
        total.calls,
        money(total.spent, total.price_estimated_spent)
    ));
    out
}

/// `done 3   provider_error 1`, or `-` when no turn ended in the window.
fn turns_line(turns: &BTreeMap<String, u32>) -> String {
    if turns.is_empty() {
        return "-".to_owned();
    }
    turns
        .iter()
        .map(|(head, n)| format!("{head} {n}"))
        .collect::<Vec<_>>()
        .join("   ")
}

/// The text report: a `Cost`-style table of days, then projects, then
/// the costliest threads.
pub fn render(stats: &Stats) -> String {
    let mut out = String::new();
    match &stats.since {
        Some(since) => out.push_str(&format!("since {since}\n")),
        None => out.push_str("all threads\n"),
    }
    let stamped: f64 = stats.days.iter().filter_map(|d| d.spent).sum();
    let estimated: f64 = stats
        .days
        .iter()
        .filter_map(|d| d.price_estimated_spent)
        .sum();
    let calls: u32 = stats.days.iter().map(|d| d.calls).sum();
    let priced: u32 = stats.days.iter().map(|d| d.priced_calls).sum();
    let price_estimated: u32 = stats.days.iter().map(|d| d.price_estimated_calls).sum();
    let unpriced: u32 = stats.days.iter().map(|d| d.unpriced_calls).sum();
    let unstamped: u32 = stats.days.iter().map(|d| d.unstamped_calls).sum();
    let context: u64 = stats.days.iter().map(|d| d.context_total).sum();
    let cache_read: u64 = stats.days.iter().map(|d| d.cache_read).sum();
    let retries: u32 = stats.days.iter().map(|d| d.retries).sum();
    // The side-job totals, summed the same way (issues #46, #49), with
    // their two parts named so the line can say what the jobs were.
    let jobs: u32 = stats.days.iter().map(|d| d.job_calls).sum();
    let extractions: u32 = stats.days.iter().map(|d| d.extractions).sum();
    let titles: u32 = stats.days.iter().map(|d| d.titles).sum();
    let job_priced: u32 = stats.days.iter().map(|d| d.job_priced_calls).sum();
    let job_price_estimated: u32 = stats.days.iter().map(|d| d.job_price_estimated_calls).sum();
    let job_unpriced: u32 = stats.days.iter().map(|d| d.job_unpriced_calls).sum();
    let job_context: u64 = stats.days.iter().map(|d| d.job_context).sum();
    let job_output: u64 = stats.days.iter().map(|d| d.job_output).sum();
    let job_stamped: f64 = stats.days.iter().filter_map(|d| d.job_spent).sum();
    let job_estimated: f64 = stats
        .days
        .iter()
        .filter_map(|d| d.job_price_estimated_spent)
        .sum();
    // #40: the two dollars are never added together. `$31.78 + ~$1.59`
    // says what was measured and what was guessed.
    out.push_str(&format!(
        "{}\n",
        cost_line(stamped, estimated, priced, price_estimated, unpriced, calls)
    ));
    out.push_str(&format!("calls      {calls}   retries {retries}\n"));
    // The side jobs get their own line and a whole-cost line under it,
    // only when the window held any (issues #46, #49): a window without
    // any renders byte-identically to before.
    if jobs > 0 {
        out.push_str(&format!(
            "{}\n",
            side_line(
                job_stamped,
                job_estimated,
                job_priced,
                job_price_estimated,
                job_unpriced,
                extractions,
                titles,
                job_context,
                job_output
            )
        ));
        out.push_str(&format!(
            "{}\n",
            total_line(
                stamped + job_stamped,
                estimated + job_estimated,
                calls,
                jobs
            )
        ));
    }
    if unstamped > 0 {
        out.push_str(&format!(
            "stamping   {unstamped} calls predate model stamping \
             (pass --assume-profile NAME to estimate them)\n"
        ));
    }
    out.push_str(&format!(
        "context    peak {:>10}   mean {:>10}   cache hit {:.0}%\n",
        stats.days.iter().map(|d| d.peak_context).max().unwrap_or(0),
        mean(context, calls),
        hit_rate(cache_read, context) * 100.0
    ));
    // The side jobs' dollars get a column of their own in both tables
    // (issues #46, #49), and only when the window held any: the cost
    // column keeps meaning "the calls", and a window without side jobs
    // renders byte-identically to before.
    let job_header = if jobs > 0 { "     jobs" } else { "" };
    if !stats.days.is_empty() {
        out.push_str(&format!(
            "\nday          calls{job_header}     priced               cost      peak     hit\n"
        ));
        for d in &stats.days {
            let cell = side_cell(jobs > 0, d.job_spent, d.job_price_estimated_spent);
            out.push_str(&format!(
                "{:<12} {:>5}{cell} {:>10} {:>18} {:>9} {:>6.0}%\n",
                d.day,
                d.calls,
                d.priced_calls,
                money(d.spent, d.price_estimated_spent),
                d.peak_context,
                d.hit_rate * 100.0
            ));
        }
    }
    if !stats.projects.is_empty() {
        out.push_str(&format!(
            "\nproject                          calls{job_header}     priced               cost      peak     hit\n"
        ));
        for p in &stats.projects {
            let cell = side_cell(jobs > 0, p.job_spent, p.job_price_estimated_spent);
            out.push_str(&format!(
                "{:<32} {:>5}{cell} {:>10} {:>18} {:>9} {:>6.0}%\n",
                p.project,
                p.calls,
                p.priced_calls,
                money(p.spent, p.price_estimated_spent),
                p.peak_context,
                p.hit_rate * 100.0
            ));
        }
    }
    if !stats.threads.is_empty() {
        out.push_str("\ncostliest threads\n");
        for t in &stats.threads {
            out.push_str(&format!(
                "  {:<26} {:<14} {:>5} calls {:>18}  {}\n",
                t.id,
                t.project,
                t.calls,
                money(t.spent, t.price_estimated_spent),
                t.title
            ));
        }
    }
    if stats.unreadable > 0 {
        out.push_str(&format!("\n{} thread(s) unreadable\n", stats.unreadable));
    }
    out
}

/// `aigentic stats --decisions` (issue #74): the decision record — how
/// many decisions of each kind the harness proposed, and how the
/// operator answered them. No producer writes the events yet, so on a
/// machine built today this prints `no decisions recorded`.
pub fn run_decisions(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    since: Option<&str>,
    json: bool,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let report = collect_decisions(base, workspaces, project, cutoff)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_decisions(&report));
    }
    Ok(())
}

/// The decision record, and what `--json` prints.
#[derive(Debug, Default, Serialize)]
pub struct DecisionReport {
    /// The window's start, RFC 3339, when `--since` was given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// One row per kind with a proposal in the window, in
    /// `DecisionKind`'s declaration order.
    pub kinds: Vec<DecisionKindStats>,
    /// Answers that named no proposal, or answered one twice, or did not
    /// parse: counted, never guessed.
    pub orphans: u32,
    /// Threads whose files could not be read; counted so a figure is
    /// never silently short (the same count `collect` makes).
    pub unreadable: u32,
}

/// One kind's row. `yes`, `no`, `corrected` and `withdrawn` count the
/// answers by what they said, whichever author wrote them; `rate` and
/// `last_30` count only the operator's own answers, so a `withdrawn` the
/// system wrote never flatters the record.
#[derive(Debug, Serialize)]
pub struct DecisionKindStats {
    pub kind: DecisionKind,
    /// True for #85's start-up proposals (a `startup-` call id), which
    /// get their own row so the model's own `project` record isn't
    /// diluted by them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub startup: bool,
    pub proposed: u32,
    pub yes: u32,
    pub no: u32,
    pub corrected: u32,
    pub withdrawn: u32,
    pub pending: u32,
    /// `yes / (yes + no + corrected)` over the operator's answers, as a
    /// whole percentage truncated (never rounded up, so a record tested
    /// at 95% is never read as more); absent when nobody answered.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate: Option<u32>,
    /// The same ratio over the 30 most recent proposals a person
    /// answered; absent when there are none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_30: Option<u32>,
}

/// Fold every walked thread's decisions and gather the proposals inside
/// the window. A proposal counts when **the proposal** is inside it,
/// with whatever answer it has: an answer inside the window to a
/// proposal before it is left out.
pub fn collect_decisions(
    base: &Path,
    workspaces: &[Workspace],
    project: Option<&str>,
    cutoff: Option<OffsetDateTime>,
) -> anyhow::Result<DecisionReport> {
    let mut report = DecisionReport {
        since: cutoff.map(|c| c.format(&Rfc3339).unwrap_or_default()),
        ..DecisionReport::default()
    };
    let mut records: Vec<DecisionRecord> = Vec::new();
    for found in threads_index::catalogue(base) {
        let Ok(events) = found.read() else {
            // The same rule `collect` counts by: an unreadable log has
            // no lines to attribute it by, so it counts unless the
            // filter can be shown to exclude it.
            if project.is_none() || found.legacy.as_deref() == project {
                report.unreadable += 1;
            }
            continue;
        };
        if project.is_some_and(|want| want != attributed(&found, &events, workspaces)) {
            continue;
        }
        let fold = decision_records(&events);
        report.orphans += fold.orphans as u32;
        records.extend(fold.records);
    }
    if let Some(cutoff) = cutoff {
        records.retain(|r| r.at >= cutoff);
    }
    report.kinds = kind_rows(&records);
    Ok(report)
}

/// The decisions table's narrowest name column: what it was before
/// start-up rows (issue #85), so a table of plain rows is unchanged.
const MIN_NAME_WIDTH: usize = 14;

/// The 30 most recent answers `last 30` is taken over: ADR 0002's "the
/// last 30 proposals of that kind that a person answered".
const LAST_N: usize = 30;

/// One row per `(kind, startup)` that has a proposal, each kind's
/// start-up row right after its plain one and kinds in `DecisionKind`'s
/// declaration order.
///
/// A subset with no proposals has no row: a kind whose plain subset is
/// empty but whose start-up subset isn't prints the start-up row alone,
/// so there is no all-zero row and no empty slice reaches `percent`.
fn kind_rows(records: &[DecisionRecord]) -> Vec<DecisionKindStats> {
    let mut rows = Vec::new();
    for kind in ALL_KINDS {
        for startup in [false, true] {
            let of_kind: Vec<&DecisionRecord> = records
                .iter()
                .filter(|r| r.kind == kind && r.startup == startup)
                .collect();
            if of_kind.is_empty() {
                continue;
            }
            rows.push(kind_row(kind, startup, &of_kind));
        }
    }
    rows
}

/// One `(kind, startup)`'s figures: the counts by what each answer said,
/// then `rate` and `last_30` over the operator's own answers.
fn kind_row(kind: DecisionKind, startup: bool, of_kind: &[&DecisionRecord]) -> DecisionKindStats {
    let mut row = DecisionKindStats {
        kind,
        startup,
        proposed: of_kind.len() as u32,
        yes: 0,
        no: 0,
        corrected: 0,
        withdrawn: 0,
        pending: 0,
        rate: None,
        last_30: None,
    };
    // The operator's own answers, kept apart from the counts above:
    // `rate` is ADR 0002's "from the operator's own answers".
    let (mut person_yes, mut person_no, mut person_corrected) = (0u32, 0u32, 0u32);
    let mut answered: Vec<&DecisionRecord> = Vec::new();
    for record in of_kind {
        let Some(answer) = &record.answer else {
            row.pending += 1;
            continue;
        };
        match answer.answer {
            DecisionAnswer::Yes => row.yes += 1,
            DecisionAnswer::No => row.no += 1,
            DecisionAnswer::Corrected => row.corrected += 1,
            DecisionAnswer::Withdrawn => row.withdrawn += 1,
        }
        if !is_person(&answer.by) {
            continue;
        }
        match answer.answer {
            DecisionAnswer::Yes => {
                person_yes += 1;
                answered.push(record);
            }
            DecisionAnswer::No => {
                person_no += 1;
                answered.push(record);
            }
            DecisionAnswer::Corrected => {
                person_corrected += 1;
                answered.push(record);
            }
            // `withdrawn` closes a proposal nobody answered, so it
            // is no part of what a person's record was.
            DecisionAnswer::Withdrawn => {}
        }
    }
    row.rate = percent(person_yes, person_yes + person_no + person_corrected);
    // The most recent by proposal time, the proposal's id settling a
    // tie, as the costliest-threads order does.
    answered.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.id.cmp(&a.id)));
    answered.truncate(LAST_N);
    row.last_30 = percent(
        answered
            .iter()
            .filter(|r| {
                matches!(
                    r.answer.as_ref().map(|a| a.answer),
                    Some(DecisionAnswer::Yes)
                )
            })
            .count() as u32,
        answered.len() as u32,
    );
    row
}

/// Every kind, in declaration order: a new variant fails to compile here
/// until it is given its place in the report.
const ALL_KINDS: [DecisionKind; 6] = [
    DecisionKind::Project,
    DecisionKind::Job,
    DecisionKind::Ticket,
    DecisionKind::Knowledge,
    DecisionKind::Route,
    DecisionKind::WorkingSet,
];

/// A row's name: the kind, plus `(start-up)` for #85's proposals, which
/// are the person's start-up question rather than the model's own.
fn row_name(row: &DecisionKindStats) -> String {
    if row.startup {
        format!("{} (start-up)", kind_name(row.kind))
    } else {
        kind_name(row.kind).to_owned()
    }
}

fn kind_name(kind: DecisionKind) -> &'static str {
    match kind {
        DecisionKind::Project => "project",
        DecisionKind::Job => "job",
        DecisionKind::Ticket => "ticket",
        DecisionKind::Knowledge => "knowledge",
        DecisionKind::Route => "route",
        DecisionKind::WorkingSet => "working_set",
    }
}

/// `Some(0)` when nobody answered at all, so the caller can print `-`
/// rather than a rate nobody earned.
fn percent(yes: u32, total: u32) -> Option<u32> {
    (total > 0).then(|| yes * 100 / total)
}

fn is_person(author: &Author) -> bool {
    matches!(author, Author::User(_))
}

fn percent_text(rate: Option<u32>) -> String {
    match rate {
        Some(rate) => format!("{rate}%"),
        None => "-".to_owned(),
    }
}

/// The report as the fixed-width text `stats` prints.
pub fn render_decisions(report: &DecisionReport) -> String {
    let mut out = String::new();
    match &report.since {
        Some(since) => out.push_str(&format!("proposals since {since} (with their answers)\n")),
        None => out.push_str("proposals: all threads (with their answers)\n"),
    }
    if report.kinds.is_empty() {
        out.push_str("no decisions recorded\n");
    } else {
        // The name column is as wide as the longest name printed, so
        // `project (start-up)` lines up with the rest (issue #85), and
        // never narrower than the 14 it always was, so a table of plain
        // rows reads as it did before (#85's review).
        let name_width = report
            .kinds
            .iter()
            .map(|k| row_name(k).chars().count())
            .chain(std::iter::once(MIN_NAME_WIDTH))
            .max()
            .unwrap_or(MIN_NAME_WIDTH);
        out.push_str(&format!(
            "\n{:<name_width$} {:>8} {:>5} {:>5} {:>10} {:>10} {:>7} {:>6} {:>8}\n",
            "kind", "proposed", "yes", "no", "corrected", "withdrawn", "pending", "rate", "last 30"
        ));
        for k in &report.kinds {
            out.push_str(&format!(
                "{:<name_width$} {:>8} {:>5} {:>5} {:>10} {:>10} {:>7} {:>6} {:>8}\n",
                row_name(k),
                k.proposed,
                k.yes,
                k.no,
                k.corrected,
                k.withdrawn,
                k.pending,
                percent_text(k.rate),
                percent_text(k.last_30)
            ));
        }
    }
    if report.orphans > 0 {
        out.push_str(&format!("\n{} orphan answers\n", report.orphans));
    }
    if report.unreadable > 0 {
        out.push_str(&format!("\n{} thread(s) unreadable\n", report.unreadable));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::threads_index::LEGACY_NONE_PROJECT;
    use std::path::PathBuf;

    use aigentic_runtime::aigentic_core::UserId;
    use aigentic_runtime::aigentic_log::{
        ProjectSwitchedPayload, RunStartedPayload, ThreadStartedPayload,
    };
    use serde_json::json;
    use time::macros::datetime;

    /// A config with the shapes the price book must handle: two priced
    /// profiles, one of which no call will name, and one without prices.
    const PRICED_CONFIG: &str = r#"
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

[profiles.priceless]
provider = "openai_compat"
base_url = "https://api.tensorx.ai/v1"
model = "some/other-model"
api_key_env = "TENSORX_API_KEY"
"#;

    /// No config, no prices: what `stats` did before #40, and still does
    /// when the config names none.
    fn no_prices() -> PriceBook {
        PriceBook::default()
    }

    fn book_of(text: &str) -> PriceBook {
        PriceBook::from_config(&Config::parse(text).unwrap(), None).unwrap()
    }

    /// What the config's table says a fixture line costs, recomputed in
    /// test from the same `[prices]` block: never a hand-computed literal.
    fn expected_cost(text: &str, profile: &str, line: &serde_json::Value) -> f64 {
        let config = Config::parse(text).unwrap();
        let (_, p) = config.select(Some(profile)).unwrap();
        let prices = p.prices.as_ref().unwrap().prices();
        let usage: Usage = serde_json::from_value(line["payload"]["usage"].clone()).unwrap();
        prices.cost_usd(&usage)
    }

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
        profile: Option<&str>,
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
                    "profile": profile,
                }
            },
            "created_at": at,
        })
    }

    /// An `assistant_message` whose usage the runtime estimated from
    /// `count_tokens` rather than the provider: never priced, whatever
    /// table knows its model (issue #40).
    fn estimated_call(at: &str, input: u64, output: u64, model: &str) -> serde_json::Value {
        json!({
            "kind": "assistant_message",
            "author": {"kind": "agent", "id": "assistant"},
            "payload": {
                "blocks": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": 0,
                    "cache_write_tokens": 0,
                    "estimated": true,
                    "cost_usd": null,
                    "model": model,
                }
            },
            "created_at": at,
        })
    }

    /// A `turn_ended` line as the runtime writes one since issue #47:
    /// the turn measured itself.
    fn measured_turn_ended(
        at: &str,
        reason: &str,
        wall: u64,
        slept: u64,
        awaiting: u64,
        keep_awake: &str,
    ) -> serde_json::Value {
        json!({
            "kind": "turn_ended",
            "author": {"kind": "system"},
            "payload": {
                "reason": reason,
                "touched": [],
                "wall_secs": wall,
                "slept_secs": slept,
                "slept_awaiting_secs": awaiting,
                "keep_awake": keep_awake,
            },
            "created_at": at,
        })
    }

    /// A side job's line after a turn, in the minimal shape #47 needs:
    /// written after `turn_ended`, so a wall time derived from
    /// timestamps would count the time between them as turn time (#47,
    /// amendment 2 item 3). #46's two shapes are `stamped_extraction`
    /// and `payload_only_extraction` below; this one carries a model no
    /// table knows and no cost, so it stays unpriced.
    fn memory_extracted(at: &str) -> serde_json::Value {
        json!({
            "kind": "memory_extracted",
            "author": {"kind": "system"},
            "payload": {
                "through_seq": 1,
                "written": [],
                "model": "m",
                "usage": {"input_tokens": 1, "output_tokens": 1},
            },
            "created_at": at,
        })
    }

    /// A `memory_extracted` line as the runtime stamps one since #46:
    /// the usage names the model that ran the extraction, and `cost` is
    /// what its profile's table said — `None` on a line the stamp never
    /// reached.
    fn stamped_extraction(
        at: &str,
        model: &str,
        input: u64,
        cache_read: u64,
        output: u64,
        cost: Option<f64>,
    ) -> serde_json::Value {
        json!({
            "kind": "memory_extracted",
            "author": {"kind": "system"},
            "payload": {
                "through_seq": 1,
                "written": [],
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": cache_read,
                    "cache_write_tokens": 0,
                    "estimated": false,
                    "cost_usd": cost,
                    "model": model,
                },
            },
            "created_at": at,
        })
    }

    /// The older shape of the same line: the model on the payload only,
    /// as #18 wrote it, and no cost — what #40's retro pricing has to
    /// cover (issue #46).
    fn payload_only_extraction(
        at: &str,
        model: &str,
        input: u64,
        output: u64,
    ) -> serde_json::Value {
        json!({
            "kind": "memory_extracted",
            "author": {"kind": "system"},
            "payload": {
                "through_seq": 1,
                "written": [],
                "model": model,
                "usage": {"input_tokens": input, "output_tokens": output},
            },
            "created_at": at,
        })
    }

    /// A `thread_renamed` line as the runtime writes one since #49: the
    /// utility call that chose the title, stamped exactly like an
    /// extraction — its usage names the model, `cost` is what its table
    /// said, and the payload's `model` is the fallback for a line the
    /// stamp never reached.
    fn titled(
        at: &str,
        title: &str,
        model: &str,
        input: u64,
        cache_read: u64,
        output: u64,
        cost: Option<f64>,
    ) -> serde_json::Value {
        json!({
            "kind": "thread_renamed",
            "author": {"kind": "system"},
            "payload": {
                "title": title,
                "model": model,
                "usage": {
                    "input_tokens": input,
                    "output_tokens": output,
                    "cache_read_tokens": cache_read,
                    "cache_write_tokens": 0,
                    "estimated": false,
                    "cost_usd": cost,
                    "model": model,
                },
            },
            "created_at": at,
        })
    }

    /// The usage a fixture side-job line carries, so a test's expected
    /// tokens are the fixture's own numbers.
    fn extraction_usage(line: &serde_json::Value) -> Usage {
        serde_json::from_value(line["payload"]["usage"].clone()).unwrap()
    }

    /// What the report's own table lookup says a fixture line costs: the
    /// `table(&usage)` step `classify_cost` takes, with the same
    /// payload-model fallback an extraction line written before the
    /// usage carried a model needs. Never a hand-computed number.
    fn expected_memory_cost(text: &str, line: &serde_json::Value) -> f64 {
        let book = book_of(text);
        let mut usage: Usage = serde_json::from_value(line["payload"]["usage"].clone()).unwrap();
        if usage.model.is_none() {
            usage.model = line["payload"]["model"].as_str().map(str::to_owned);
        }
        book.table(&usage)
            .unwrap_or_else(|| panic!("no table for {usage:?}"))
            .cost_usd(&usage)
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

    /// What the agent said before the user did: never a thread's title.
    fn agent_says(at: &str, text: &str) -> serde_json::Value {
        json!({
            "kind": "assistant_message",
            "author": {"kind": "agent", "id": "assistant"},
            "payload": {"blocks": [{"type": "text", "text": text}]},
            "created_at": at,
        })
    }

    fn evicted(at: &str) -> serde_json::Value {
        json!({
            "kind": "context_evicted",
            "author": {"kind": "system"},
            "payload": {"through_seq": 41},
            "created_at": at,
        })
    }

    fn tool_result(at: &str, is_error: bool) -> serde_json::Value {
        json!({
            "kind": "tool_result",
            "author": {"kind": "system"},
            "payload": {"id": "c1", "content": "boom", "is_error": is_error},
            "created_at": at,
        })
    }

    /// A slash command: the client loaded the skill the user typed, and
    /// its arguments are the thread's first message (#40).
    fn skill_loaded(at: &str, name: &str, invoked_by: &str) -> serde_json::Value {
        json!({
            "kind": "skill_loaded",
            "author": {"kind": "system"},
            "payload": {"name": name, "hash": "h", "source": "s", "body": "b", "invoked_by": invoked_by},
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
                None,
            ),
            call(
                "2026-09-22T09:00:02Z",
                200,
                800,
                20,
                Some(0.5),
                Some("gpt-4o"),
                None,
            ),
            retried("2026-09-22T09:00:03Z"),
            turn_ended("2026-09-22T09:00:04Z", "done"),
        ];
        let mut events = lines.clone();
        write_thread(&dir.path().join("alpha"), one, &lines);
        lines = vec![
            user("2026-09-23T10:00:00Z", "second thread"),
            renamed("2026-09-23T10:00:01Z", "a named thread"),
            call("2026-09-23T10:00:02Z", 50, 50, 5, None, None, None),
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
        let stats = collect(base, &[], None, None, &no_prices()).unwrap();

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
        let stats = collect(f.dir.path(), &[], None, Some(cutoff), &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), &[], None, Some(week), &no_prices()).unwrap();
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
    fn costliest_threads_rank_by_effective_spend_then_calls_with_titles() {
        let f = fixture();
        let stats = collect(f.dir.path(), &[], None, None, &no_prices()).unwrap();
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
    fn threads_rank_by_effective_spend_and_the_cell_shows_both() {
        let dir = tempfile::tempdir().unwrap();
        // A mixed thread (stamped + retro), a retro-only one, and a
        // stamped-only one whose stamp is the biggest single number.
        let mixed = Ulid::generate();
        let guessy = Ulid::generate();
        let stamped = Ulid::generate();
        let mixed_lines = vec![
            user("2026-09-26T09:00:00Z", "mixed"),
            call(
                "2026-09-26T09:00:01Z",
                10,
                0,
                10,
                Some(0.9),
                Some("z-ai/glm-5.3"),
                None,
            ),
            // 2M output tokens on the tensorx table: exactly 9 dollars.
            call(
                "2026-09-26T09:00:02Z",
                0,
                0,
                2_000_000,
                None,
                Some("z-ai/glm-5.3"),
                None,
            ),
        ];
        let guessy_lines = vec![
            user("2026-09-26T09:10:00Z", "guessy"),
            call(
                "2026-09-26T09:10:01Z",
                0,
                0,
                2_000_000,
                None,
                Some("unknown/model"),
                Some("flash"),
            ),
        ];
        let stamped_lines = vec![
            user("2026-09-26T09:20:00Z", "stamped"),
            call(
                "2026-09-26T09:20:01Z",
                10,
                0,
                10,
                Some(1.0),
                Some("z-ai/glm-5.3"),
                None,
            ),
        ];
        let base = dir.path().join("alpha");
        write_thread(&base, mixed, &mixed_lines);
        write_thread(&base, guessy, &guessy_lines);
        write_thread(&base, stamped, &stamped_lines);

        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let mixed_retro = expected_cost(PRICED_CONFIG, "tensorx", &mixed_lines[2]);
        let guessy_retro = expected_cost(PRICED_CONFIG, "flash", &guessy_lines[1]);
        assert!(
            (mixed_retro - 9.0).abs() < 1e-12 && (guessy_retro - 3.0).abs() < 1e-12,
            "the fixture's tables: {mixed_retro} {guessy_retro}"
        );

        let ids: Vec<&str> = stats.threads.iter().map(|t| t.id.as_str()).collect();
        let (m, g, s) = (mixed.to_string(), guessy.to_string(), stamped.to_string());
        assert_eq!(
            ids,
            vec![m.as_str(), g.as_str(), s.as_str()],
            "{:?}",
            stats.threads
        );
        // The effective spend is what the sort used: 9.9 beats the plain
        // $1.0000 stamp, and the retro-only thread still ranks.
        let cells: Vec<String> = stats
            .threads
            .iter()
            .map(|t| money(t.spent, t.price_estimated_spent))
            .collect();
        assert_eq!(
            cells,
            vec![
                format!("${:.4}+~${:.4}", 0.9, mixed_retro),
                format!("~${:.4}", guessy_retro),
                format!("${:.4}", 1.0),
            ],
            "{cells:?}"
        );
        let text = render(&stats);
        assert!(text.contains("costliest threads"), "{text}");
        for cell in &cells {
            assert!(text.contains(cell.as_str()), "{cell} missing from {text}");
        }
    }

    #[test]
    fn a_thread_takes_its_latest_rename_else_its_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("alpha");
        let twice = Ulid::generate();
        let plain = Ulid::generate();
        write_thread(
            &base,
            twice,
            &[
                user("2026-09-27T09:00:00Z", "the first line of a thread"),
                renamed("2026-09-27T09:00:01Z", "an earlier title"),
                call(
                    "2026-09-27T09:00:02Z",
                    10,
                    0,
                    10,
                    Some(0.1),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
                renamed("2026-09-27T09:00:03Z", "the later title"),
            ],
        );
        write_thread(
            &base,
            plain,
            &[
                user("2026-09-27T09:10:00Z", "no rename here\nsecond line"),
                call(
                    "2026-09-27T09:10:02Z",
                    10,
                    0,
                    10,
                    Some(0.2),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );

        // The agent spoke first: that is not the thread's opening line.
        let agent_first = Ulid::generate();
        write_thread(
            &base,
            agent_first,
            &[
                agent_says("2026-09-27T09:20:00Z", "a greeting from the agent"),
                user("2026-09-27T09:20:01Z", "what the user actually asked"),
                call(
                    "2026-09-27T09:20:02Z",
                    10,
                    0,
                    10,
                    Some(0.3),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );

        let stats = collect(dir.path(), &[], None, None, &no_prices()).unwrap();
        let title = |id: Ulid| {
            stats
                .threads
                .iter()
                .find(|t| t.id == id.to_string())
                .map(|t| t.title.clone())
                .unwrap_or_else(|| panic!("{id} missing from {:?}", stats.threads))
        };
        assert_eq!(title(twice), "the later title");
        assert_eq!(title(plain), "no rename here");
        assert_eq!(title(agent_first), "what the user actually asked");
    }

    /// A log with everything a single-thread report reads: two calls
    /// (one stamped, one retro-priced), a sweep, an errored tool result,
    /// two retries, and two turn reasons under a shared head.
    fn single_thread_fixture() -> (tempfile::TempDir, Ulid, Vec<serde_json::Value>) {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T12:00:00Z", "one thread, everything in it"),
            retried("2026-09-27T12:00:01Z"),
            call(
                "2026-09-27T12:00:02Z",
                100,
                40,
                20,
                Some(0.5),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            evicted("2026-09-27T12:00:03Z"),
            tool_result("2026-09-27T12:00:04Z", true),
            tool_result("2026-09-27T12:00:05Z", false),
            call(
                "2026-09-27T12:00:06Z",
                300,
                60,
                400_000,
                None,
                Some("unknown/model"),
                Some("flash"),
            ),
            retried("2026-09-27T12:00:07Z"),
            turn_ended("2026-09-27T12:00:08Z", "done"),
            turn_ended("2026-09-27T12:00:09Z", "provider_error: transport timeout"),
            turn_ended("2026-09-27T12:00:10Z", "provider_error: http 503"),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        (dir, id, lines)
    }

    #[test]
    fn one_thread_reads_back_every_field_from_its_lines() {
        let (dir, id, lines) = single_thread_fixture();
        let book = book_of(PRICED_CONFIG);
        let report = collect_thread(dir.path(), &[], None, id, None, &book).unwrap();

        assert_eq!(report.id, id.to_string());
        assert_eq!(report.project, "alpha");
        assert_eq!(report.title, "one thread, everything in it");
        // The last usage line to carry a profile is the retro-priced one.
        assert_eq!(report.profile, "flash");

        assert_eq!(report.calls, 2);
        assert_eq!(report.priced_calls, 1);
        assert_eq!(report.price_estimated_calls, 1);
        assert_eq!(report.unpriced_calls, 0);
        assert_eq!(report.unstamped_calls, 0);
        assert_eq!(report.spent, Some(0.5));
        assert_eq!(
            report.price_estimated_spent,
            Some(expected_cost(PRICED_CONFIG, "flash", &lines[6]))
        );
        // Context, peak, mean, cache and hit rate, the `reports.rs`
        // formula, recomputed here from the lines.
        // The fixture's two calls are lines 2 and 6; the formula is the
        // `reports.rs` one, applied to their own usage fields.
        let usage = |i: usize| &lines[i]["payload"]["usage"];
        let contexts: Vec<u64> = [2, 6]
            .iter()
            .map(|i| {
                ["input_tokens", "cache_read_tokens", "cache_write_tokens"]
                    .iter()
                    .map(|k| usage(*i)[*k].as_u64().unwrap())
                    .sum()
            })
            .collect();
        let cache_read: u64 = [2, 6]
            .iter()
            .map(|i| usage(*i)["cache_read_tokens"].as_u64().unwrap())
            .sum();
        assert_eq!(report.context_total, contexts.iter().sum::<u64>());
        assert_eq!(report.peak_context, contexts.iter().copied().max().unwrap());
        assert_eq!(report.mean_context, contexts.iter().sum::<u64>() / 2);
        assert_eq!(report.cache_read, cache_read);
        assert!(
            (report.hit_rate - cache_read as f64 / contexts.iter().sum::<u64>() as f64).abs()
                < 1e-12
        );

        assert_eq!(report.sweeps, 1);
        assert_eq!(report.tool_errors, 1, "only the is_error result counts");
        assert_eq!(report.retries, 2);
        assert_eq!(
            report.turns,
            BTreeMap::from([("done".to_owned(), 1), ("provider_error".to_owned(), 2)])
        );

        let text = render_thread(&report);
        assert!(text.contains(&id.to_string()), "{text}");
        assert!(text.contains("provider_error 2"), "{text}");
        assert!(text.contains("sweeps 1"), "{text}");

        // The window gates the report too: a cutoff past the thread
        // leaves its labels but no calls.
        let cutoff = Some(datetime!(2026-09-28 00:00:00 UTC));
        let after = collect_thread(dir.path(), &[], None, id, cutoff, &book).unwrap();
        assert_eq!(after.calls, 0);
        assert_eq!(after.sweeps, 0);
        assert_eq!(after.tool_errors, 0);
        assert_eq!(after.title, "one thread, everything in it");

        // An id no project holds is an error naming it, never an empty
        // report.
        let missing = Ulid::generate();
        let err = collect_thread(dir.path(), &[], None, missing, None, &book)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&missing.to_string()), "{err}");

        // `--project` narrows the search: the same id is not in `beta`.
        assert!(collect_thread(dir.path(), &[], Some("beta"), id, None, &book).is_err());
        assert!(collect_thread(dir.path(), &[], Some("alpha"), id, None, &book).is_ok());
    }

    /// T15 (issue #47): a thread whose turns measured themselves reports
    /// the wall time and the sleep their lines carry; one whose turns
    /// predate the field prints no `time` line at all, so an old log
    /// renders as it always did. The expected minutes come from the
    /// fixture's own logged seconds, not from a hand-computed number,
    /// and the hour the fixture idles between turns plus the side job
    /// after a turn are not in the total (amendment 2 item 3).
    #[test]
    fn a_thread_reports_its_sleep_only_when_a_turn_measured_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("alpha");
        let book = no_prices();

        // Two measured turns with a `memory_extracted` side job between
        // the first and a 41-minute idle gap before the second: the log
        // spans far more than the turns did, and stats must say so.
        let (first_wall, second_wall, slept) = (750u64, 240u64, 750u64);
        let span_secs = 4370u64; // 11:27:30 to 12:40:20, the fixture's own gap
        let measured = Ulid::generate();
        write_thread(
            &base,
            measured,
            &[
                user("2026-09-27T11:27:30Z", "slept here"),
                measured_turn_ended("2026-09-27T11:28:00Z", "done", first_wall, slept, 0, "on"),
                memory_extracted("2026-09-27T11:28:10Z"),
                user("2026-09-27T12:40:00Z", "and again"),
                measured_turn_ended("2026-09-27T12:40:20Z", "done", second_wall, 0, 0, "on"),
            ],
        );
        let report = collect_thread(dir.path(), &[], None, measured, None, &book).unwrap();
        let wall = first_wall + second_wall;
        assert_eq!(report.wall_secs, Some(wall));
        assert_eq!(report.slept_secs, Some(slept));
        assert_ne!(
            report.wall_secs,
            Some(span_secs),
            "idle time between turns is not turn time"
        );
        let text = render_thread(&report);
        assert!(
            text.contains(&format!(
                "time       wall {} min   slept {} min",
                wall / 60,
                slept / 60
            )),
            "{text}"
        );

        // A thread from before the field existed: no line, old render.
        let old = Ulid::generate();
        write_thread(
            &base,
            old,
            &[
                user("2026-09-27T11:50:00Z", "no idea"),
                turn_ended("2026-09-27T12:00:00Z", "done"),
            ],
        );
        let report = collect_thread(dir.path(), &[], None, old, None, &book).unwrap();
        assert_eq!(report.wall_secs, None);
        assert_eq!(report.slept_secs, None);
        let text = render_thread(&report);
        assert!(!text.contains("\ntime "), "no time line at all: {text}");

        // A measured turn that slept nowhere: also no line, because a
        // "slept 0 min" row would be noise on every thread.
        let awake = Ulid::generate();
        write_thread(
            &base,
            awake,
            &[
                user("2026-09-27T12:10:00Z", "awake"),
                measured_turn_ended("2026-09-27T12:12:00Z", "done", 120, 0, 0, "on"),
            ],
        );
        let report = collect_thread(dir.path(), &[], None, awake, None, &book).unwrap();
        assert_eq!(report.wall_secs, Some(120));
        assert_eq!(report.slept_secs, Some(0));
        let text = render_thread(&report);
        assert!(
            !text.contains("\ntime "),
            "a nap-free thread is silent: {text}"
        );
    }

    /// One thread per match shape, each named so the assertion can point
    /// at the case that broke. Built here, matched by `collect_issue`.
    fn issue_fixture() -> (tempfile::TempDir, Ulid, Ulid, Ulid, Ulid, Ulid) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("alpha");
        let plain = Ulid::generate();
        let skill = Ulid::generate();
        let model_skill = Ulid::generate();
        let next_line = Ulid::generate();
        write_thread(
            &base,
            plain,
            &[
                user(
                    "2026-09-27T09:00:00Z",
                    "fix the stats windowing -- see #40 for the rule",
                ),
                call(
                    "2026-09-27T09:00:01Z",
                    10,
                    0,
                    10,
                    Some(0.25),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        write_thread(
            &base,
            skill,
            &[
                skill_loaded("2026-09-27T09:10:00Z", "brief", "user"),
                user("2026-09-27T09:10:01Z", "40 -- the stats drill-downs"),
                call(
                    "2026-09-27T09:10:02Z",
                    10,
                    0,
                    10,
                    Some(0.5),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        write_thread(
            &base,
            model_skill,
            &[
                skill_loaded("2026-09-27T09:20:00Z", "simplify", "model"),
                user("2026-09-27T09:20:01Z", "40 -- the model chose this skill"),
                call(
                    "2026-09-27T09:20:02Z",
                    10,
                    0,
                    10,
                    Some(1.0),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        write_thread(
            &base,
            next_line,
            &[
                user(
                    "2026-09-27T09:30:00Z",
                    "a long first line that runs past the display title and only mentions #40 here",
                ),
                call(
                    "2026-09-27T09:30:01Z",
                    10,
                    0,
                    10,
                    Some(2.0),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        // A bare `40` with no slash command before it: no `#`, no skill,
        // so nothing names the issue.
        let bare = Ulid::generate();
        write_thread(
            &base,
            bare,
            &[
                user("2026-09-27T09:40:00Z", "40 is just a number here"),
                call(
                    "2026-09-27T09:40:01Z",
                    10,
                    0,
                    10,
                    Some(4.0),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        (dir, plain, skill, model_skill, next_line, bare)
    }

    fn issue_run_started(at: &str, issue: u64) -> serde_json::Value {
        json!({
            "kind": "run_started",
            "author": {"kind": "agent", "id": "runner"},
            "payload": serde_json::to_value(RunStartedPayload {
                issue,
                workflow: "test".into(),
                version: 1,
                content_hash: "test".into(),
                budget_usd: 10.0,
            })
            .unwrap(),
            "created_at": at,
        })
    }

    fn issue_thread_started(at: &str, parent_thread: Option<Ulid>) -> serde_json::Value {
        json!({
            "kind": "thread_started",
            "author": {"kind": "agent", "id": "runner"},
            "payload": serde_json::to_value(ThreadStartedPayload {
                project: Some("alpha".into()),
                root: PathBuf::from("alpha"),
                created_by: Author::User(UserId("steve".into())),
                parent_thread,
                step: parent_thread.map(|_| "implement".into()),
                front: false,
            })
            .unwrap(),
            "created_at": at,
        })
    }

    #[test]
    fn an_issue_search_includes_run_leads_and_their_step_threads() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("alpha");
        let lead = id_at(1);
        let first_child = id_at(2);
        let second_child = id_at(3);
        let other_lead = id_at(4);
        let other_child = id_at(5);

        // The catalogue visits newer ULIDs first, so both children are
        // read before their lead. Association must not depend on order.
        write_thread(
            &base,
            lead,
            &[
                issue_thread_started("2026-09-27T09:00:00Z", None),
                issue_run_started("2026-09-27T09:00:01Z", 71),
                call(
                    "2026-09-27T09:00:02Z",
                    10,
                    0,
                    10,
                    Some(0.25),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        for (id, cost, at) in [
            (first_child, 0.5, "2026-09-27T09:10:00Z"),
            (second_child, 0.75, "2026-09-27T09:20:00Z"),
        ] {
            write_thread(
                &base,
                id,
                &[
                    issue_thread_started(at, Some(lead)),
                    call(at, 10, 0, 10, Some(cost), Some("z-ai/glm-5.3"), None),
                ],
            );
        }

        write_thread(
            &base,
            other_lead,
            &[
                issue_thread_started("2026-09-27T09:30:00Z", None),
                issue_run_started("2026-09-27T09:30:01Z", 72),
                call(
                    "2026-09-27T09:30:02Z",
                    10,
                    0,
                    10,
                    Some(4.0),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        write_thread(
            &base,
            other_child,
            &[
                issue_thread_started("2026-09-27T09:40:00Z", Some(other_lead)),
                call(
                    "2026-09-27T09:40:01Z",
                    10,
                    0,
                    10,
                    Some(8.0),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );

        let report = collect_issue(dir.path(), &[], None, 71, None, &no_prices()).unwrap();
        assert_eq!(report.total.threads, 3, "{:?}", report.threads);
        assert_eq!(report.total.calls, 3, "{:?}", report.threads);
        assert_eq!(report.total.spent, Some(1.5), "{:?}", report.threads);
        for (id, spent) in [(lead, 0.25), (first_child, 0.5), (second_child, 0.75)] {
            let thread = report
                .threads
                .iter()
                .find(|thread| thread.id == id.to_string())
                .unwrap_or_else(|| panic!("missing run thread {id}"));
            assert_eq!(thread.calls, 1);
            assert_eq!(thread.spent, Some(spent));
        }
        assert!(
            !report
                .threads
                .iter()
                .any(|thread| thread.id == other_lead.to_string()
                    || thread.id == other_child.to_string()),
            "another issue's run must stay out of the report: {:?}",
            report.threads
        );
    }

    #[test]
    fn an_issue_search_matches_the_first_message_and_never_a_substring() {
        let (dir, plain, skill, model_skill, past_title, bare) = issue_fixture();
        let book = no_prices();
        let ids = |report: &IssueReport| {
            report
                .threads
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>()
        };

        // (a) `#40` anywhere in the first message matches.
        let report = collect_issue(dir.path(), &[], None, 40, None, &book).unwrap();
        assert!(
            ids(&report).contains(&plain.to_string()),
            "{:?}",
            ids(&report)
        );
        // (b) a slash command's arguments are the first message.
        assert!(ids(&report).contains(&skill.to_string()));
        // A skill the *model* loaded says nothing, and a bare `40` with
        // no slash command before it is not a reference either.
        assert!(!ids(&report).contains(&model_skill.to_string()));
        assert!(
            !ids(&report).contains(&bare.to_string()),
            "{:?}",
            ids(&report)
        );
        // #40's first rule keeps the whole message, so a `#40` past the
        // display title's 72 characters still matches.
        assert!(ids(&report).contains(&past_title.to_string()));
        assert_eq!(report.issue, 40);
        assert_eq!(report.total.threads, 3);
        assert_eq!(report.total.calls, 3);
        // Dearest first: 2.0000, 0.5000, 0.2500.
        assert_eq!(report.total.spent, Some(2.75));
        let money_order: Vec<f64> = report.threads.iter().filter_map(|t| t.spent).collect();
        assert_eq!(money_order, vec![2.0, 0.5, 0.25], "{:?}", ids(&report));
        let text = render_issue(&report);
        assert!(text.contains("issue 40"), "{text}");
        assert!(text.contains("threads    3"), "{text}");
        assert!(text.contains("3 calls"), "{text}");

        // `#400` is not `#40` (the first reference wins, so the later
        // `#40x` is never read), and `4` is not `#40`.
        let dir2 = tempfile::tempdir().unwrap();
        let over = Ulid::generate();
        write_thread(
            &dir2.path().join("alpha"),
            over,
            &[
                user("2026-09-27T10:00:00Z", "see #400 and #40x"),
                call(
                    "2026-09-27T10:00:01Z",
                    10,
                    0,
                    10,
                    Some(0.1),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        assert!(
            collect_issue(dir2.path(), &[], None, 40, None, &book)
                .unwrap()
                .threads
                .is_empty()
        );
        // `#400` names 400, not 40 — the digits are one reference, so
        // the boundary rule can never split them into 4 and 00.
        assert_eq!(
            collect_issue(dir2.path(), &[], None, 400, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
        assert!(
            collect_issue(dir.path(), &[], None, 4, None, &book)
                .unwrap()
                .threads
                .is_empty(),
            "#40 must not match 4"
        );
    }

    #[test]
    fn an_issue_reference_takes_the_whole_digit_run_and_checks_only_before() {
        assert_eq!(hashed_issue_number("#40"), Some(40));
        assert_eq!(hashed_issue_number("see #40."), Some(40));
        assert_eq!(hashed_issue_number("#40's review"), Some(40));
        assert_eq!(hashed_issue_number("#40x"), Some(40));
        assert_eq!(hashed_issue_number("#400"), Some(400));
        assert_eq!(hashed_issue_number("a#40"), None);
        assert_eq!(hashed_issue_number("# 40, then #41"), Some(41));
        assert_eq!(hashed_issue_number("no reference"), None);
    }

    #[test]
    fn an_issue_search_takes_the_first_reference_not_a_later_one() {
        // The amendment's case: the prompt itself is about #40 and
        // mentions #44 later.
        let dir = tempfile::tempdir().unwrap();
        let first = Ulid::generate();
        let lines = vec![
            user("2026-09-27T11:00:00Z", "for #40, not like #44 did"),
            call(
                "2026-09-27T11:00:01Z",
                10,
                0,
                10,
                Some(0.1),
                Some("z-ai/glm-5.3"),
                None,
            ),
        ];
        write_thread(&dir.path().join("alpha"), first, &lines);
        let book = no_prices();
        assert_eq!(
            collect_issue(dir.path(), &[], None, 40, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
        assert!(
            collect_issue(dir.path(), &[], None, 44, None, &book)
                .unwrap()
                .threads
                .is_empty(),
            "the first reference is #40, so #44 must not match"
        );

        // A `#40` on the message's second line is still the first message.
        let dir2 = tempfile::tempdir().unwrap();
        let second = Ulid::generate();
        write_thread(
            &dir2.path().join("alpha"),
            second,
            &[
                user("2026-09-27T11:10:00Z", "the headline\nand #40 below it"),
                call(
                    "2026-09-27T11:10:01Z",
                    10,
                    0,
                    10,
                    Some(0.1),
                    Some("z-ai/glm-5.3"),
                    None,
                ),
            ],
        );
        assert_eq!(
            collect_issue(dir2.path(), &[], None, 40, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
    }

    #[test]
    fn json_round_trips_the_same_totals() {
        let f = fixture();
        let stats = collect(f.dir.path(), &[], None, None, &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), &[], Some("beta"), None, &no_prices()).unwrap();
        assert_eq!(stats.projects.len(), 1);
        assert_eq!(stats.projects[0].project, "beta");
        assert_eq!(stats.projects[0].calls, 1);
        // A project with no threads is an empty report, not an error.
        let none = collect(f.dir.path(), &[], Some("gamma"), None, &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), &[], None, None, &no_prices()).unwrap();
        assert_eq!(stats.unreadable, 1, "{stats:?}");
        // And the readable thread is still fully counted.
        assert_eq!(stats.projects.iter().map(|p| p.calls).sum::<u32>(), 3);
    }

    #[test]
    fn an_unpriced_call_is_retro_priced_from_its_model_table() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-24T09:00:00Z", "retro"),
            // Unpriced and model-stamped: the config's table prices it now.
            call(
                "2026-09-24T09:00:01Z",
                1000,
                2000,
                500,
                None,
                Some("z-ai/glm-5.3"),
                None,
            ),
            // Stamped: kept exactly, never recomputed.
            call(
                "2026-09-24T09:00:02Z",
                10,
                0,
                10,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                None,
            ),
            // Token-estimated: never priced, whatever table knows its model.
            estimated_call("2026-09-24T09:00:03Z", 100, 10, "z-ai/glm-5.3"),
            // Model unknown, profile known: the profile's table prices it.
            call(
                "2026-09-24T09:00:04Z",
                100,
                0,
                100,
                None,
                Some("unknown/model"),
                Some("flash"),
            ),
            // Model and profile both known: the model wins (#40's
            // amendment to the plan's original profile-first order).
            call(
                "2026-09-24T09:00:05Z",
                100,
                0,
                100,
                None,
                Some("z-ai/glm-5.3"),
                Some("flash"),
            ),
        ];
        let project = dir.path().join("alpha");
        write_thread(&project, id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        assert_eq!(day.calls, 5);
        assert_eq!(day.priced_calls, 1);
        assert_eq!(day.price_estimated_calls, 3);
        assert_eq!(day.unpriced_calls, 1);
        assert!(
            (day.spent.unwrap() - 0.42).abs() < 1e-12,
            "the stamp is kept: {day:?}"
        );
        let expected_model = expected_cost(PRICED_CONFIG, "tensorx", &lines[1]);
        let expected_profile = expected_cost(PRICED_CONFIG, "flash", &lines[4]);
        let expected_wins = expected_cost(PRICED_CONFIG, "tensorx", &lines[5]);
        assert!(
            (expected_wins - expected_profile).abs() > 1e-12,
            "the two tables must differ for the model-first order to be visible"
        );
        let retro = expected_model + expected_profile + expected_wins;
        assert!(
            (day.price_estimated_spent.unwrap() - retro).abs() < 1e-12,
            "expected ~{retro}: {day:?}"
        );

        // The report separates measured dollars from guessed ones, and the
        // project and day rows agree.
        let text = render(&stats);
        assert!(
            text.contains(&format!("${:.4}+~${:.4}", 0.42, retro)),
            "{text}"
        );
        assert!(
            text.contains("1 priced, 3 estimated, 1 unpriced of 5 calls"),
            "{text}"
        );
        assert_eq!(
            stats
                .projects
                .iter()
                .map(|p| p.price_estimated_calls)
                .sum::<u32>(),
            day.price_estimated_calls
        );

        // No table at all: nothing is guessed, and nothing is claimed.
        let none = collect(dir.path(), &[], None, None, &no_prices()).unwrap();
        assert_eq!(none.days[0].price_estimated_calls, 0, "{:?}", none.days[0]);
        assert!(none.days[0].price_estimated_spent.is_none());
        assert_eq!(none.days[0].priced_calls, 1);
    }

    #[test]
    fn unstamped_calls_need_assume_profile() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        // Neither model nor profile: a line written before #31 stamped them.
        let lines = vec![
            user("2026-09-25T09:00:00Z", "old line"),
            call("2026-09-25T09:00:01Z", 1000, 0, 500, None, None, None),
        ];
        let project = dir.path().join("alpha");
        write_thread(&project, id, &lines);
        let config = Config::parse(PRICED_CONFIG).unwrap();

        // Without the flag: unpriced, and said to predate model stamping.
        let stats = collect(
            dir.path(),
            &[],
            None,
            None,
            &PriceBook::from_config(&config, None).unwrap(),
        )
        .unwrap();
        assert_eq!(stats.days[0].unpriced_calls, 1);
        assert_eq!(stats.days[0].unstamped_calls, 1);
        assert_eq!(stats.days[0].price_estimated_calls, 0);
        assert!(
            render(&stats).contains("1 calls predate model stamping"),
            "{}",
            render(&stats)
        );

        // With it: the named profile's table prices it, flagged estimated,
        // and the unstamped line goes away.
        let book = PriceBook::from_config(&config, Some("tensorx")).unwrap();
        let stats = collect(dir.path(), &[], None, None, &book).unwrap();
        let expected = expected_cost(PRICED_CONFIG, "tensorx", &lines[1]);
        assert_eq!(stats.days[0].price_estimated_calls, 1);
        assert_eq!(stats.days[0].unstamped_calls, 0);
        assert_eq!(stats.days[0].unpriced_calls, 0);
        assert!(
            (stats.days[0].price_estimated_spent.unwrap() - expected).abs() < 1e-12,
            "{:?}",
            stats.days[0]
        );
        let text = render(&stats);
        assert!(text.contains(&format!("~${expected:.4}")), "{text}");
        assert!(!text.contains("predate model stamping"), "{text}");

        // A name that is not a profile, and a profile without prices, each
        // say which name is the problem.
        let err = PriceBook::from_config(&config, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nope"), "{err}");
        let err = PriceBook::from_config(&config, Some("priceless"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("priceless") && err.contains("prices"), "{err}");
    }

    /// T5 (issue #46, the issue's own ask): one turn call and one stamped
    /// extraction. The call keeps its arithmetic — its dollars, its
    /// context, its hit rate — and the extraction lands in the memory
    /// counters beside it, never inside them.
    #[test]
    fn a_stamped_extraction_is_counted_beside_the_turn_calls() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T09:00:00Z", "one turn and its side job"),
            call(
                "2026-09-28T09:00:01Z",
                1000,
                200,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            stamped_extraction(
                "2026-09-28T09:00:02Z",
                "deepseek/deepseek-v4.1-flash",
                500,
                100,
                50,
                Some(0.03),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        assert_eq!(day.calls, 1);
        assert_eq!(day.spent, Some(0.42), "{day:?}");
        // 1000 input + 200 cache read, the call's own usage: the side
        // job's 600 tokens are not in the context total.
        assert_eq!(day.context_total, 1200);
        assert_eq!(day.cache_read, 200);
        assert_eq!(
            day.cache_read as f64 / day.context_total as f64,
            day.hit_rate
        );

        assert_eq!(day.job_calls, 1);
        assert_eq!(day.job_priced_calls, 1);
        assert_eq!(day.job_price_estimated_calls, 0);
        assert_eq!(day.job_unpriced_calls, 0);
        assert_eq!(day.job_spent, Some(0.03));
        assert_eq!(day.job_context, 600);
        assert_eq!(day.job_output, 50);
        // The thread row is the one that shows a single figure, so it is
        // the one that carries both (the plan's amendment, item 5).
        assert_eq!(
            stats.threads[0].spent,
            Some(0.42 + 0.03),
            "{:?}",
            stats.threads
        );
        assert_eq!(stats.threads[0].calls, 1);
    }

    /// T6 (issue #46): a line written before the stamp existed is priced
    /// by the model the payload names (#40's retro path), and a model no
    /// table knows stays unpriced with no dollars invented for it.
    #[test]
    fn an_old_extraction_is_retro_priced_by_its_payload_model() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T10:00:00Z", "old extraction lines"),
            // The payload names a model `PRICED_CONFIG` knows.
            payload_only_extraction("2026-09-28T10:00:01Z", "z-ai/glm-5.3", 2000, 300),
            // A model no table knows: nothing to price it with.
            payload_only_extraction("2026-09-28T10:00:02Z", "some/other-model", 900, 90),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        let retro = expected_memory_cost(PRICED_CONFIG, &lines[1]);
        assert!(retro > 0.0, "{retro}");
        assert_eq!(day.job_calls, 2);
        assert_eq!(day.job_price_estimated_calls, 1);
        assert_eq!(day.job_priced_calls, 0);
        assert_eq!(day.job_unpriced_calls, 1);
        assert_eq!(day.job_spent, None, "nothing was stamped");
        assert_eq!(day.job_price_estimated_spent, Some(retro), "{day:?}");
        let context: u64 = lines[1..3]
            .iter()
            .map(|l| {
                let u = extraction_usage(l);
                u.input_tokens + u.cache_read_tokens
            })
            .sum();
        let output: u64 = lines[1..3]
            .iter()
            .map(|l| extraction_usage(l).output_tokens)
            .sum();
        assert_eq!(day.job_context, context);
        assert_eq!(day.job_output, output);
        // The guessed dollar is dressed as guessed, never as measured.
        let text = render(&stats);
        assert!(
            text.contains(&format!("side jobs  ~${retro:.4} ")),
            "{text}"
        );
        assert_eq!(stats.threads[0].spent, None);
        assert_eq!(stats.threads[0].price_estimated_spent, Some(retro));
    }

    /// T7 (issue #46): the two lines a window with extractions adds. The
    /// measured and the guessed dollars keep #40's `$a + ~$b` split, and
    /// the total is the two halves added by class.
    #[test]
    fn render_shows_the_memory_line_and_the_total_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T11:00:00Z", "stamped and guessed side jobs"),
            call(
                "2026-09-28T11:00:01Z",
                1000,
                0,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            stamped_extraction(
                "2026-09-28T11:00:02Z",
                "deepseek/deepseek-v4.1-flash",
                500,
                100,
                50,
                Some(0.03),
            ),
            payload_only_extraction("2026-09-28T11:00:03Z", "z-ai/glm-5.3", 800, 100),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let retro = expected_memory_cost(PRICED_CONFIG, &lines[3]);
        let day = &stats.days[0];
        let side_jobs = [&lines[2], &lines[3]];
        let context: u64 = side_jobs
            .iter()
            .map(|l| {
                let u = extraction_usage(l);
                u.input_tokens + u.cache_read_tokens
            })
            .sum();
        let output: u64 = side_jobs
            .iter()
            .map(|l| extraction_usage(l).output_tokens)
            .sum();
        assert_eq!((day.job_context, day.job_output), (context, output));

        let text = render(&stats);
        assert!(
            text.contains(&format!(
                "side jobs  $0.0300 + ~${retro:.4} \
                 (1 priced, 1 estimated, 0 unpriced; 2 extractions, in {context} out {output})"
            )),
            "{text}"
        );
        // The calls' line is untouched: one call, its own stamp.
        assert!(
            text.contains("1 priced, 0 estimated, 0 unpriced of 1 calls"),
            "{text}"
        );
        // The total adds within each class, once.
        assert!(
            text.contains(&format!(
                "total      ${:.4} + ~${retro:.4} (1 calls + 2 side jobs)",
                0.42 + 0.03
            )),
            "{text}"
        );

        // A window with no side jobs at all: byte-identical to before
        // #46 — no `side jobs` line, no `total` line, no `jobs` column.
        let plain = tempfile::tempdir().unwrap();
        let plain_id = Ulid::generate();
        write_thread(
            &plain.path().join("alpha"),
            plain_id,
            &[
                user("2026-09-28T11:10:00Z", "a plain window"),
                lines[1].clone(),
            ],
        );
        let none = collect(plain.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let text = render(&none);
        assert!(!text.contains("side jobs"), "{text}");
        assert!(!text.contains("total "), "{text}");
        assert!(!text.contains("jobs"), "{text}");
    }

    /// T8 (issue #46): the day and project rows show the side jobs'
    /// dollars in a column of their own, and the costliest-threads table
    /// ranks a thread with only side jobs above a cheaper turn-only one,
    /// because a row's figure is the row's whole spend.
    #[test]
    fn the_tables_show_side_job_dollars_and_rank_by_whole_spend() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("alpha");
        let turn_only = Ulid::generate();
        let side_job_only = Ulid::generate();
        write_thread(
            &base,
            turn_only,
            &[
                user("2026-09-28T12:00:00Z", "a cheap turn"),
                call(
                    "2026-09-28T12:00:01Z",
                    100,
                    0,
                    10,
                    Some(0.05),
                    Some("z-ai/glm-5.3"),
                    Some("tensorx"),
                ),
            ],
        );
        write_thread(
            &base,
            side_job_only,
            &[
                user("2026-09-28T12:01:00Z", "only a side job"),
                stamped_extraction(
                    "2026-09-28T12:01:01Z",
                    "deepseek/deepseek-v4.1-flash",
                    100,
                    0,
                    10,
                    Some(0.30),
                ),
            ],
        );
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();

        // Dearest first: the side-job-only thread's 0.30 beats the
        // turn's 0.05, and its `calls` still says 0.
        assert_eq!(stats.threads[0].id, side_job_only.to_string());
        assert_eq!(stats.threads[0].calls, 0);
        assert_eq!(stats.threads[0].spent, Some(0.30));
        assert_eq!(stats.threads[1].id, turn_only.to_string());
        assert_eq!(stats.threads[1].spent, Some(0.05));

        // Both rows carry the call's dollars and the side job's, apart.
        let day = &stats.days[0];
        assert_eq!(day.spent, Some(0.05), "{day:?}");
        assert_eq!(day.job_spent, Some(0.30));
        let project = &stats.projects[0];
        assert_eq!((project.spent, project.job_spent), (Some(0.05), Some(0.30)));

        let text = render(&stats);
        assert!(text.contains("jobs"), "{text}");
        let row = text
            .lines()
            .find(|l| l.starts_with(&day.day))
            .unwrap_or_else(|| panic!("no day row in {text}"));
        assert!(
            row.contains(&money(day.spent, day.price_estimated_spent)),
            "{row}"
        );
        assert!(
            row.contains(&money(day.job_spent, day.job_price_estimated_spent)),
            "{row}"
        );
    }

    /// T9 (issue #46): the drill-down and the issue report show the
    /// side jobs too — the issue's totals include the side jobs of the
    /// build it describes, which is the whole point of the issue.
    #[test]
    fn the_drill_down_and_the_issue_report_include_the_extractions() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T13:00:00Z", "fix for #46, the stats gap"),
            call(
                "2026-09-28T13:00:01Z",
                1000,
                0,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            stamped_extraction(
                "2026-09-28T13:00:02Z",
                "deepseek/deepseek-v4.1-flash",
                500,
                100,
                50,
                Some(0.03),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let book = book_of(PRICED_CONFIG);

        let report = collect_thread(dir.path(), &[], None, id, None, &book).unwrap();
        assert_eq!((report.calls, report.job_calls), (1, 1));
        assert_eq!(report.job_spent, Some(0.03));
        let u = extraction_usage(&lines[2]);
        let (context, output) = (u.input_tokens + u.cache_read_tokens, u.output_tokens);
        let text = render_thread(&report);
        assert!(
            text.contains(&format!(
                "side jobs  $0.0300 (1 priced, 0 estimated, 0 unpriced; 1 extraction, \
                 in {context} out {output})"
            )),
            "{text}"
        );

        let issue = collect_issue(dir.path(), &[], None, 46, None, &book).unwrap();
        assert_eq!(issue.total.calls, 1);
        assert_eq!(issue.total.job_calls, 1);
        assert_eq!(issue.total.job_spent, Some(0.03));
        assert_eq!(issue.total.job_context, context);
        let text = render_issue(&issue);
        assert!(
            text.contains(&format!(
                "side jobs  $0.0300 (1 priced, 0 estimated, 0 unpriced; 1 extraction, \
                 in {context} out {output})"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "total      ${:.4} (1 calls + 1 side jobs)",
                0.42 + 0.03
            )),
            "{text}"
        );
    }

    /// T10 (issue #46): `--since` gates the side jobs exactly as it
    /// gates the calls — one outside the window is not in any counter.
    #[test]
    fn an_extraction_outside_the_window_is_invisible() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-27T08:00:00Z", "before the window"),
            stamped_extraction(
                "2026-09-27T08:00:01Z",
                "deepseek/deepseek-v4.1-flash",
                900,
                0,
                90,
                Some(0.09),
            ),
            user("2026-09-28T08:00:00Z", "inside the window"),
            stamped_extraction(
                "2026-09-28T08:00:01Z",
                "deepseek/deepseek-v4.1-flash",
                100,
                0,
                10,
                Some(0.01),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let book = book_of(PRICED_CONFIG);
        let cutoff = parse_since("2026-09-28", datetime!(2026-09-28 12:00:00 UTC)).unwrap();
        let stats = collect(dir.path(), &[], None, Some(cutoff), &book).unwrap();

        assert_eq!(stats.days.iter().map(|d| d.job_calls).sum::<u32>(), 1);
        let inside = extraction_usage(&lines[3]);
        assert_eq!(
            stats.days.iter().filter_map(|d| d.job_spent).sum::<f64>(),
            0.01
        );
        assert_eq!(
            stats.days.iter().map(|d| d.job_context).sum::<u64>(),
            inside.input_tokens + inside.cache_read_tokens
        );
        // The whole window, extractions included, is one day.
        assert_eq!(stats.days.len(), 1, "{:?}", stats.days);
        assert!(
            !render(&stats).contains("0.0900"),
            "the old side job is nowhere: {}",
            render(&stats)
        );
    }

    /// T3 (issue #49): a stamped `thread_renamed` is a side job on the
    /// same line as an extraction. Its usage is the call's, so it never
    /// enters the day's calls, its context or the hit rate, and the
    /// thread row — the one figure — carries it.
    #[test]
    fn a_title_call_is_counted_on_the_side_jobs_line() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T14:00:00Z", "one turn and its title"),
            call(
                "2026-09-28T14:00:01Z",
                1000,
                200,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            titled(
                "2026-09-28T14:00:02Z",
                "Deps check",
                "deepseek/deepseek-v4.1-flash",
                500,
                100,
                50,
                Some(0.03),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        // The call's own arithmetic, untouched by the title call.
        assert_eq!(day.calls, 1);
        assert_eq!(day.spent, Some(0.42));
        assert_eq!(day.context_total, 1200);
        assert_eq!(day.cache_read, 200);
        assert_eq!(
            day.cache_read as f64 / day.context_total as f64,
            day.hit_rate
        );

        // The title call is a side job, and the split says which part.
        assert_eq!(day.job_calls, 1);
        assert_eq!((day.extractions, day.titles), (0, 1));
        assert_eq!(day.job_priced_calls, 1);
        assert_eq!(day.job_spent, Some(0.03));
        let u = extraction_usage(&lines[2]);
        assert_eq!(day.job_context, u.input_tokens + u.cache_read_tokens);
        assert_eq!(day.job_output, u.output_tokens);
        assert_eq!(
            stats.threads[0].spent,
            Some(0.42 + 0.03),
            "{:?}",
            stats.threads
        );
        assert_eq!(stats.threads[0].calls, 1);
        assert_eq!(stats.threads[0].title, "Deps check");

        let (context, output) = (u.input_tokens + u.cache_read_tokens, u.output_tokens);
        let text = render(&stats);
        assert!(
            text.contains(&format!(
                "side jobs  $0.0300 (1 priced, 0 estimated, 0 unpriced; 1 title, \
                 in {context} out {output})"
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "total      ${:.4} (1 calls + 1 side jobs)",
                0.42 + 0.03
            )),
            "{text}"
        );
    }

    /// T4b (issue #49): the line names its parts when both kinds are
    /// there, and the counts and dollars add by kind.
    #[test]
    fn the_side_jobs_line_names_both_parts() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T15:00:00Z", "a build and a title"),
            call(
                "2026-09-28T15:00:01Z",
                1000,
                0,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            stamped_extraction(
                "2026-09-28T15:00:02Z",
                "deepseek/deepseek-v4.1-flash",
                500,
                100,
                50,
                Some(0.03),
            ),
            titled(
                "2026-09-28T15:00:03Z",
                "Deps check",
                "deepseek/deepseek-v4.1-flash",
                300,
                0,
                20,
                Some(0.01),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];
        assert_eq!(day.job_calls, 2);
        assert_eq!((day.extractions, day.titles), (1, 1));
        assert_eq!(day.job_spent, Some(0.04));

        let (context, output) = (900, 70);
        let text = render(&stats);
        assert!(
            text.contains(&format!(
                "side jobs  $0.0400 (2 priced, 0 estimated, 0 unpriced; \
                 1 extraction, 1 title, in {context} out {output})"
            )),
            "{text}"
        );
    }

    /// T5 (issue #49): a title line with a model but no cost — what #40's
    /// retro path and a stamp that never reached the line look like — is
    /// priced by the table that knows the model and lands in the guessed
    /// class, never added to a measured dollar.
    #[test]
    fn an_unstamped_title_line_is_retro_priced_by_its_payload_model() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T16:00:00Z", "a title nobody stamped"),
            titled(
                "2026-09-28T16:00:01Z",
                "Deps check",
                "deepseek/deepseek-v4.1-flash",
                400,
                100,
                40,
                None,
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        let retro = expected_memory_cost(PRICED_CONFIG, &lines[1]);
        assert!(retro > 0.0, "{retro}");
        assert_eq!((day.job_calls, day.titles), (1, 1));
        assert_eq!(day.job_priced_calls, 0);
        assert_eq!(day.job_price_estimated_calls, 1);
        assert_eq!(day.job_spent, None, "nothing was stamped");
        assert_eq!(day.job_price_estimated_spent, Some(retro), "{day:?}");
        // The guessed dollar is dressed as guessed, never as measured.
        let text = render(&stats);
        assert!(
            text.contains(&format!("side jobs  ~${retro:.4} (0 priced, 1 estimated, ")),
            "{text}"
        );
        assert_eq!(stats.threads[0].spent, None);
        assert_eq!(stats.threads[0].price_estimated_spent, Some(retro));
    }

    /// T6 (issue #49): a `thread_renamed` with no usage is no call at
    /// all. Every line written before #49 is in that shape, and so is a
    /// person's `/rename`, which makes no call — an old title is never
    /// priced after the fact and never counted as unpriced work. The
    /// title still shows.
    #[test]
    fn an_old_title_line_is_no_side_job_at_all() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T17:00:00Z", "a plain window"),
            call(
                "2026-09-28T17:00:01Z",
                1000,
                0,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            renamed("2026-09-28T17:00:02Z", "An old title"),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let stats = collect(dir.path(), &[], None, None, &book_of(PRICED_CONFIG)).unwrap();
        let day = &stats.days[0];

        assert_eq!(day.job_calls, 0);
        assert_eq!((day.extractions, day.titles), (0, 0));
        assert_eq!((day.job_spent, day.job_price_estimated_spent), (None, None));
        assert_eq!(day.job_context, 0);
        assert_eq!(stats.threads[0].title, "An old title");

        // No line, no total, no column: byte-identical to before #46 and
        // #49.
        let text = render(&stats);
        assert!(!text.contains("side jobs"), "{text}");
        assert!(!text.contains("jobs"), "{text}");
        assert!(!text.contains("total "), "{text}");
        assert_eq!(stats.threads[0].spent, Some(0.42));
    }

    /// T7 (issue #49): `stats --issue` counts a title call in the
    /// issue's side figures, because the issue's totals are what the
    /// build cost — the whole point of the issue.
    #[test]
    fn an_issue_report_counts_a_title_call() {
        let dir = tempfile::tempdir().unwrap();
        let id = Ulid::generate();
        let lines = vec![
            user("2026-09-28T18:00:00Z", "fix for #49, the last side job"),
            call(
                "2026-09-28T18:00:01Z",
                1000,
                0,
                100,
                Some(0.42),
                Some("z-ai/glm-5.3"),
                Some("tensorx"),
            ),
            titled(
                "2026-09-28T18:00:02Z",
                "Deps check",
                "deepseek/deepseek-v4.1-flash",
                300,
                0,
                20,
                Some(0.01),
            ),
        ];
        write_thread(&dir.path().join("alpha"), id, &lines);
        let book = book_of(PRICED_CONFIG);

        let issue = collect_issue(dir.path(), &[], None, 49, None, &book).unwrap();
        assert_eq!(issue.total.calls, 1);
        assert_eq!(issue.total.job_calls, 1);
        assert_eq!((issue.total.extractions, issue.total.titles), (0, 1));
        assert_eq!(issue.total.job_spent, Some(0.01));
        let u = extraction_usage(&lines[2]);
        assert_eq!(
            issue.total.job_context,
            u.input_tokens + u.cache_read_tokens
        );
        let text = render_issue(&issue);
        assert!(text.contains("1 title"), "{text}");
        assert!(
            text.contains(&format!(
                "total      ${:.4} (1 calls + 1 side jobs)",
                0.42 + 0.01
            )),
            "{text}"
        );
    }

    // ---- decisions (issue #74) ----

    /// A person, and the system: only a person's answers count in `rate`
    /// and `last 30`.
    const PERSON: &str = "steve";
    const SYS: &str = "system";

    /// One fixture decision: the kind, the proposal's time, and how (and
    /// by whom) it was answered. The lines are written from the table and
    /// the expected figures are computed from it, so no number in an
    /// assertion is hand-written.
    #[derive(Clone, Copy)]
    struct DecisionLine {
        kind: &'static str,
        at: &'static str,
        answer: Option<(&'static str, &'static str)>,
    }

    fn proposed(id: Ulid, at: &str, kind: &str, proposal: &str) -> serde_json::Value {
        json!({
            "id": id.to_string(),
            "kind": "decision_proposed",
            "author": {"kind": "agent", "id": "assistant"},
            "payload": {"kind": kind, "proposal": proposal, "reason": "the fixture says so"},
            "created_at": at,
        })
    }

    /// #85's start-up proposal: the same line with a `startup-` call id,
    /// which is what the fold reads as `startup: true`.
    fn proposed_startup(id: Ulid, at: &str, kind: &str, proposal: &str) -> serde_json::Value {
        let mut line = proposed(id, at, kind, proposal);
        let prefix = aigentic_runtime::aigentic_log::STARTUP_PREFIX;
        line["payload"]["call_id"] = json!(format!("{prefix}{}", Ulid::generate()));
        line
    }

    /// Like `decision_thread`, but every proposal is a start-up one
    /// (issue #85), so the report gives it its own row.
    fn startup_thread(project: &Path, table: &[DecisionLine]) {
        let mut lines = Vec::new();
        for (n, entry) in table.iter().enumerate() {
            let proposal = Ulid::generate();
            lines.push(proposed_startup(
                proposal,
                entry.at,
                entry.kind,
                &format!("proposal {n}"),
            ));
            if let Some((answer, who)) = entry.answer {
                lines.push(decision_answered(
                    Ulid::generate(),
                    entry.at,
                    proposal,
                    answer,
                    who,
                ));
            }
        }
        write_decisions(project, Ulid::generate(), &lines);
    }

    fn decision_answered(
        id: Ulid,
        at: &str,
        parent: Ulid,
        answer: &str,
        who: &str,
    ) -> serde_json::Value {
        let author = if who == SYS {
            json!({"kind": "system"})
        } else {
            json!({"kind": "user", "id": who})
        };
        json!({
            "id": id.to_string(),
            "kind": "decision_answered",
            "author": author,
            "parent_event": parent.to_string(),
            "payload": {"answer": answer},
            "created_at": at,
        })
    }

    /// A log that keeps the ids it is given: `write_thread` stamps a fresh
    /// one on every line, but an answer has to name its proposal's id.
    fn write_decisions(dir: &Path, id: Ulid, lines: &[serde_json::Value]) {
        std::fs::create_dir_all(dir).unwrap();
        let text: String = lines
            .iter()
            .enumerate()
            .map(|(seq, line)| {
                let mut e = line.clone();
                // A proposal keeps the id its answer points at; any other
                // line gets one, since every event carries an id.
                if e["id"].is_null() {
                    e["id"] = json!(Ulid::generate().to_string());
                }
                e["thread_id"] = json!(id.to_string());
                e["seq"] = json!(seq);
                format!("{e}\n")
            })
            .collect();
        std::fs::write(dir.join(format!("{id}.jsonl")), text).unwrap();
    }

    /// Write a table of decisions into one thread under `project`.
    fn decision_thread(project: &Path, table: &[DecisionLine]) {
        let mut lines = Vec::new();
        for (n, entry) in table.iter().enumerate() {
            let proposal = Ulid::generate();
            lines.push(proposed(
                proposal,
                entry.at,
                entry.kind,
                &format!("proposal {n}"),
            ));
            if let Some((answer, who)) = entry.answer {
                lines.push(decision_answered(
                    Ulid::generate(),
                    entry.at,
                    proposal,
                    answer,
                    who,
                ));
            }
        }
        write_decisions(project, Ulid::generate(), &lines);
    }

    /// The figures a table implies for one kind, recomputed from the table
    /// rather than written down: proposals, the answers by what they said,
    /// pending, and the operator's own rate.
    fn expected_row(
        table: &[DecisionLine],
        kind: &str,
    ) -> (u32, u32, u32, u32, u32, u32, Option<u32>) {
        let of_kind: Vec<&DecisionLine> = table.iter().filter(|e| e.kind == kind).collect();
        let (mut yes, mut no, mut corrected, mut withdrawn, mut pending) = (0, 0, 0, 0, 0);
        for entry in &of_kind {
            match entry.answer {
                None => pending += 1,
                Some((answer, _)) => match answer {
                    "yes" => yes += 1,
                    "no" => no += 1,
                    "corrected" => corrected += 1,
                    "withdrawn" => withdrawn += 1,
                    other => panic!("the fixture named no answer: {other}"),
                },
            }
        }
        // The operator's own rate, over their own yes/no/corrected.
        let person: Vec<&DecisionLine> = of_kind
            .iter()
            .copied()
            .filter(|e| {
                e.answer.is_some_and(|(answer, who)| {
                    who != SYS && matches!(answer, "yes" | "no" | "corrected")
                })
            })
            .collect();
        let person_yes = person
            .iter()
            .filter(|e| e.answer.is_some_and(|(answer, _)| answer == "yes"))
            .count() as u32;
        let rate = (!person.is_empty()).then(|| person_yes * 100 / person.len() as u32);
        (
            of_kind.len() as u32,
            yes,
            no,
            corrected,
            withdrawn,
            pending,
            rate,
        )
    }

    /// T5 (issue #74): `stats --decisions` over two projects — kinds in
    /// declaration order, one kind nobody answered, a system `withdrawn`
    /// and a pending proposal.
    #[test]
    fn decisions_report_each_kind_in_declaration_order() {
        let dir = tempfile::tempdir().unwrap();
        let alpha = [
            DecisionLine {
                kind: "project",
                at: "2026-09-22T09:00:00Z",
                answer: Some(("yes", PERSON)),
            },
            DecisionLine {
                kind: "job",
                at: "2026-09-22T09:05:00Z",
                answer: Some(("no", PERSON)),
            },
            DecisionLine {
                kind: "project",
                at: "2026-09-23T10:00:00Z",
                answer: Some(("corrected", PERSON)),
            },
            DecisionLine {
                kind: "working_set",
                at: "2026-09-23T11:00:00Z",
                answer: Some(("withdrawn", SYS)),
            },
            DecisionLine {
                kind: "project",
                at: "2026-09-23T12:00:00Z",
                answer: None,
            },
        ];
        let beta = [
            DecisionLine {
                kind: "job",
                at: "2026-09-24T08:00:00Z",
                answer: Some(("yes", PERSON)),
            },
            DecisionLine {
                kind: "route",
                at: "2026-09-24T08:30:00Z",
                answer: Some(("withdrawn", SYS)),
            },
        ];
        decision_thread(&dir.path().join("alpha"), &alpha);
        decision_thread(&dir.path().join("beta"), &beta);
        let all: Vec<DecisionLine> = alpha.iter().chain(beta.iter()).copied().collect();

        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        let names: Vec<&str> = report.kinds.iter().map(|k| kind_name(k.kind)).collect();
        assert_eq!(
            names,
            vec!["project", "job", "route", "working_set"],
            "declaration order, and no row for a kind with no proposal"
        );
        for row in &report.kinds {
            let name = kind_name(row.kind);
            let (proposed, yes, no, corrected, withdrawn, pending, rate) = expected_row(&all, name);
            assert_eq!(
                (
                    row.proposed,
                    row.yes,
                    row.no,
                    row.corrected,
                    row.withdrawn,
                    row.pending
                ),
                (proposed, yes, no, corrected, withdrawn, pending),
                "{name}"
            );
            assert_eq!(row.rate, rate, "{name} rate");
            assert_eq!(row.last_30, rate, "{name} last 30: fewer than 30 answered");
        }
        assert_eq!((report.orphans, report.unreadable), (0, 0));

        let text = render_decisions(&report);
        assert!(
            text.contains("proposals: all threads (with their answers)"),
            "{text}"
        );
        for word in [
            "kind",
            "proposed",
            "yes",
            "corrected",
            "withdrawn",
            "pending",
            "last 30",
        ] {
            assert!(
                text.contains(word),
                "{word} missing from the header: {text}"
            );
        }
        // The kind nobody answered prints `-` for both ratios.
        let quiet = text
            .lines()
            .find(|l| l.starts_with("working_set"))
            .expect("a working_set row");
        assert_eq!(quiet.matches('-').count(), 2, "{text}");
        // A kind's own figure reaches its row, recomputed from the table.
        let (_, _, _, _, _, _, rate) = expected_row(&all, "project");
        let row = text.lines().find(|l| l.starts_with("project")).unwrap();
        assert!(row.contains(&format!("{}%", rate.unwrap())), "{text}");
    }

    /// T6 (issue #74): `last 30` takes the 30 most recent proposals a
    /// person answered, ordering two in the same second by the proposal's
    /// id, and the rate is truncated.
    #[test]
    fn decisions_last_30_takes_the_thirty_most_recent_a_person_answered() {
        let dir = tempfile::tempdir().unwrap();
        // Two proposals in the same second: the boundary between the 30
        // that count and those that do not is decided by the proposal's
        // id, so the `no` with the smaller id is the one left out.
        let mut ids: Vec<Ulid> = (0..2).map(|_| Ulid::generate()).collect();
        ids.sort();
        let (small, large) = (ids[0], ids[1]);
        let boundary = "2026-09-24T09:00:00Z";
        let mut lines = vec![
            proposed(small, boundary, "project", "not this one"),
            decision_answered(Ulid::generate(), boundary, small, "no", PERSON),
            proposed(large, boundary, "project", "this one"),
            decision_answered(Ulid::generate(), boundary, large, "yes", PERSON),
        ];
        // 29 later yes answers, so the 30 most recent are the 29 plus the
        // larger-id proposal above.
        let later = 29;
        for n in 0..later {
            let at = format!("2026-09-25T10:00:{n:02}Z");
            let proposal = Ulid::generate();
            lines.push(proposed(proposal, &at, "project", "later"));
            lines.push(decision_answered(
                Ulid::generate(),
                &at,
                proposal,
                "yes",
                PERSON,
            ));
        }
        // One more `no`, a second earlier and with the largest id of all:
        // time, not id, keeps it out of the 30.
        let older = Ulid::generate();
        lines.push(proposed(older, "2026-09-24T08:00:00Z", "project", "older"));
        lines.push(decision_answered(
            Ulid::generate(),
            "2026-09-24T08:00:00Z",
            older,
            "no",
            PERSON,
        ));
        write_decisions(&dir.path().join("alpha"), Ulid::generate(), &lines);

        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        let row = &report.kinds[0];
        assert_eq!(kind_name(row.kind), "project");
        let person_yes = later + 1;
        let person_answers = later + 2 + 1;
        assert_eq!(row.proposed, person_answers as u32);
        assert_eq!(
            row.rate,
            Some(person_yes as u32 * 100 / person_answers as u32),
            "truncated over every person answer in the window"
        );
        // The 30 that count are all yes: 29 later plus the larger id.
        let counted_yes = later + 1;
        assert_eq!(row.last_30, Some(counted_yes as u32 * 100 / 30));
    }

    /// T7 (issue #74): `--since` keeps proposals inside the window — an
    /// answer inside it to a proposal before it is left out — and the
    /// global `--project` narrows the walk.
    #[test]
    fn decisions_honour_since_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let before = Ulid::generate();
        let inside = Ulid::generate();
        let alpha = vec![
            proposed(before, "2026-09-21T23:00:00Z", "project", "before"),
            // The proposal is before the boundary; the answer is inside.
            decision_answered(
                Ulid::generate(),
                "2026-09-23T10:00:00Z",
                before,
                "yes",
                PERSON,
            ),
            proposed(inside, "2026-09-23T09:00:00Z", "job", "inside"),
            decision_answered(
                Ulid::generate(),
                "2026-09-23T09:00:01Z",
                inside,
                "no",
                PERSON,
            ),
        ];
        write_decisions(&dir.path().join("alpha"), Ulid::generate(), &alpha);
        let beta = vec![proposed(
            Ulid::generate(),
            "2026-09-23T09:30:00Z",
            "ticket",
            "beta",
        )];
        write_decisions(&dir.path().join("beta"), Ulid::generate(), &beta);
        let cutoff = datetime!(2026-09-23 00:00:00 UTC);

        let report = collect_decisions(dir.path(), &[], None, Some(cutoff)).unwrap();
        let names: Vec<&str> = report.kinds.iter().map(|k| kind_name(k.kind)).collect();
        assert_eq!(
            names,
            vec!["job", "ticket"],
            "the proposal before the window is out, its answer inside it does not pull it in"
        );

        let narrowed = collect_decisions(dir.path(), &[], Some("alpha"), Some(cutoff)).unwrap();
        let names: Vec<&str> = narrowed.kinds.iter().map(|k| kind_name(k.kind)).collect();
        assert_eq!(names, vec!["job"], "`--project` narrows the walk");

        // Without the window the proposal is back, answered: pairing is by
        // the whole thread, whatever the window says.
        let all = collect_decisions(dir.path(), &[], Some("alpha"), None).unwrap();
        let project = all
            .kinds
            .iter()
            .find(|k| k.kind == DecisionKind::Project)
            .unwrap();
        assert_eq!((project.proposed, project.yes, project.pending), (1, 1, 0));
    }

    /// T8 (issue #74): `--json` carries the table's figures, an orphan
    /// answer and an unreadable thread are counted, and with nothing to
    /// report the kind list is empty.
    #[test]
    fn decisions_json_matches_the_table_and_counts_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let one = Ulid::generate();
        let two = Ulid::generate();
        let lines = vec![
            proposed(one, "2026-09-24T09:00:00Z", "project", "one"),
            decision_answered(Ulid::generate(), "2026-09-24T09:00:01Z", one, "yes", PERSON),
            // An answer naming no proposal is an orphan.
            decision_answered(
                Ulid::generate(),
                "2026-09-24T09:05:00Z",
                Ulid::generate(),
                "yes",
                PERSON,
            ),
            proposed(two, "2026-09-24T09:10:00Z", "project", "two"),
            decision_answered(Ulid::generate(), "2026-09-24T09:10:01Z", two, "no", PERSON),
        ];
        let alpha = dir.path().join("alpha");
        write_decisions(&alpha, Ulid::generate(), &lines);
        // A thread whose file cannot be read is counted, not swallowed.
        std::fs::write(
            alpha.join(format!("{}.jsonl", Ulid::generate())),
            "not json\n",
        )
        .unwrap();

        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        assert_eq!((report.orphans, report.unreadable), (1, 1));
        let text = render_decisions(&report);
        assert!(text.contains("1 orphan answers"), "{text}");
        assert!(text.contains("1 thread(s) unreadable"), "{text}");

        // The fixture answers one proposal yes and one no: half,
        // truncated, recomputed from those two.
        let (fixture_yes, fixture_no) = (1u32, 1u32);
        let row = &report.kinds[0];
        assert_eq!((row.yes, row.no), (fixture_yes, fixture_no));
        assert_eq!(
            row.rate,
            Some(fixture_yes * 100 / (fixture_yes + fixture_no))
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["kinds"][0]["kind"], "project");
        assert_eq!(json["kinds"][0]["proposed"], row.proposed);
        assert_eq!(json["kinds"][0]["rate"], row.rate.unwrap());
        assert_eq!(json["kinds"][0]["last_30"], row.last_30.unwrap());
        assert_eq!(json["orphans"], 1);
        assert_eq!(json["unreadable"], 1);

        // A kind nobody answered carries no ratio key at all.
        let quiet = DecisionReport {
            kinds: vec![DecisionKindStats {
                kind: DecisionKind::Route,
                startup: false,
                proposed: 1,
                yes: 0,
                no: 0,
                corrected: 0,
                withdrawn: 0,
                pending: 1,
                rate: None,
                last_30: None,
            }],
            ..DecisionReport::default()
        };
        let quiet = serde_json::to_value(&quiet).unwrap();
        let quiet = quiet["kinds"][0].as_object().unwrap();
        assert!(!quiet.contains_key("rate"), "{quiet:?}");
        assert!(!quiet.contains_key("last_30"), "{quiet:?}");

        // With no decisions, an empty kind list with zero counts.
        let empty = serde_json::to_value(DecisionReport::default()).unwrap();
        assert_eq!(empty["kinds"], json!([]));
        assert_eq!(empty["orphans"], 0);
    }

    /// T9 (issue #74): with no decisions it says so, naming the window as
    /// `render` names it.
    #[test]
    fn decisions_with_none_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write_thread(
            &dir.path().join("alpha"),
            Ulid::generate(),
            &[user("2026-09-24T09:00:00Z", "no decisions here")],
        );
        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        assert!(report.kinds.is_empty());
        let text = render_decisions(&report);
        assert!(text.contains("all threads"), "{text}");
        assert!(text.contains("no decisions recorded"), "{text}");

        let windowed = collect_decisions(
            dir.path(),
            &[],
            None,
            Some(datetime!(2026-09-23 00:00:00 UTC)),
        )
        .unwrap();
        let text = render_decisions(&windowed);
        assert!(text.contains("2026-09-23T00:00:00Z"), "{text}");
        assert!(text.contains("no decisions recorded"), "{text}");
    }

    // ---- the flat threads directory (issue #83) ----

    /// A `thread_started` line, built from the payload type: `created_by`
    /// is required, and a hand-written payload that failed to parse
    /// would be skipped silently, passing a test for the wrong reason.
    fn thread_started(at: &str, project: Option<&str>, root: &Path) -> serde_json::Value {
        json!({
            "kind": "thread_started",
            "author": {"kind": "agent", "id": "runtime"},
            "payload": serde_json::to_value(ThreadStartedPayload {
                project: project.map(str::to_owned),
                root: root.to_path_buf(),
                created_by: Author::User(UserId("steve".into())),
                parent_thread: None,
                step: None,
                front: false,
            })
            .unwrap(),
            "created_at": at,
        })
    }

    /// A `project_switched` line, built from the payload type.
    fn project_switched(at: &str, to: &str, root: &Path) -> serde_json::Value {
        json!({
            "kind": "project_switched",
            "author": {"kind": "agent", "id": "runtime"},
            "payload": serde_json::to_value(ProjectSwitchedPayload {
                from: None,
                to: Some(to.to_owned()),
                root: root.to_path_buf(),
                workspace: None,
            })
            .unwrap(),
            "created_at": at,
        })
    }

    /// A directory to name in a fixture's `thread_started.root`.
    fn root_dir(base: &Path, name: &str) -> PathBuf {
        let root = base.join(name);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// An id the tests can order: `1_700_000_000_000` plus `n` seconds.
    fn id_at(n: u64) -> Ulid {
        Ulid::from_parts(1_700_000_000_000 + n * 1_000, n as u128 + 1)
    }

    /// T1 (issue #83): the flat layout #9 wrote reads exactly like the
    /// legacy project directories, given logs that name their project.
    #[test]
    fn the_flat_layout_reads_like_the_legacy_one() {
        let flat = tempfile::tempdir().unwrap();
        let legacy = tempfile::tempdir().unwrap();
        let root = flat.path().join("work");
        std::fs::create_dir_all(&root).unwrap();
        let alpha = id_at(0);
        let beta = id_at(1);
        // One fixture, built twice: flat with a `thread_started` naming
        // the project, and in `alpha/` and `beta/` with the same lines.
        let build = |base: &Path, named: bool| {
            for (project, id) in [("alpha", alpha), ("beta", beta)] {
                let dir = if named {
                    base.to_path_buf()
                } else {
                    base.join(project)
                };
                let mut lines = vec![
                    user("2026-09-28T12:00:00Z", "a prompt"),
                    call("2026-09-28T12:00:01Z", 100, 0, 50, Some(0.25), None, None),
                ];
                if named {
                    lines.insert(
                        0,
                        thread_started("2026-09-28T11:59:00Z", Some(project), &root),
                    );
                }
                write_thread(&dir, id, &lines);
            }
        };
        build(flat.path(), true);
        build(legacy.path(), false);

        let named = collect(flat.path(), &[], None, None, &no_prices()).unwrap();
        let by_dir = collect(legacy.path(), &[], None, None, &no_prices()).unwrap();
        assert_eq!(named, by_dir);
        assert_eq!(
            named
                .projects
                .iter()
                .map(|p| p.project.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );

        // A base that is not there is an empty report, not an error.
        let missing = collect(&flat.path().join("nope"), &[], None, None, &no_prices()).unwrap();
        assert_eq!(missing, Stats::default());
        assert_eq!(missing.unreadable, 0);
    }

    /// T2 (issue #83): a thread that switched projects counts under the
    /// last project it switched to — in the report, in an issue's rows
    /// and in the narrowed decision record.
    #[test]
    fn a_switched_thread_counts_under_its_last_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = root_dir(dir.path(), "work");
        let id = id_at(0);
        let proposal = Ulid::generate();
        write_decisions(
            dir.path(),
            id,
            &[
                thread_started("2026-09-28T09:00:00Z", Some("alpha"), &root),
                user("2026-09-28T09:00:01Z", "work on #83"),
                project_switched("2026-09-28T09:01:00Z", "beta", &root),
                project_switched("2026-09-28T09:02:00Z", "gamma", &root),
                proposed(proposal, "2026-09-28T09:03:00Z", "project", "a decision"),
                call("2026-09-28T09:04:00Z", 100, 0, 50, Some(0.25), None, None),
            ],
        );

        let stats = collect(dir.path(), &[], None, None, &no_prices()).unwrap();
        assert_eq!(
            stats
                .projects
                .iter()
                .map(|p| p.project.as_str())
                .collect::<Vec<_>>(),
            vec!["gamma"],
            "the last switch wins, not the thread_started root"
        );
        assert_eq!(stats.projects[0].calls, 1);
        assert_eq!(stats.threads[0].project, "gamma");

        let issue = collect_issue(dir.path(), &[], None, 83, None, &no_prices()).unwrap();
        assert_eq!(issue.threads.len(), 1);
        assert_eq!(issue.threads[0].project, "gamma");

        let target = collect_decisions(dir.path(), &[], Some("gamma"), None).unwrap();
        assert_eq!(target.kinds.len(), 1);
        assert_eq!(target.kinds[0].proposed, 1);
        let origin = collect_decisions(dir.path(), &[], Some("alpha"), None).unwrap();
        assert!(origin.kinds.is_empty(), "{:?}", origin.kinds);
        assert_eq!(origin.orphans, 0);
    }

    /// T3 (issue #83): a mixed tree — flat and legacy, a canary whose
    /// `thread_started` disagrees with its directory, two logs that can
    /// be attributed to no project, and an id written twice.
    #[test]
    fn a_mixed_tree_is_attributed_from_the_logs() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let outside = root_dir(base, "x");
        let flat = id_at(0);
        let legacy = id_at(1);
        let canary = id_at(2);
        let outside_named = id_at(3);
        let outside_bare = id_at(4);
        let old = id_at(5);
        let twice = id_at(6);

        write_thread(
            base,
            flat,
            &[
                thread_started("2026-09-28T08:00:00Z", Some("alpha"), base),
                user("2026-09-28T08:00:01Z", "flat"),
                call("2026-09-28T08:00:02Z", 100, 0, 50, Some(0.25), None, None),
            ],
        );
        // The legacy log says nothing about its project: the directory
        // names it.
        write_thread(
            &base.join("alpha"),
            legacy,
            &[
                user("2026-09-28T08:01:00Z", "legacy"),
                call("2026-09-28T08:01:01Z", 100, 0, 50, None, None, None),
            ],
        );
        // The canary: filed under `alpha/`, its own line says `beta`.
        write_thread(
            &base.join("alpha"),
            canary,
            &[
                thread_started("2026-09-28T08:02:00Z", Some("beta"), base),
                user("2026-09-28T08:02:01Z", "filed under alpha, working in beta"),
                call("2026-09-28T08:02:02Z", 100, 0, 50, None, None, None),
            ],
        );
        // An old `_none` log whose root is a folder named `x`.
        write_thread(
            &base.join(LEGACY_NONE_PROJECT),
            outside_named,
            &[
                thread_started("2026-09-28T08:03:00Z", Some(LEGACY_NONE_PROJECT), &outside),
                user("2026-09-28T08:03:01Z", "outside, but its root has a name"),
                call("2026-09-28T08:03:02Z", 100, 0, 50, None, None, None),
            ],
        );
        // And one with no `thread_started` at all: no project.
        write_thread(
            &base.join(LEGACY_NONE_PROJECT),
            outside_bare,
            &[
                user("2026-09-28T08:04:00Z", "outside, unnamed"),
                call("2026-09-28T08:04:01Z", 100, 0, 50, None, None, None),
            ],
        );
        // A pre-phase-4 flat log: no thread_started, no root, no project.
        write_thread(
            base,
            old,
            &[
                user("2026-09-28T08:05:00Z", "before phase 4"),
                call("2026-09-28T08:05:01Z", 100, 0, 50, None, None, None),
            ],
        );
        // One id both flat and in `alpha/`: the flat copy is the one read,
        // and the two copies differ, so the call count shows which.
        write_thread(
            base,
            twice,
            &[
                thread_started("2026-09-28T08:06:00Z", Some("alpha"), base),
                call("2026-09-28T08:06:01Z", 100, 0, 50, None, None, None),
                call("2026-09-28T08:06:02Z", 200, 0, 60, None, None, None),
            ],
        );
        write_thread(
            &base.join("alpha"),
            twice,
            &[
                user("2026-09-28T08:06:00Z", "the copy under alpha/"),
                call("2026-09-28T08:06:01Z", 100, 0, 50, None, None, None),
                call("2026-09-28T08:06:02Z", 200, 0, 60, None, None, None),
                call("2026-09-28T08:06:03Z", 300, 0, 70, None, None, None),
                call("2026-09-28T08:06:04Z", 400, 0, 80, None, None, None),
                call("2026-09-28T08:06:05Z", 500, 0, 90, None, None, None),
            ],
        );

        let stats = collect(base, &[], None, None, &no_prices()).unwrap();
        assert_eq!(
            stats
                .projects
                .iter()
                .map(|p| p.project.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta", "x", "(no project)"],
            "the canary counts in beta, and (no project) sorts last"
        );
        assert_eq!(stats.unreadable, 0);
        let calls =
            |s: &Stats, name: &str| s.projects.iter().find(|p| p.project == name).unwrap().calls;
        // One call per fixture log, except the id written twice: its flat
        // copy holds two calls and its `alpha/` copy five, so the count
        // shows which one was read.
        assert_eq!(
            calls(&stats, "alpha"),
            4,
            "the flat log, the legacy-named log and the flat copy"
        );
        assert_eq!(calls(&stats, "beta"), 1, "the canary follows its own line");
        assert_eq!(calls(&stats, "x"), 1, "the old root's basename names it");
        assert_eq!(
            calls(&stats, NO_PROJECT),
            2,
            "the two logs nothing attributes"
        );

        // `--project "(no project)"` selects exactly those two.
        let none = collect(base, &[], Some(NO_PROJECT), None, &no_prices()).unwrap();
        assert_eq!(none.projects.len(), 1);
        assert_eq!(none.projects[0].project, NO_PROJECT);
        assert_eq!(none.projects[0].calls, 2);
        assert_eq!(none.threads.len(), 2);
        assert_eq!(none.days.len(), 1, "both logs are from the same day");
        // Nothing is counted twice: the per-project sums are the day's.
        let total: u32 = stats.projects.iter().map(|p| p.calls).sum();
        assert_eq!(stats.days.iter().map(|d| d.calls).sum::<u32>(), total);
    }

    /// T4 (issue #83): `--thread` finds an id in either layout, and a
    /// `--project` that names another project says which project the
    /// thread is in.
    #[test]
    fn collect_thread_finds_flat_and_legacy_and_names_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let flat = id_at(0);
        let legacy = id_at(1);
        let outside = id_at(2);
        write_thread(
            base,
            flat,
            &[
                thread_started("2026-09-28T08:00:00Z", Some("alpha"), base),
                call("2026-09-28T08:00:01Z", 100, 0, 50, Some(0.25), None, None),
            ],
        );
        write_thread(
            &base.join("alpha"),
            legacy,
            &[call(
                "2026-09-28T08:01:00Z",
                100,
                0,
                50,
                Some(0.25),
                None,
                None,
            )],
        );
        write_thread(
            base,
            outside,
            &[call(
                "2026-09-28T08:02:00Z",
                100,
                0,
                50,
                Some(0.25),
                None,
                None,
            )],
        );

        let book = no_prices();
        assert_eq!(
            collect_thread(base, &[], None, flat, None, &book)
                .unwrap()
                .project,
            "alpha"
        );
        assert_eq!(
            collect_thread(base, &[], None, legacy, None, &book)
                .unwrap()
                .project,
            "alpha",
            "a legacy id is found too, named by its directory"
        );

        let err = collect_thread(base, &[], Some("beta"), flat, None, &book)
            .unwrap_err()
            .to_string();
        assert_eq!(err, format!("thread {flat} is in alpha, not beta"));

        let err = collect_thread(base, &[], Some("alpha"), outside, None, &book)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("thread {outside} is in {NO_PROJECT}, not alpha")
        );

        let missing = Ulid::generate();
        let err = collect_thread(base, &[], None, missing, None, &book)
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!("no thread {missing} found under {}", base.display())
        );
    }

    /// T5 (issue #83): a log that cannot be read has no lines to
    /// attribute it by, so it counts unless the filter can be shown to
    /// exclude it.
    #[test]
    fn unreadable_logs_count_by_what_can_be_shown() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let good = id_at(0);
        write_thread(
            base,
            good,
            &[
                thread_started("2026-09-28T08:00:00Z", Some("alpha"), base),
                call("2026-09-28T08:00:01Z", 100, 0, 50, Some(0.25), None, None),
            ],
        );
        // A corrupt flat log, and a corrupt `alpha/` one.
        std::fs::write(base.join(format!("{}.jsonl", id_at(1))), b"{not json").unwrap();
        std::fs::create_dir_all(base.join("alpha")).unwrap();
        std::fs::write(
            base.join("alpha").join(format!("{}.jsonl", id_at(2))),
            b"{not json",
        )
        .unwrap();

        let book = no_prices();
        let all = collect(base, &[], None, None, &book).unwrap();
        assert_eq!(all.unreadable, 2, "neither log can be attributed");
        let alpha = collect(base, &[], Some("alpha"), None, &book).unwrap();
        assert_eq!(
            alpha.unreadable, 1,
            "only the log in alpha/ can be shown to be alpha's"
        );
        let beta = collect(base, &[], Some("beta"), None, &book).unwrap();
        assert_eq!(beta.unreadable, 0);
        let decisions = collect_decisions(base, &[], Some("alpha"), None).unwrap();
        assert_eq!(decisions.unreadable, 1, "the same rule for the record");
    }
    // ---- start-up proposals (issue #85) ----

    /// T6 (issue #85): the model's `project` proposals and start-up ones
    /// get their own rows, in that order, each with its own figures, and
    /// `--json` marks the start-up row alone.
    #[test]
    fn t6a_startup_proposals_have_their_own_row() {
        let dir = tempfile::tempdir().unwrap();
        let plain = [DecisionLine {
            kind: "project",
            at: "2026-10-05T09:00:00Z",
            answer: Some(("yes", PERSON)),
        }];
        let startup = [
            DecisionLine {
                kind: "project",
                at: "2026-10-05T09:05:00Z",
                answer: Some(("no", PERSON)),
            },
            DecisionLine {
                kind: "project",
                at: "2026-10-05T09:06:00Z",
                answer: None,
            },
        ];
        decision_thread(&dir.path().join("model"), &plain);
        startup_thread(&dir.path().join("start-up"), &startup);

        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        let names: Vec<String> = report.kinds.iter().map(row_name).collect();
        assert_eq!(
            names,
            vec!["project".to_owned(), "project (start-up)".to_owned()],
            "the start-up row follows the plain one"
        );
        for (row, table, name) in [
            (&report.kinds[0], &plain[..], "project"),
            (&report.kinds[1], &startup[..], "project (start-up)"),
        ] {
            let (proposed, yes, no, corrected, withdrawn, pending, rate) =
                expected_row(table, "project");
            assert_eq!(
                (
                    row.proposed,
                    row.yes,
                    row.no,
                    row.corrected,
                    row.withdrawn,
                    row.pending
                ),
                (proposed, yes, no, corrected, withdrawn, pending),
                "{name}"
            );
            assert_eq!(row.rate, rate, "{name} rate");
            assert_eq!(row.last_30, rate, "{name} last 30: under 30 answered");
        }
        assert!(!report.kinds[0].startup, "the plain row");
        assert!(report.kinds[1].startup, "the start-up row");

        // The model's own record is its own: the plain row's rate comes
        // from its one `yes`, not from the start-up `no` beside it.
        assert_eq!(report.kinds[0].yes, 1);
        assert_eq!(report.kinds[0].no, 0);
        assert_eq!(report.kinds[1].no, 1);

        let json = serde_json::to_value(&report).unwrap();
        let rows = json["kinds"].as_array().unwrap();
        assert!(rows[0].get("startup").is_none(), "{}", rows[0]);
        assert_eq!(rows[1]["startup"], json!(true));
    }

    /// T6c (issue #85): the rendered table lines up — the start-up row
    /// carries the same column count as the plain one, and the header and
    /// both rows start their first figure in the same column.
    #[test]
    fn t6c_the_rendered_table_lines_up() {
        let dir = tempfile::tempdir().unwrap();
        let plain = [DecisionLine {
            kind: "project",
            at: "2026-10-05T09:00:00Z",
            answer: Some(("yes", PERSON)),
        }];
        let startup = [DecisionLine {
            kind: "project",
            at: "2026-10-05T09:05:00Z",
            answer: Some(("no", PERSON)),
        }];
        decision_thread(&dir.path().join("model"), &plain);
        startup_thread(&dir.path().join("start-up"), &startup);
        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        let text = render_decisions(&report);
        let header = text.lines().find(|l| l.starts_with("kind")).unwrap();
        // `project (start-up)` also starts with `project `, so the plain
        // row is the one without the suffix.
        let plain_row = text
            .lines()
            .find(|l| l.starts_with("project ") && !l.starts_with("project ("))
            .unwrap();
        let startup_row = text
            .lines()
            .find(|l| l.starts_with("project (start-up)"))
            .unwrap();
        // Every figure after the name, counted the same way.
        let figures = |line: &str, name: &str| line[name.len()..].split_whitespace().count();
        assert_eq!(
            figures(plain_row, "project"),
            figures(startup_row, "project (start-up)"),
            "the same figures:\n{text}"
        );
        // And the numbers begin in the same column on every line: the
        // name column is as wide as the longest name printed, so the
        // header's first figure field starts where each row's does.
        let name_width = header.find("proposed").unwrap() - 1;
        for line in [&header, &plain_row, &startup_row] {
            assert_eq!(line.len(), header.len(), "a fixed-width table:\n{text}");
        }
        assert_eq!(
            &plain_row[..name_width],
            &format!("{:<name_width$}", "project"),
            "the plain row's name column:\n{text}"
        );
        assert_eq!(
            &startup_row[..name_width],
            &format!("{:<name_width$}", "project (start-up)"),
            "the start-up row's name column, not truncated:\n{text}"
        );
        for line in [&plain_row, &startup_row] {
            assert_eq!(
                line.as_bytes()[name_width],
                b' ',
                "the name column ends where the numbers begin:\n{text}"
            );
        }
    }
    /// T6b: only start-up proposals: the start-up row alone, and no
    /// all-zero `project` row beside it.
    #[test]
    fn t6b_only_startup_proposals_print_one_row() {
        let dir = tempfile::tempdir().unwrap();
        let startup = [DecisionLine {
            kind: "project",
            at: "2026-10-05T09:00:00Z",
            answer: Some(("yes", PERSON)),
        }];
        startup_thread(&dir.path().join("start-up"), &startup);

        let report = collect_decisions(dir.path(), &[], None, None).unwrap();
        let names: Vec<String> = report.kinds.iter().map(row_name).collect();
        assert_eq!(names, vec!["project (start-up)".to_owned()]);
        let (proposed, yes, no, corrected, withdrawn, pending, rate) =
            expected_row(&startup, "project");
        let row = &report.kinds[0];
        assert_eq!(
            (
                row.proposed,
                row.yes,
                row.no,
                row.corrected,
                row.withdrawn,
                row.pending
            ),
            (proposed, yes, no, corrected, withdrawn, pending)
        );
        assert_eq!(row.rate, rate);
        let text = render_decisions(&report);
        assert!(!text.lines().any(|l| l == "project"), "{text}");
        assert!(text.contains("project (start-up)"), "{text}");
    }

    /// #85's review: a table with plain rows only keeps the 14-wide
    /// name column it always had.
    #[test]
    fn a_plain_decisions_table_keeps_its_name_column() {
        let report = DecisionReport {
            kinds: vec![DecisionKindStats {
                kind: DecisionKind::Project,
                proposed: 1,
                yes: 1,
                no: 0,
                corrected: 0,
                withdrawn: 0,
                pending: 0,
                rate: Some(100),
                last_30: Some(100),
                startup: false,
            }],
            ..DecisionReport::default()
        };
        let text = render_decisions(&report);
        let header = text.lines().find(|l| l.starts_with("kind")).unwrap();
        // `kind` padded to the column, one space, then `proposed`,
        // which fills its own 8-wide column exactly.
        assert_eq!(header.find("proposed"), Some(MIN_NAME_WIDTH + 1), "{text}");
    }
}
