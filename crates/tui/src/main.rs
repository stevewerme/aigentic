//! `aigentic`: a plain streaming REPL over the runtime, and the `skills`
//! subcommands. No full-screen mode; the terminal keeps its scrollback.

mod approve;
mod config;
mod cost;
mod project;
mod repl;
mod skills_cmd;

use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
use aigentic_runtime::aigentic_log::{Repair, ThreadLog};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{Runtime, load_instructions};
use anyhow::Context;
use clap::{Parser, Subcommand};
use ulid::Ulid;

use crate::approve::InlineApprover;
use crate::config::Config;
use crate::project::ProjectFile;
use crate::skills_cmd::{SkillPaths, SkillsCommand};

#[derive(Debug, Parser)]
#[command(
    name = "aigentic",
    version,
    about = "Aigentic agent harness, streaming REPL"
)]
struct Cli {
    /// Config file (default: ~/.config/aigentic/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Thread to resume. Omit to start a new thread; its id is printed.
    #[arg(long)]
    thread: Option<Ulid>,
    /// Profile from the config file (default: its default_profile).
    #[arg(long)]
    profile: Option<String>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List, check, vendor or update skills.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // `.env` in the current directory, if present. Never printed.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config = Config::load(&config_path)?;
    let cwd = std::env::current_dir().context("current directory")?;
    let config_dir = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let skill_paths = SkillPaths::new(&cwd, &config_dir, config.bundled_dir.as_deref());

    if let Some(Command::Skills { command }) = cli.command {
        let code = skills_cmd::run(command, &skill_paths)?;
        std::process::exit(code);
    }

    let (profile_name, profile) = config.select(cli.profile.as_deref())?;
    let api_key = profile.api_key()?;
    let provider = profile.build_provider(api_key);
    let (project, project_path) = ProjectFile::load(&cwd)?;

    // Tools: built-ins, then every MCP server in aigentic.toml. A server
    // that fails to connect is reported and skipped; the thread still runs.
    let mut tools = ToolRegistry::builtin(Workdir::new(&cwd));
    for server in &project.mcp_servers {
        match tools.connect_mcp(server).await {
            Ok(specs) => {
                println!(
                    "mcp server {} connected: {} tools (class {:?}; descriptions below are the server's own text)",
                    server.name,
                    specs.len(),
                    server.class
                );
                for spec in specs {
                    let desc = repl::truncate_for_display(&spec.description, 2, 200);
                    println!("  {}: {}", spec.name, desc.replace('\n', " "));
                }
            }
            Err(e) => println!("[mcp server {} skipped: {e}]", server.name),
        }
    }

    // Skills: the enabled set, hash-verified. A tampered or unlocked skill
    // refuses to start with its name; nothing loads silently.
    let mut available = tools.names();
    available.extend(aigentic_runtime::harness_tools::harness_names());
    let skills = skills_cmd::load_enabled(&project.skills.enabled, &skill_paths, &available)
        .context("loading skills")?;

    let threads_dir = config
        .threads_dir
        .clone()
        .unwrap_or_else(config::default_threads_dir);
    std::fs::create_dir_all(&threads_dir)
        .with_context(|| format!("creating {}", threads_dir.display()))?;
    let (thread_id, resumed) = match cli.thread {
        Some(id) => (id, true),
        None => (Ulid::generate(), false),
    };
    let (log, torn) = ThreadLog::open_with(&threads_dir, thread_id, Repair::TruncateTornTail)?;
    let instructions = load_instructions(&cwd).context("reading repository instructions")?;
    let user = Author::User(UserId(config.user_name()));

    let mut runtime = Runtime::new(provider, tools, log, AgentId("assistant".into()))
        .with_instructions(instructions)
        .with_compaction(profile.compaction_settings())
        .with_model_label(&profile.model)
        .with_policy(project.policy())
        .with_skills(skills)
        .with_approver(Box::new(InlineApprover::new(user.clone())));

    println!(
        "aigentic · profile {profile_name} · {} · {}",
        profile.model,
        profile.endpoint()
    );
    match project_path {
        Some(p) => println!(
            "project file {} · {} skills enabled · {} policy rules",
            p.display(),
            runtime.skills().len(),
            runtime.policy().rules.len()
        ),
        None => println!("no aigentic.toml here: no skills, default policy, no MCP servers"),
    }
    if resumed {
        println!(
            "thread {thread_id} resumed with {} events from {}",
            runtime.log().len(),
            runtime.log().path().display()
        );
    } else {
        println!("new thread {thread_id} (resume with --thread {thread_id})");
    }
    if let Some(bytes) = torn {
        println!("[repaired torn tail: {bytes} bytes of an unfinished event were cut]");
    }
    println!("/help lists commands; /skills lists skills; /quit exits");

    let resumed = runtime.resume(torn, &mut |_| {})?;
    let history = config_path.with_file_name("history");
    repl::Repl::new(runtime, user, history).run(resumed).await
}
