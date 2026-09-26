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

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aigentic_runtime::Prices;
use aigentic_runtime::aigentic_core::{Author, ContentBlock, EventKind};
use aigentic_runtime::aigentic_log::{
    AssistantMessagePayload, Invoker, SkillLoadedPayload, ThreadLog, ThreadRenamedPayload,
    ToolResultPayload, TurnEndedPayload, Usage, UserMessagePayload,
};
use aigentic_server::config::Config;
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
    /// Up to five threads by effective spend, dearest first.
    pub threads: Vec<ThreadSpend>,
    /// Threads whose files could not be read; counted so the totals are
    /// never silently short.
    pub unreadable: u32,
}

#[derive(Debug, Default, Serialize)]
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
}

#[derive(Debug, Default, Serialize)]
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
}

/// What a thread's dollars really are (#40): stamped plus retro-priced,
/// `Some` when either exists, `None` when neither does.
fn effective(t: &ThreadSpend) -> Option<f64> {
    match (t.spent, t.price_estimated_spent) {
        (None, None) => None,
        (stamped, estimated) => Some(stamped.unwrap_or(0.0) + estimated.unwrap_or(0.0)),
    }
}

/// The same sum for the drill-downs, which carry a `ThreadReport`.
fn effective_report(t: &ThreadReport) -> Option<f64> {
    match (t.spent, t.price_estimated_spent) {
        (None, None) => None,
        (stamped, estimated) => Some(stamped.unwrap_or(0.0) + estimated.unwrap_or(0.0)),
    }
}

#[derive(Debug, Default, Serialize)]
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
}

