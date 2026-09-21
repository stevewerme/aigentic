//! `aigentic`: a plain streaming REPL over the runtime. No full-screen mode;
//! the terminal keeps its normal scrollback.

mod config;
mod cost;
mod repl;

use std::path::PathBuf;

use aigentic_runtime::aigentic_core::{AgentId, Author, UserId};
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_runtime::aigentic_providers::{OpenAiCompat, OpenAiCompatConfig};
use aigentic_runtime::aigentic_tools::{Workdir, builtin_tools};
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
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // `.env` in the current directory, if present. Never printed.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let config = Config::load(&config_path)?;
    let api_key = config.api_key()?;

    let cwd = std::env::current_dir().context("current directory")?;
    let provider = OpenAiCompat::new(
        OpenAiCompatConfig::new(&config.base_url, &config.model)
            .with_api_key(api_key)
            .with_max_context_tokens(config.max_context_tokens),
    );
    let tools = builtin_tools(Workdir::new(&cwd));

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
    let log = ThreadLog::open(&threads_dir, thread_id)?;
    let instructions = load_instructions(&cwd).context("reading repository instructions")?;

    let runtime = Runtime::new(Box::new(provider), tools, log, AgentId("assistant".into()))
        .with_instructions(instructions);

    println!("aigentic · {} · {}", config.model, config.base_url);
    if resumed {
        println!(
            "thread {thread_id} resumed with {} events from {}",
            runtime.log().len(),
            runtime.log().path().display()
        );
    } else {
        println!("new thread {thread_id} (resume with --thread {thread_id})");
    }
    println!("/cost shows tokens, /quit exits");

    let user = Author::User(UserId(config.user_name()));
    let history = config_path.with_file_name("history");
    repl::Repl::new(runtime, user, history).run().await
}
