//! `aigentic doctor`: the checks in `checks.rs`, one line each, exit 1 on
//! any `fail`. Runs before the REPL's own loading so a broken config or
//! project is a line in the report, not a startup error. Network only
//! behind `--probe`.

use std::path::Path;

use aigentic_runtime::aigentic_providers::ReasoningEffort;

use crate::checks::{
    Check, GhCli, Status, check_api_key_env, check_config, check_env_ignored, check_github,
    check_participants, check_probe, check_project, check_skills, check_threads_dir, check_window,
    compare_efforts, origin_url, unknown_keys,
};
use crate::config;
use crate::skills_cmd::SkillPaths;

/// `aigentic doctor --probe-effort LOW,HIGH [--profile NAME]`: one tiny
/// prompt at each effort, both usages side by side. Prints the table and
/// returns 0 when both calls answered, 2 when one did not (the doctor's
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
        Ok(calls) => {
            println!(
                "{:<10} {:>14} {:>17}",
                "effort", "output_tokens", "reasoning_tokens"
            );
            for c in &calls {
                let reasoning = c
                    .reasoning_tokens
                    .map_or_else(|| "-".to_owned(), |n| n.to_string());
                println!("{:<10} {:>14} {:>17}", c.effort, c.output_tokens, reasoning);
            }
            let [a, b] = &calls[..] else {
                unreachable!("two calls were asked for");
            };
            if a.output_tokens == b.output_tokens && a.reasoning_tokens == b.reasoning_tokens {
                println!(
                    "no difference at efforts {} and {}: the endpoint looks like it ignores the field",
                    a.effort, b.effort
                );
            } else {
                println!(
                    "efforts {} and {} differ: the endpoint honours the field",
                    a.effort, b.effort
                );
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