/// `stats --issue <n>` (issue #40): every thread whose first own-user
/// message names that issue, with a total over them.
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
enum Cost {
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
    fn table(&self, u: &Usage) -> Option<Prices> {
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
fn classify_cost(u: &Usage, book: &PriceBook) -> Cost {
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
    project: Option<&str>,
    since: Option<&str>,
    json: bool,
    book: &PriceBook,
) -> anyhow::Result<()> {
    let cutoff = match since {
        Some(arg) => Some(parse_since(arg, OffsetDateTime::now_utc())?),
        None => None,
    };
    let stats = collect(base, project, cutoff, book)?;
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

/// Every project directory under `base`, `--project` narrowing to one.
/// `_none` is the directory for work outside a project: it is a real
/// group, not something to hide. A named project with no threads is an
/// empty group, not an error.
fn groups(base: &Path, project: Option<&str>) -> anyhow::Result<Vec<(String, PathBuf)>> {
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
        groups.push((want.to_owned(), base.join(want)));
    }
    groups.sort();
    Ok(groups)
}

/// Walk every project directory under `base` and fold each thread's
/// events into the day, project and top-thread totals.
pub fn collect(
    base: &Path,
    project: Option<&str>,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<Stats> {
    let mut stats = Stats {
        since: cutoff.map(|c| c.format(&Rfc3339).unwrap_or_default()),
        ..Stats::default()
    };

    let mut days: BTreeMap<String, Accum> = BTreeMap::new();
    let mut projects: Vec<ProjectStats> = Vec::new();
    let mut threads: Vec<ThreadSpend> = Vec::new();

    for (name, dir) in groups(base, project)? {
        let mut acc = Accum::default();
        for id in thread_ids(&dir) {
            match ThreadLog::open(&dir, id).and_then(|log| log.read_all()) {
                Ok(events) => {
                    let mut thread = Accum::default();
                    let meta = absorb(&mut acc, &mut thread, &events, cutoff, &mut days, book);
                    // #40: a thread with no call inside the window is not
                    // a row. Its title is a label, but a row is a total.
                    if thread.calls > 0 {
                        threads.push(thread.into_spend(id.to_string(), &name, meta.title));
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
    stats.projects = projects;
    stats.days = days.into_iter().map(|(day, a)| a.into_day(day)).collect();
    Ok(stats)
}

/// `aigentic stats --thread <id>` (issue #40): one thread read on its
/// own, under the same `--since` window as the report. `--project`
/// narrows the search; the global `--thread` supplies the id.
pub fn run_thread(
    base: &Path,
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
    let report = collect_thread(base, project, id, cutoff, book)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_thread(&report));
    }
    Ok(())
}

/// `aigentic stats --issue <n>` (issue #40): every thread whose first
/// own-user message names that issue, dearest first, with a total.
pub fn run_issue(
    base: &Path,
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
    let report = collect_issue(base, project, issue, cutoff, book)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_issue(&report));
    }
    Ok(())
}

/// Read one thread's log and fold it. An id that no project holds is an
/// error naming it, so a typo never reads as an empty report.
fn collect_thread(
    base: &Path,
    project: Option<&str>,
    id: Ulid,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<ThreadReport> {
    for (name, dir) in groups(base, project)? {
        if !dir.join(format!("{id}.jsonl")).is_file() {
            continue;
        }
        let events = ThreadLog::open(&dir, id)
            .and_then(|log| log.read_all())
            .with_context(|| format!("reading {}/{id}.jsonl", dir.display()))?;
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

/// Every thread whose first own-user message names `issue`, folded.
/// `--thread`'s walk, one project at a time.
fn collect_issue(
    base: &Path,
    project: Option<&str>,
    issue: u64,
    cutoff: Option<OffsetDateTime>,
    book: &PriceBook,
) -> anyhow::Result<IssueReport> {
    let mut threads: Vec<ThreadReport> = Vec::new();
    for (name, dir) in groups(base, project)? {
        for id in thread_ids(&dir) {
            let Ok(events) = ThreadLog::open(&dir, id).and_then(|log| log.read_all()) else {
                continue;
            };
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
            if !names_issue(&meta, issue) {
                continue;
            }
            // The window applies here too: a matched thread with no call
            // inside it has no row, exactly as in the main report.
            if thread.calls == 0 {
                continue;
            }
            threads.push(thread.into_report(id.to_string(), &name, meta));
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
}

impl Accum {
    fn add_call(&mut self, context: u64, cache_read: u64, cost: Cost) {
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
            day,
        }
    }

    /// The row the report's thread table shows.
    fn into_spend(self, id: String, project: &str, title: String) -> ThreadSpend {
        ThreadSpend {
            id,
            project: project.to_owned(),
            title,
            calls: self.calls,
            spent: self.spent,
            price_estimated_spent: self.price_estimated_spent,
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
    // fully folded.
    let mut calls: Vec<(String, u64, u64, Cost, bool)> = Vec::new();
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
                if let Ok(p) = serde_json::from_value::<ThreadRenamedPayload>(event.payload.clone())
                {
                    renamed = Some(p.title);
                }
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
/// each side — so `#40` and `see #40.` match, `#400` and `#40x` do not —
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
/// non-alphanumeric character, and the digits must run to the text's end
/// or a non-digit — so `#400`, `#40x` and `a#40` name nothing.
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
fn money(stamped: Option<f64>, estimated: Option<f64>) -> String {
    match (stamped, estimated) {
        (Some(a), Some(b)) => format!("${a:.4}+~${b:.4}"),
        (Some(a), None) => format!("${a:.4}"),
        (None, Some(b)) => format!("~${b:.4}"),
        (None, None) => "-".into(),
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
    out.push_str(&format!(
        "context    peak {:>10}   mean {:>10}   cache hit {:.0}%\n",
        t.peak_context,
        t.mean_context,
        t.hit_rate * 100.0
    ));
    out.push_str(&format!("turns      {}\n", turns_line(&t.turns)));
    if t.unstamped_calls > 0 {
        out.push_str(&format!(
            "stamping   {} calls predate model stamping \
             (pass --assume-profile NAME to estimate them)\n",
            t.unstamped_calls
        ));
    }
    out
}

/// `stats --issue <n>`: the matched threads, dearest first, and the
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
    // #40: the two dollars are never added together. `$31.78 + ~$1.59`
    // says what was measured and what was guessed.
    out.push_str(&format!(
        "{}\n",
        cost_line(stamped, estimated, priced, price_estimated, unpriced, calls)
    ));
    out.push_str(&format!("calls      {calls}   retries {retries}\n"));
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
    if !stats.days.is_empty() {
        out.push_str("\nday          calls     priced               cost      peak     hit\n");
        for d in &stats.days {
            out.push_str(&format!(
                "{:<12} {:>5} {:>10} {:>18} {:>9} {:>6.0}%\n",
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
        out.push_str(
            "\nproject                          calls     priced               cost      peak     hit\n",
        );
        for p in &stats.projects {
            out.push_str(&format!(
                "{:<32} {:>5} {:>10} {:>18} {:>9} {:>6.0}%\n",
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

#[cfg(test)]
mod tests {
    use super::*;
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
        let stats = collect(base, None, None, &no_prices()).unwrap();

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
        let stats = collect(f.dir.path(), None, Some(cutoff), &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), None, Some(week), &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), None, None, &no_prices()).unwrap();
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

        let stats = collect(dir.path(), None, None, &book_of(PRICED_CONFIG)).unwrap();
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

        let stats = collect(dir.path(), None, None, &no_prices()).unwrap();
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
        let report = collect_thread(dir.path(), None, id, None, &book).unwrap();

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
        let after = collect_thread(dir.path(), None, id, cutoff, &book).unwrap();
        assert_eq!(after.calls, 0);
        assert_eq!(after.sweeps, 0);
        assert_eq!(after.tool_errors, 0);
        assert_eq!(after.title, "one thread, everything in it");

        // An id no project holds is an error naming it, never an empty
        // report.
        let missing = Ulid::generate();
        let err = collect_thread(dir.path(), None, missing, None, &book)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&missing.to_string()), "{err}");

        // `--project` narrows the search: the same id is not in `beta`.
        assert!(collect_thread(dir.path(), Some("beta"), id, None, &book).is_err());
        assert!(collect_thread(dir.path(), Some("alpha"), id, None, &book).is_ok());
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
        let report = collect_issue(dir.path(), None, 40, None, &book).unwrap();
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

        // `#400` and `#40x` are not `#40`, and `4` is not `#40`.
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
            collect_issue(dir2.path(), None, 40, None, &book)
                .unwrap()
                .threads
                .is_empty()
        );
        // `#400` names 400, not 40 — the digits are one reference, so
        // the boundary rule can never split them into 4 and 00.
        assert_eq!(
            collect_issue(dir2.path(), None, 400, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
        assert!(
            collect_issue(dir.path(), None, 4, None, &book)
                .unwrap()
                .threads
                .is_empty(),
            "#40 must not match 4"
        );
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
            collect_issue(dir.path(), None, 40, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
        assert!(
            collect_issue(dir.path(), None, 44, None, &book)
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
            collect_issue(dir2.path(), None, 40, None, &book)
                .unwrap()
                .threads
                .len(),
            1
        );
    }

    #[test]
    fn json_round_trips_the_same_totals() {
        let f = fixture();
        let stats = collect(f.dir.path(), None, None, &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), Some("beta"), None, &no_prices()).unwrap();
        assert_eq!(stats.projects.len(), 1);
        assert_eq!(stats.projects[0].project, "beta");
        assert_eq!(stats.projects[0].calls, 1);
        // A project with no threads is an empty report, not an error.
        let none = collect(f.dir.path(), Some("gamma"), None, &no_prices()).unwrap();
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
        let stats = collect(f.dir.path(), None, None, &no_prices()).unwrap();
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
        let stats = collect(dir.path(), None, None, &book_of(PRICED_CONFIG)).unwrap();
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
        let none = collect(dir.path(), None, None, &no_prices()).unwrap();
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
        let stats = collect(dir.path(), None, None, &book).unwrap();
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
}
