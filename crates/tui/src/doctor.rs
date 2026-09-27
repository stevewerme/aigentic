//! `aigentic doctor`: the checks in `checks.rs`, one line each, exit 1 on
//! any `fail`. Runs before the REPL's own loading so a broken config or
//! project is a line in the report, not a startup error. Network only
//! behind `--probe`.

use std::path::Path;

use aigentic_runtime::aigentic_providers::ReasoningEffort;

use crate::checks::{
    Check, EffortVerdict, GhCli, JudgedMetric, SAMPLES, Status, check_api_key_env, check_config,
    check_env_ignored, check_github, check_keep_awake, check_participants, check_probe,
    check_project, check_skills, check_threads_dir, check_window, compare_efforts, compare_medians,
    judged_metric, origin_url, unknown_keys,
};
use crate::config;
use crate::skills_cmd::SkillPaths;

/// `aigentic doctor --probe-effort LOW,HIGH [--profile NAME]`: one tiny
/// prompt at each effort, `SAMPLES` times each, every usage printed and
/// the medians compared. Prints the table and returns 0 when all
/// `2 × SAMPLES` calls answered, 2 when one did not (the doctor's
/// existing "cannot tell" code is 1; 2 keeps the two apart).
pub async fn compare(
    config_path: &Path,
    profile_name: Option<&str>,
    pair: &str,
) -> anyhow::Result<i32> {
    let Some((low, high)) = pair.split_once(',') else {
        anyhow::bail!("--probe-effort wants LOW,HIGH, got {pair:?}");
    };
    let efforts = [parse_effort(low)?, parse_effort(high)?];
    let config = config::Config::load(config_path)?;
    let (name, profile) = match profile_name {
        Some(name) => (
            name,
            config.profiles.get(name).ok_or_else(|| {
                anyhow::anyhow!("no profile {name:?} in {}", config_path.display())
            })?,
        ),
        None => {
            let (name, profile) = config.select(None)?;
            (name, profile)
        }
    };
    println!(
        "effort comparison · profile {name} · {} · {}",
        profile.endpoint(),
        profile.model
    );
    match compare_efforts(profile, efforts).await {
        Ok(samples) => {
            println!(
                "{:<10} {:>7} {:>14} {:>17}",
                "effort", "attempt", "output_tokens", "reasoning_tokens"
            );
            for s in &samples {
                print_sample(
                    &s.call.effort,
                    s.attempt.to_string(),
                    s.call.output_tokens,
                    s.call.reasoning_tokens,
                );
            }
            // Effort-major, so the first `SAMPLES` are LOW and the next
            // are HIGH; every sample is present or `compare_efforts` erred.
            let (low_samples, high_samples) = samples.split_at(SAMPLES as usize);
            let low_label = &low_samples[0].call.effort;
            let high_label = &high_samples[0].call.effort;
            for group in [low_samples, high_samples] {
                let output = median(group.iter().map(|s| s.call.output_tokens));
                let reasoning = median(
                    group
                        .iter()
                        .map(|s| s.call.reasoning_tokens.unwrap_or_default()),
                );
                let all_reasoning = group.iter().all(|s| s.call.reasoning_tokens.is_some());
                print_sample(
                    &group[0].call.effort,
                    "median".into(),
                    output,
                    all_reasoning.then_some(reasoning),
                );
            }
            let metric = judged_metric(&samples);
            let (low_median, high_median) = match metric {
                JudgedMetric::Reasoning => (
                    median(low_samples.iter().flat_map(|s| s.call.reasoning_tokens)),
                    median(high_samples.iter().flat_map(|s| s.call.reasoning_tokens)),
                ),
                JudgedMetric::Output => (
                    median(low_samples.iter().map(|s| s.call.output_tokens)),
                    median(high_samples.iter().map(|s| s.call.output_tokens)),
                ),
            };
            let metric_name = match metric {
                JudgedMetric::Reasoning => "reasoning",
                JudgedMetric::Output => "output",
            };
            println!("judged on {metric_name} tokens");
            match compare_medians(low_median, high_median) {
                EffortVerdict::Honours => {
                    println!("efforts {low_label} and {high_label}: the endpoint honours the field")
                }
                EffortVerdict::NoClearDifference => println!(
                    "no clear difference: the endpoint may ignore the field, or {low_label} and \
                     {high_label} behave alike on a short prompt"
                ),
                EffortVerdict::Below => {
                    println!("{high_label} reasoned less than {low_label}: inconclusive");
                }
            }
            Ok(0)
        }
        Err(e) => {
            // The endpoint's own words, never a key: the API key never
            // enters this message (it comes from the environment).
            println!("comparison could not run: {e}");
            Ok(2)
        }
    }
}

/// One row of the table: an effort, what the row is (its attempt number or
/// `median`), and its usage. `None` reasoning prints `-`.
fn print_sample(effort: &str, row: String, output: u64, reasoning: Option<u64>) {
    let reasoning = reasoning.map_or_else(|| "-".to_owned(), |n| n.to_string());
    println!("{effort:<10} {row:>7} {output:>14} {reasoning:>17}");
}

