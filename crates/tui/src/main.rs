//! `aigentic`: a plain streaming REPL over the runtime, and the `skills`
//! subcommands. No full-screen mode; the terminal keeps its scrollback.

mod approve;
mod config;
mod cost;
mod project_cmd;
mod repl;
mod skills_cmd;

use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
use aigentic_runtime::aigentic_log::{Repair, ThreadLog};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{GlobalLayer, Layers, Project, ProjectFile, Runtime};
use anyhow::Context;
use clap::{Parser, Subcommand};
use ulid::Ulid;

use crate::approve::InlineApprover;
use crate::config::Config;
use crate::project_cmd::ProjectCommand;
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
    /// Profile from the config file (default: the project's `[model]
    /// profile`, else the config's default_profile).
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
    /// Initialise or inspect the project here.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// List this project's threads, newest first.
    Threads,
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
    // The nearest aigentic.toml at or above the working directory; the
    // tools still work where the user launched.
    let opened = Project::open(&cwd)?;
    let project_root = opened
        .as_ref()
        .map_or_else(|| cwd.clone(), |p| p.root.clone());
    let skill_paths = SkillPaths::new(&project_root, &config_dir, config.bundled_dir.as_deref());

    let threads_base = config
        .threads_dir
        .clone()
        .unwrap_or_else(config::default_threads_dir);
    let threads_dir = project_cmd::threads_dir_for(&threads_base, opened.as_ref());
    let global_instructions = config
        .global_instructions
        .clone()
        .unwrap_or_else(|| config_dir.join("instructions.md"));
    let project: ProjectFile = opened
        .as_ref()
        .map_or_else(ProjectFile::default, |p| p.file.clone());
    // `--profile` wins over the project's `[model] profile`.
    let profile_arg = cli
        .profile
        .as_deref()
        .or(project.model.as_ref().map(|m| m.profile.as_str()));

    match cli.command {
        Some(Command::Skills { command }) => {
            let code = skills_cmd::run(command, &skill_paths)?;
            std::process::exit(code);
        }
        Some(Command::Project {
            command: ProjectCommand::Init,
        }) => std::process::exit(project_cmd::init(&cwd)?),
        Some(Command::Threads) => {
            let threads = project_cmd::list_threads(&threads_dir)?;
            println!("{}", project_cmd::render_threads(&threads, &threads_dir));
            std::process::exit(0);
        }
        Some(Command::Project {
            command: ProjectCommand::Show,
        }) => {
            // The same report as `/project`, over a runtime that never
            // calls the model: the provider is built without a key so the
            // knowledge mode is decided for this profile's window.
            let (_, profile) = config.select(profile_arg)?;
            let global = GlobalLayer::load(
                &global_instructions,
                config.denied_tools.clone(),
                config.denied_skills.clone(),
            )?;
            let scratch = tempfile::tempdir()?;
            let log = ThreadLog::open(scratch.path(), Ulid::generate())?;
            let runtime = Runtime::new(
                profile.build_provider(String::new()),
                ToolRegistry::builtin(Workdir::new(&cwd)),
                log,
                AgentId("assistant".into()),
            )
            .with_layers(Layers {
                global,
                project: opened.clone(),
            });
            println!("{}", project_cmd::report(&runtime, &global_instructions));
            if !project.mcp_servers.is_empty() {
                println!(
                    "mcp servers (connected at thread start, not here): {}",
                    project
                        .mcp_servers
                        .iter()
                        .map(|s| s.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            std::process::exit(0);
        }
        None => {}
    }

    let (profile_name, profile) = config.select(profile_arg)?;
    let api_key = profile.api_key()?;
    let provider = profile.build_provider(api_key);

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

    // Layers: the owner's instructions and denials, then the project.
    let global = GlobalLayer::load(
        &global_instructions,
        config.denied_tools.clone(),
        config.denied_skills.clone(),
    )?;
    let layers = Layers {
        global,
        project: opened.clone(),
    };

    // Skills: the enabled set minus global denials, hash-verified against
    // the tools the model will see. A tampered or unlocked skill refuses
    // to start with its name; nothing loads silently.
    let mut available = tools.names();
    available.extend(aigentic_runtime::harness_tools::harness_names());
    let available = layers.allowed_tools(&available);
    let enabled = layers.allowed_skills(&project.skills.enabled);
    let skills =
        skills_cmd::load_enabled(&enabled, &skill_paths, &available).context("loading skills")?;

    std::fs::create_dir_all(&threads_dir)
        .with_context(|| format!("creating {}", threads_dir.display()))?;
    let (thread_id, resumed) = match cli.thread {
        Some(id) => (id, true),
        None => (Ulid::generate(), false),
    };
    // A thread from before the per-project layout lives flat in the base
    // directory; resume it where it is rather than start an empty one.
    let log_dir = if resumed
        && !threads_dir.join(format!("{thread_id}.jsonl")).exists()
        && threads_base.join(format!("{thread_id}.jsonl")).exists()
    {
        println!(
            "[thread {thread_id} found in {} (pre-project layout)]",
            threads_base.display()
        );
        threads_base.clone()
    } else {
        threads_dir.clone()
    };
    let (log, torn) = ThreadLog::open_with(&log_dir, thread_id, Repair::TruncateTornTail)?;
    let user = Author::User(UserId(config.user_name()));

    let mut runtime = Runtime::new(provider, tools, log, AgentId("assistant".into()))
        .with_layers(layers)
        .with_compaction(profile.compaction_settings())
        .with_budget(profile.budget())
        .with_model_label(&profile.model)
        .with_policy(project.policy())
        .with_skills(skills)
        .with_approver(Box::new(InlineApprover::new(user.clone())));

    println!(
        "aigentic · profile {profile_name} · {} · {}",
        profile.model,
        profile.endpoint()
    );
    match &opened {
        Some(p) => {
            let mut layers_loaded = Vec::new();
            if runtime.layers().global.instructions.is_some() {
                layers_loaded.push("global".to_owned());
            }
            if p.instructions.is_some() {
                layers_loaded.push("project".to_owned());
            }
            if !runtime.knowledge().is_empty() {
                let mode = match runtime.knowledge_mode() {
                    aigentic_runtime::KnowledgeMode::Inline => "inline",
                    aigentic_runtime::KnowledgeMode::Index => "index",
                };
                layers_loaded.push(format!(
                    "knowledge ({mode}, {} files)",
                    runtime.knowledge().files.len()
                ));
            }
            if !p.memory.is_empty() {
                layers_loaded.push(format!("memory ({} files)", p.memory.len()));
            }
            let thread_count = project_cmd::list_threads(&threads_dir)
                .map(|t| t.len())
                .unwrap_or(0);
            println!(
                "project {} at {} · layers: {} · {} skills · {} policy rules · {} tools visible · {} threads",
                p.name,
                p.root.display(),
                if layers_loaded.is_empty() {
                    "none".to_owned()
                } else {
                    layers_loaded.join(", ")
                },
                runtime.skills().len(),
                runtime.policy().rules.len(),
                runtime.tool_specs().len(),
                thread_count
            );
        }
        None => {
            println!("no aigentic.toml here or above: no skills, default policy, no MCP servers")
        }
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
    println!(
        "/help lists commands; /project shows the layers; /threads lists threads; /quit exits"
    );

    let resumed = runtime.resume(torn, &mut |_| {})?;
    let history = config_path.with_file_name("history");
    repl::Repl::new(runtime, user, history)
        .with_project_paths(threads_dir, global_instructions)
        .run(resumed)
        .await
}
