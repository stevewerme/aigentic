//! `aigentic`: a plain streaming REPL over the runtime. No full-screen mode;
//! the terminal keeps its normal scrollback.

mod config;
mod cost;
mod repl;

use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
use aigentic_runtime::aigentic_log::{Repair, ThreadLog};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{Runtime, load_instructions};
use anyhow::Context;
use clap::Parser;
use ulid::Ulid;

use crate::config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "aigentic",
    version,
    about = "Aigentic agent harness, streaming REPL"
)]
struct Cli {
    /// Config file (default: ~/.config/aigentic/config.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Thread to resume. Omit to start a new thread; its id is printed.
    #[arg(long)]
    thread: Option<Ulid>,
    /// Profile from the config file (default: its default_profile).
    #[arg(long)]
    profile: Option<String>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // `.env` in the current directory, if present. Never printed.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config = Config::load(&config_path)?;
    let (profile_name, profile) = config.select(cli.profile.as_deref())?;
    let api_key = profile.api_key()?;

    let cwd = std::env::current_dir().context("current directory")?;
    let provider = profile.build_provider(api_key);
    let tools = ToolRegistry::builtin(Workdir::new(&cwd));

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

    let mut runtime = Runtime::new(provider, tools, log, AgentId("assistant".into()))
        .with_instructions(instructions)
        .with_compaction(profile.compaction_settings())
        .with_model_label(&profile.model);

    println!(
        "aigentic · profile {profile_name} · {} · {}",
        profile.model,
        profile.endpoint()
    );
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
    println!("/cost shows tokens, /pin <text> pins a fact, /compact compacts, /quit exits");

    let resumed = runtime.resume(torn, &mut |_| {})?;
    let user = Author::User(UserId(config.user_name()));
    let history = config_path.with_file_name("history");
    repl::Repl::new(runtime, user, history).run(resumed).await
}