/// The middle of an odd-length run of samples: `compare_efforts` returns
/// `SAMPLES` (odd) per effort, so one sample is the median.
fn median(values: impl Iterator<Item = u64>) -> u64 {
    let mut values: Vec<u64> = values.collect();
    values.sort_unstable();
    values[values.len() / 2]
}

/// `1` or `low`: an integer or a label, the two shapes the config takes.
fn parse_effort(text: &str) -> anyhow::Result<ReasoningEffort> {
    let text = text.trim();
    if let Ok(n) = text.parse::<u64>() {
        return Ok(ReasoningEffort::Int(n));
    }
    if text.is_empty() {
        anyhow::bail!("--probe-effort: an empty effort");
    }
    Ok(ReasoningEffort::Label(text.to_owned()))
}

/// Run every check and print it; `1` when any failed, or when `strict`
/// and any key was unknown.
pub async fn run(config_path: &Path, cwd: &Path, probe: bool, strict: bool) -> anyhow::Result<i32> {
    let mut checks = Vec::new();
    let (found, config) = check_config(config_path);
    checks.extend(found);
    let Some(config) = config else {
        return Ok(finish(&checks, strict));
    };
    for (name, profile) in &config.profiles {
        checks.push(check_api_key_env(name, profile));
        checks.push(check_window(name, profile));
    }
    let threads_base = config
        .threads_dir
        .clone()
        .unwrap_or_else(config::default_threads_dir);
    checks.push(check_threads_dir(&threads_base));

    // The default profile's window decides the knowledge mode, as
    // `project show` does; the provider is built without a key and never
    // called here.
    let (_, profile) = config.select(None)?;
    let provider = profile.build_provider(String::new());
    let (c, project) = check_project(cwd, &*provider);
    checks.push(c);
    // `aigentic.toml`'s unknown keys, beside the config's: the project
    // check above already opened the file, so this is the same walk's
    // result, not a second read.
    if let Some(project) = &project {
        checks.push(unknown_keys("project keys", &project.unknown, |path| {
            aigentic_runtime::project::Project::suggest_key(path)
        }));
    }

    let config_dir = config_path
        .parent()
        .map_or_else(|| Path::new(".").to_path_buf(), Path::to_path_buf);
    // The daemon's owner: server.toml's first user when that file is
    // beside config.toml, else the config's user, who owns the embedded
    // daemon. Never a token, never printed beyond the name.
    let (owner, source) = match aigentic_server::ServerConfig::load(&config_dir.join("server.toml"))
        .ok()
        .and_then(|s| s.owner().map(str::to_owned))
    {
        Some(owner) => (owner, "server.toml"),
        None => (config.user_name(), "config.toml's user"),
    };
    checks.push(check_participants(project.as_ref(), &owner, source));
    // Whether the daemon holding this machine's threads would hold the
    // machine awake (issue #47): the same key and the same search the
    // daemon uses, so the answer is the daemon's answer.
    checks.push(check_keep_awake(
        aigentic_server::awake::detect(config.keep_awake).as_ref(),
    ));
    checks.push(check_env_ignored(
        project.as_ref().map_or(cwd, |p| p.root.as_path()),
    ));
    let project_root = project
        .as_ref()
        .map_or_else(|| cwd.to_path_buf(), |p| p.root.clone());
    let paths = SkillPaths::new(&project_root, &config_dir, config.bundled_dir.as_deref());
    let global_instructions = config
        .global_instructions
        .clone()
        .unwrap_or_else(|| config_dir.join("instructions.md"));
    checks.push(check_skills(
        project.as_ref(),
        &config,
        &global_instructions,
        &paths,
        cwd,
    ));

    let remote = origin_url(&project_root);
    checks.push(check_github(project.as_ref(), &GhCli, remote.as_deref()));

    if probe {
        for (name, profile) in &config.profiles {
            checks.push(check_probe(name, profile).await);
        }
    }
    Ok(finish(&checks, strict))
}

fn finish(checks: &[Check], strict: bool) -> i32 {
    for c in checks {
        println!("{}", c.render());
    }
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    let warned = checks.iter().filter(|c| c.status == Status::Warn).count();
    if failed > 0 {
        println!("{failed} failed");
        return 1;
    }
    if strict && warned > 0 {
        // `--strict` promotes a warn to a failure for a caller that wants
        // an unknown key or a missing window to stop the line: the CI
        // check this issue asks for.
        println!("{warned} warned (--strict)");
        return 1;
    }
    if warned > 0 {
        println!("all good ({warned} warning(s))");
    } else {
        println!("all good");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warn() -> Check {
        Check::new("config", Status::Warn, "1 unknown key(s): nope")
    }

    /// The exit code, which is what a script reads: a warn passes unless
    /// `--strict` asks for more, and a fail always passes nothing.
    #[test]
    fn strict_turns_a_warning_into_a_failure() {
        let clean = [Check::ok("config", "fine")];
        assert_eq!(finish(&clean, false), 0);
        assert_eq!(finish(&clean, true), 0);

        let warning = [Check::ok("config", "fine"), warn()];
        assert_eq!(finish(&warning, false), 0, "a lock file is not an error");
        assert_eq!(finish(&warning, true), 1);

        let failing = [Check::fail("config", "broken"), warn()];
        assert_eq!(finish(&failing, false), 1);
        assert_eq!(finish(&failing, true), 1, "a fail outranks a warn");
    }
}
