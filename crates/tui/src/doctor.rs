//! `aigentic doctor`: the checks in `checks.rs`, one line each, exit 1 on
//! any `fail`. Runs before the REPL's own loading so a broken config or
//! project is a line in the report, not a startup error. Network only
//! behind `--probe`.

use std::path::Path;

use crate::checks::{
    Check, GhCli, Status, check_api_key_env, check_config, check_env_ignored, check_github,
    check_participants, check_probe, check_project, check_skills, check_threads_dir, check_window,
    origin_url,
};
use crate::config;
use crate::skills_cmd::SkillPaths;

/// Run every check and print it; `1` when any failed.
pub async fn run(config_path: &Path, cwd: &Path, probe: bool) -> anyhow::Result<i32> {
    let mut checks = Vec::new();
    let (c, config) = check_config(config_path);
    checks.push(c);
    let Some(config) = config else {
        return Ok(finish(&checks));
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
    Ok(finish(&checks))
}

fn finish(checks: &[Check]) -> i32 {
    for c in checks {
        println!("{}", c.render());
    }
    let failed = checks.iter().filter(|c| c.status == Status::Fail).count();
    if failed > 0 {
        println!("{failed} failed");
        1
    } else {
        println!("all good");
        0
    }
}
