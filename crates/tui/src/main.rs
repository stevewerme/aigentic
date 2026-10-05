//! `aigentic`: a plain streaming REPL over the runtime, and the `skills`
//! subcommands. No full-screen mode; the terminal keeps its scrollback.

mod app;
mod build_cmd;
mod checks;
mod config;
mod doctor;
mod exec;
mod front;
mod init_cmd;
mod pocock;
mod pocock_templates;
mod project_cmd;
mod run_view;
mod skills_cmd;
mod stats;
mod threads_index;

use std::path::PathBuf;

use aigentic_api::client::{Addr, Client};
use aigentic_api::{Request, Response, ThreadState};
use aigentic_runtime::aigentic_core::AgentId;
use aigentic_runtime::aigentic_log::ThreadLog;
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{GlobalLayer, Layers, Mode, Project, ProjectFile, Runtime};
use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use ulid::Ulid;

use crate::app::engine::{ClientRepl, Identity};
use crate::build_cmd::BuildArgs;
use crate::config::Config;
use crate::project_cmd::ProjectCommand;
use crate::skills_cmd::{SkillPaths, SkillsCommand};

/// The version `aigentic --version` prints: the package version, and the
/// commit the binary was built from. The commit is put in the environment
/// by `build.rs`, which reads it out of the repository's git directory.
const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("AIGENTIC_GIT_SHA"),
    ")"
);

#[derive(Debug, Parser)]
#[command(
    name = "aigentic",
    version = VERSION,
    about = "Aigentic agent harness, streaming REPL"
)]
struct Cli {
    /// Config file (default: ~/.config/aigentic/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Thread to resume. Omit to resume your front thread (the first run
    /// makes one; `/new` starts another). `exec` without it starts a new
    /// thread and prints its id.
    #[arg(long, global = true)]
    thread: Option<Ulid>,
    /// Profile from the config file (default: the project's `[model]
    /// profile`, else the config's default_profile). For the embedded
    /// daemon and `project show`; a remote daemon uses the project's.
    #[arg(long, global = true)]
    profile: Option<String>,
    /// Permission mode: manual (default), accept-edits or auto. Session
    /// state; `/mode` changes it later.
    #[arg(long, global = true, default_value = "manual")]
    mode: Mode,
    /// A daemon to connect to (`unix:/path` or `tcp:host:port`). Without
    /// it, a daemon is started in this process for this directory.
    #[arg(long, global = true)]
    server: Option<String>,
    /// The environment variable holding your token for `--server`.
    #[arg(long, global = true, default_value = "AIGENTIC_TOKEN")]
    token_env: String,
    /// The project on the daemon to work in (default: this directory's
    /// `aigentic.toml` name, else the first project you have a role in).
    #[arg(long, global = true)]
    project: Option<String>,
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
    /// List this folder's project's threads, newest first.
    Threads,
    /// What the thread logs spent: cost, calls, context, cache and
    /// retries, by day and project. Reads local logs only. With the
    /// global `--thread <id>`, one thread read on its own (`--project`
    /// narrows the search).
    Stats {
        /// A window: `7d` is the last seven days, `2026-09-23` that day
        /// onwards. Without it, every thread on this machine.
        #[arg(long)]
        since: Option<String>,
        /// The same report as one JSON object.
        #[arg(long)]
        json: bool,
        /// Price calls that carry neither a model nor a profile from this
        /// profile's `[prices]`, and mark them estimated (`~$`). Off by
        /// default: those calls predate model stamping, so guessing their
        /// profile guesses their cost.
        #[arg(long, value_name = "NAME")]
        assume_profile: Option<String>,
        /// Every thread whose first user message names `#<n>`, with a
        /// total: the cost of one issue's build cycle.
        #[arg(long, value_name = "N", conflicts_with = "thread")]
        issue: Option<u64>,
        /// The decision record (issue #74): per kind, how many proposals
        /// were made and how the operator answered them. Honours
        /// `--since`; no producer writes the events yet.
        #[arg(long, conflicts_with_all = ["thread", "issue"])]
        decisions: bool,
    },
    /// Guided setup: config, project file and AGENTS.md, GitHub issues
    /// and labels through `gh`, knowledge links. Shows every file first.
    Init,
    /// Run the daemon: threads for the projects in server.toml, sessions
    /// over a Unix socket.
    Serve {
        /// `unix` (the default socket path) or `unix:/path`; overrides
        /// server.toml's `listen`.
        #[arg(long)]
        listen: Option<String>,
        /// The daemon's own config (default: server.toml beside config.toml).
        #[arg(long)]
        server_config: Option<PathBuf>,
        /// Print a fresh token for this user once and exit; put it in the
        /// user's `token_env` variable on the daemon and in
        /// `AIGENTIC_TOKEN` on their machine. Nothing is stored.
        #[arg(long, value_name = "USER")]
        new_token: Option<String>,
    },
    /// Build one issue to the end: start (or resume) the build workflow
    /// for `n` and follow it, answering a checkpoint `stop`. Progress on
    /// stderr. Exit 0 closed, 3 stopped (a human is needed), 1 a run
    /// stopped by an error. Ctrl-C detaches; the run stays resumable.
    Build {
        /// The issue number.
        n: u64,
        /// The workflow to run (default: the daemon's, `build`).
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
        /// Every notice as a JSON line on stdout.
        #[arg(long)]
        json: bool,
    },
    /// Run one prompt to the end with no prompt shown: progress on
    /// stderr, the final message on stdout. Exit 0 done, 1 failed, 3 done
    /// but a request needed a human (denied), 130 interrupted.
    Exec {
        /// The prompt; read from stdin when absent.
        prompt: Option<String>,
        /// Every notice as a JSON line on stdout, then a summary line.
        #[arg(long)]
        json: bool,
        /// Also write the final message to this file.
        #[arg(short = 'o', long = "output-last-message", value_name = "FILE")]
        output_last: Option<PathBuf>,
    },
    /// Check the config, keys, threads directory, project, skills and
    /// GitHub setup; exit 1 on any failure.
    Doctor {
        /// Also send one tiny completion per profile (the only network use).
        #[arg(long)]
        probe: bool,
        /// Compare two efforts on `--profile`: `LOW,HIGH`, the first is
        /// LOW, the second HIGH; each an integer or a label. Sends the
        /// same prompt SAMPLES (3) times per effort and prints every
        /// call's output and reasoning tokens, then the medians decide.
        /// The effort comes from this flag, so the config is never edited.
        #[arg(long, value_name = "LOW,HIGH")]
        probe_effort: Option<String>,
        /// Exit non-zero on a warning too, unknown config keys included.
        #[arg(long)]
        strict: bool,
    },
}

/// Connect to `--server` as the token's user. The token comes from
/// `--token-env`, never an argument, and is never printed.
async fn connect_server(
    server: &str,
    token_env: &str,
) -> anyhow::Result<(Client, aigentic_api::Welcome)> {
    let addr: Addr = server
        .parse()
        .map_err(|e: String| anyhow::anyhow!("--server: {e}"))?;
    let token = std::env::var(token_env).with_context(|| {
        format!(
            "{token_env} is not set (the token for {addr}; `aigentic serve --new-token` mints one)"
        )
    })?;
    Ok(Client::connect(&addr, &token).await?)
}

/// `connect_server` plus the project a subcommand is about: `--project`,
/// else this directory's `aigentic.toml` name, else the first project
/// the user has a role in.
async fn connect_remote(
    server: &str,
    token_env: &str,
    project: Option<&str>,
    opened: Option<&Project>,
) -> anyhow::Result<(Client, String)> {
    let (client, welcome) = connect_server(server, token_env).await?;
    let project = project
        .map(str::to_owned)
        .or_else(|| opened.map(|p| p.name.clone()))
        .or_else(|| welcome.projects.first().map(|p| p.name.clone()))
        .context("no project: pass --project, or run in a checkout with aigentic.toml")?;
    Ok((client, project))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // `.env` in the current directory, if present. Never printed.
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(config::default_config_path);
    let cwd = std::env::current_dir().context("current directory")?;
    // The doctor reports what the loading below would refuse on.
    if let Some(Command::Doctor {
        probe,
        probe_effort,
        strict,
    }) = cli.command
    {
        if let Some(pair) = probe_effort {
            let code = doctor::compare(&config_path, cli.profile.as_deref(), &pair).await?;
            std::process::exit(code);
        }
        let code = doctor::run(&config_path, &cwd, probe, strict, cli.profile.as_deref()).await?;
        std::process::exit(code);
    }
    if let Some(Command::Init) = cli.command {
        std::process::exit(init_cmd::run(&config_path, &cwd)?);
    }
    if let Some(Command::Serve {
        listen,
        server_config,
        new_token,
    }) = cli.command
    {
        if let Some(user) = new_token {
            let server_path = server_config
                .clone()
                .unwrap_or_else(aigentic_server::config::default_server_config_path);
            let var = aigentic_server::ServerConfig::load(&server_path)
                .ok()
                .and_then(|s| s.users.into_iter().find(|u| u.name == user))
                .and_then(|u| u.token_env)
                .unwrap_or_else(|| format!("AIGENTIC_TOKEN_{}", user.to_ascii_uppercase()));
            println!("{}", aigentic_server::serve::random_token());
            eprintln!(
                "token for {user}, shown once: export it as {var} where the daemon runs and as AIGENTIC_TOKEN on {user}'s machine"
            );
            return Ok(());
        }
        checks::warn_unknown_keys(&config_path, None);
        let config = Config::load(&config_path)?;
        let config_dir = config_path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let server_path =
            server_config.unwrap_or_else(aigentic_server::config::default_server_config_path);
        let server = aigentic_server::ServerConfig::load(&server_path)?;
        let listener =
            aigentic_server::Listener::parse(listen.as_deref().unwrap_or(&server.listen))?;
        println!(
            "aigentic serve · {} · {} users · {} projects · config {}",
            listener.addr(),
            server.users.len(),
            server.projects.len(),
            server_path.display()
        );
        let daemon = std::sync::Arc::new(aigentic_server::Server::from_configs(
            config, config_dir, server,
        ));
        daemon.serve(listener).await?;
        return Ok(());
    }
    let config = Config::load(&config_path)?;
    let config_dir = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    // The nearest aigentic.toml at or above the working directory; the
    // tools still work where the user launched.
    let opened = Project::open(&cwd)?;
    // An unknown key in either file was ignored while loading; say so
    // once, here, rather than refuse the session (issue #37).
    checks::warn_unknown_keys(&config_path, opened.as_ref());
    let project_root = opened
        .as_ref()
        .map_or_else(|| cwd.clone(), |p| p.root.clone());
    let skill_paths = SkillPaths::new(&project_root, &config_dir, config.bundled_dir.as_deref());

    let threads_base = config
        .threads_dir
        .clone()
        .unwrap_or_else(config::default_threads_dir);
    // The embedded daemon's workspaces, as the daemon reads them; an
    // unreadable file is none, since a local read never needs them.
    let workspaces = aigentic_server::workspaces::load_all(&config_dir).unwrap_or_default();
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
        Some(Command::Project {
            command: ProjectCommand::Setup,
        }) => std::process::exit(pocock::run(opened.as_ref())?),
        Some(Command::Threads) if cli.server.is_some() => {
            let (client, project) = connect_remote(
                cli.server.as_deref().expect("checked"),
                &cli.token_env,
                cli.project.as_deref(),
                opened.as_ref(),
            )
            .await?;
            println!(
                "{}",
                app::engine::list_threads_over(&client, &project).await?
            );
            std::process::exit(0);
        }
        Some(Command::Threads) => {
            let project = threads_index::folder_project(&cwd, opened.as_ref(), &workspaces);
            let threads = project_cmd::list_threads(&threads_base, &project, &workspaces);
            println!(
                "{}",
                project_cmd::render_threads(&threads, &project, &threads_base)
            );
            std::process::exit(0);
        }
        // Stats read this machine's logs: a daemon holds threads of other
        // users and other projects, and would answer about its own disk.
        Some(Command::Stats { .. }) if cli.server.is_some() => {
            bail!("stats reads this machine's logs; drop --server and run it locally");
        }
        Some(Command::Stats {
            since,
            json,
            assume_profile,
            issue,
            decisions,
        }) => {
            // #40: the config's price tables travel with the request, so
            // an unpriced call can be retro-priced and marked estimated.
            let book = stats::PriceBook::from_config(&config, assume_profile.as_deref())?;
            // A drill-down, not the whole report: the global `--thread`
            // names one thread, `--issue` names every thread that walked
            // off one issue.
            match (cli.thread, issue) {
                (Some(id), _) => stats::run_thread(
                    &threads_base,
                    &workspaces,
                    cli.project.as_deref(),
                    id,
                    since.as_deref(),
                    json,
                    &book,
                )?,
                (None, Some(n)) => stats::run_issue(
                    &threads_base,
                    &workspaces,
                    cli.project.as_deref(),
                    n,
                    since.as_deref(),
                    json,
                    &book,
                )?,
                (None, None) if decisions => stats::run_decisions(
                    &threads_base,
                    &workspaces,
                    cli.project.as_deref(),
                    since.as_deref(),
                    json,
                )?,
                (None, None) => stats::run(
                    &threads_base,
                    &workspaces,
                    cli.project.as_deref(),
                    since.as_deref(),
                    json,
                    &book,
                )?,
            }
            std::process::exit(0);
        }
        Some(Command::Project {
            command: ProjectCommand::Show,
        }) if cli.server.is_some() => {
            let (client, project) = connect_remote(
                cli.server.as_deref().expect("checked"),
                &cli.token_env,
                cli.project.as_deref(),
                opened.as_ref(),
            )
            .await?;
            println!(
                "{}",
                app::engine::project_report_over(&client, &project).await?
            );
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
                ToolRegistry::builtin(Workdir::new(&cwd), project.tools.bash_timeout()),
                log,
                AgentId("assistant".into()),
            )
            .with_layers(Layers {
                global,
                workspace: None,
                project: opened.clone(),
            });
            println!(
                "{}",
                aigentic_server::reports::project_report(&runtime, &global_instructions)
            );
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
        Some(
            Command::Doctor { .. }
            | Command::Init
            | Command::Serve { .. }
            | Command::Exec { .. }
            | Command::Build { .. },
        )
        | None => {}
    }

    // `exec` reads its prompt before anything connects, so a missing
    // prompt fails fast.
    let exec_args = match &cli.command {
        Some(Command::Exec {
            prompt,
            json,
            output_last,
        }) => Some(exec::ExecArgs {
            prompt: exec::prompt_from(prompt.clone())?,
            json: *json,
            output_last: output_last.clone(),
        }),
        _ => None,
    };
    // `build` names its issue before anything connects, like `exec`.
    let build_args = match &cli.command {
        Some(Command::Build { n, workflow, json }) => Some(BuildArgs {
            issue: *n,
            workflow: workflow.clone(),
            json: *json,
        }),
        _ => None,
    };

    // The REPL over the daemon's client: connect to `--server`, or start a
    // daemon in this process for this directory over a private socket.
    let user = config.user_name();
    let history = config_path.with_file_name("history");
    let (client, welcome, embedded) = match &cli.server {
        Some(server) => {
            if cli.profile.is_some() {
                bail!(
                    "--profile does not apply with --server: the daemon builds each thread from its project's [model] profile"
                );
            }
            let (client, welcome) = connect_server(server, &cli.token_env).await?;
            (client, welcome, None)
        }
        None => {
            // A wrong name fails here, not at the first turn.
            if let Some(name) = cli.profile.as_deref() {
                config.select(Some(name))?;
            }
            let embedded = aigentic_server::Server::embed(
                config.clone(),
                config_dir.clone(),
                project_root.clone(),
                &user,
                cli.profile.as_deref(),
            )
            .await?;
            let (client, welcome) =
                Client::connect(&Addr::Unix(embedded.socket.clone()), &embedded.token).await?;
            (client, welcome, Some(embedded))
        }
    };
    // `exec` never lands in a project by accident: it needs one named, a
    // project file here, or a thread to continue.
    if exec_args.is_some() && cli.project.is_none() && opened.is_none() && cli.thread.is_none() {
        let names: Vec<&str> = welcome.projects.iter().map(|p| p.name.as_str()).collect();
        bail!(
            "exec needs a project: pass --project (one of: {}), run it in a project directory, or pass --thread",
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(", ")
            }
        );
    }
    // The folder's project: what a client would have created in today,
    // and `exec`'s project. A plain run resumes the front thread in
    // whatever project that thread lives in, so this is only a fallback
    // (issue #89).
    let folder_project = cli
        .project
        .clone()
        .or_else(|| opened.as_ref().map(|p| p.name.clone()))
        .or_else(|| embedded.as_ref().map(|e| e.project.clone()))
        .or_else(|| welcome.projects.first().map(|p| p.name.clone()));
    let Some(folder_project) = folder_project else {
        bail!(
            "no project to work in: {} has no role anywhere on {}",
            welcome.user,
            welcome.server
        );
    };
    let project_name = folder_project.clone();
    // `build` asks the daemon to run the issue and follows the run: it
    // never opens a thread of its own, so a run's lead is its one log.
    if let Some(args) = build_args {
        let notices = client.take_notices().context("notice stream")?;
        let outcome = build_cmd::run(
            &client,
            notices,
            &project_name,
            &welcome.user,
            &args,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
        .await?;
        drop(embedded);
        std::process::exit(outcome.code);
    }
    // One pick serves the REPL, plain mode and `exec` (issue #89):
    // `--thread X` opens X; `exec` creates a thread in the folder's
    // project; anything else resumes (or starts) the front thread.
    // `here` is the folder's project, sent on `Front` only (issue #92).
    let here = front::here_project(cli.project.as_deref(), opened.as_ref());
    let picked = front::pick_thread(
        &client,
        cli.thread,
        exec_args.is_some(),
        &project_name,
        here.as_deref(),
    )
    .await?;
    let thread_id = picked.id;
    let (state, events, mode, identity) = match client
        .request(Request::Open {
            thread: thread_id,
            from_seq: 0,
        })
        .await?
    {
        Response::Opened {
            state,
            events,
            mode,
            profile,
            model,
            effort,
            ..
        } => (
            state,
            events,
            mode,
            Identity {
                profile,
                model,
                effort,
            },
        ),
        Response::Refused { reason } => bail!("cannot open thread {thread_id}: {reason}"),
        other => bail!("unexpected reply opening the thread: {other:?}"),
    };
    // The REPL is in the thread's project, not the folder's (issue #89).
    // `exec`'s thread was created in the folder's project, so it keeps it.
    let thread_project = if exec_args.is_some() {
        project_name.clone()
    } else {
        front::thread_project(&picked, &events, &project_name)
    };
    let role = front::role_for(&welcome.projects, &thread_project);
    // The title the status line shows from the first frame: a `Front`
    // reply carries one, a `--thread` resume has no reply, so the
    // thread's own events name it.
    let title = picked
        .info
        .as_ref()
        .and_then(front::title_of)
        .or_else(|| aigentic_runtime::title::title_of(&events));
    let project_name = thread_project.clone();
    if cli.mode != Mode::Manual {
        client
            .request(Request::SetMode {
                thread: thread_id,
                mode: cli.mode.name().to_owned(),
            })
            .await?;
    }
    let mode = if cli.mode != Mode::Manual {
        cli.mode.name().to_owned()
    } else {
        mode
    };

    if let Some(args) = exec_args {
        let notices = client.take_notices().context("notice stream")?;
        let outcome = exec::run(
            &client,
            notices,
            thread_id,
            &state,
            &args,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        )
        .await?;
        drop(embedded);
        std::process::exit(outcome.code);
    }

    let where_ = embedded.as_ref().map_or_else(
        || cli.server.clone().unwrap_or_default(),
        |_| "embedded daemon".into(),
    );
    let mode_part = if mode == "manual" {
        String::new()
    } else {
        format!(" · mode {mode}")
    };
    let running = match &state {
        ThreadState::Running { by, queued } => Some(match queued {
            0 => format!("a turn is running for {}", app::engine::author_name(by)),
            n => format!(
                "a turn is running for {}; {n} message(s) sent, reaching the agent at its next step",
                app::engine::author_name(by)
            ),
        }),
        _ => None,
    };
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        // At a terminal: the logo and three short lines.
        let mut lines = vec![format!(
            "{} · {} ({}) · project {project_name}{mode_part}{}",
            welcome.server,
            welcome.user,
            role.as_deref().unwrap_or("no role"),
            if embedded.is_some() {
                String::new()
            } else {
                format!(" · {where_}")
            }
        )];
        lines.push(front::front_line(
            picked.outcome.as_ref(),
            picked.info.as_ref(),
            thread_id,
            events.len(),
            false,
        ));
        if matches!(
            picked.outcome,
            Some(aigentic_api::FrontOutcome::Resumed) | None
        ) {
            for line in app::engine::recent_lines(&events, 3) {
                lines.push(format!("  {line}"));
            }
        }
        if let Some(r) = &running {
            lines.push(r.clone());
        }
        lines.push(
            "/ commands · @ files · shift-enter new line · esc interrupts · ctrl-t transcript"
                .into(),
        );
        print!("{}", app::look::welcome(&lines));
    } else {
        println!(
            "aigentic · {} · {where_} as {} ({}) · project {project_name}{mode_part}",
            welcome.server,
            welcome.user,
            role.as_deref().unwrap_or("no role"),
        );
        println!(
            "{}",
            front::front_line(
                picked.outcome.as_ref(),
                picked.info.as_ref(),
                thread_id,
                events.len(),
                true,
            )
        );
        if matches!(
            picked.outcome,
            Some(aigentic_api::FrontOutcome::Resumed) | None
        ) {
            for line in app::engine::recent_lines(&events, 3) {
                println!("  {line}");
            }
        }
        if let Some(r) = &running {
            println!("[{r}]");
        }
        println!(
            "/help lists commands; /project shows the layers; /who the participants; /quit exits"
        );
    }

    // User-invoked skills for slash dispatch: the daemon's skills report
    // lists them; the client keeps the names.
    let skills = match client
        .request(Request::Report {
            thread: thread_id,
            report: aigentic_api::ReportKind::Skills,
        })
        .await?
    {
        Response::Text { text } => text
            .lines()
            .filter_map(|l| l.strip_prefix('/'))
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };

    let notices = client.take_notices().context("notice stream")?;
    let repl = ClientRepl::new(
        client,
        thread_id,
        &welcome.user,
        role,
        state,
        mode,
        identity,
        &project_name,
    )
    .with_skills(skills)
    .with_title(title);
    app::run(repl, notices, history, project_name, project_root).await?;
    drop(embedded);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use ulid::Ulid;

    /// Issue #65: `aigentic --version` shows the package version and the
    /// commit the binary was built from, so a run can say which binary a
    /// thread actually used. The sha may be `unknown` off a checkout.
    #[test]
    fn version_names_the_package_and_the_commit() {
        let sha = VERSION
            .strip_prefix(env!("CARGO_PKG_VERSION"))
            .and_then(|rest| rest.strip_prefix(" ("))
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("not `version (sha)`: {VERSION}"));
        assert_eq!(sha.len(), 12, "a short sha is twelve characters: {sha}");
        assert!(
            sha == "unknown"
                || sha
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "neither a short sha nor `unknown`: {sha}"
        );

        let cli = Cli::try_parse_from(["aigentic", "--version"]).unwrap_err();
        assert_eq!(
            cli.kind(),
            clap::error::ErrorKind::DisplayVersion,
            "--version is clap's, not an argument error"
        );
        assert!(cli.to_string().contains(VERSION), "{}", cli);
    }

    /// Issue #31: `stats` takes its own `--since` and `--json`, and the
    /// global `--project` still reaches it.
    #[test]
    fn stats_parses_since_and_json() {
        let cli = Cli::try_parse_from(["aigentic", "stats", "--since", "7d", "--json"]).unwrap();
        match cli.command {
            Some(Command::Stats {
                since,
                json,
                assume_profile,
                issue,
                decisions,
            }) => {
                assert_eq!(since.as_deref(), Some("7d"));
                assert!(json);
                assert_eq!(assume_profile, None);
                assert_eq!(issue, None);
                assert!(!decisions);
            }
            other => panic!("expected stats: {other:?}"),
        }

        // Bare `stats` is every thread, text.
        let cli = Cli::try_parse_from(["aigentic", "stats"]).unwrap();
        match cli.command {
            Some(Command::Stats {
                since,
                json,
                assume_profile,
                issue,
                decisions,
            }) => {
                assert_eq!(since, None);
                assert!(!json);
                assert_eq!(assume_profile, None);
                assert_eq!(issue, None);
                assert!(!decisions);
            }
            other => panic!("expected stats: {other:?}"),
        }

        // `--project` is global, so it comes before or after the
        // subcommand and lands in the same field.
        let cli = Cli::try_parse_from(["aigentic", "stats", "--project", "alpha", "--since", "1d"])
            .unwrap();
        assert_eq!(cli.project.as_deref(), Some("alpha"));
        assert!(matches!(cli.command, Some(Command::Stats { .. })));
    }

    /// Issue #40: the drill-downs. A thread id is the *global* `--thread`
    /// (the same flag `exec` uses), `--issue` is the subcommand's, and
    /// asking for both is an error rather than a silent winner.
    #[test]
    fn stats_parses_the_drill_down_flags() {
        let id = Ulid::generate();
        let cli = Cli::try_parse_from(["aigentic", "stats", "--thread", &id.to_string()]).unwrap();
        assert_eq!(cli.thread, Some(id), "the global --thread must bind");
        assert!(matches!(cli.command, Some(Command::Stats { .. })));

        let cli = Cli::try_parse_from(["aigentic", "stats", "--issue", "40"]).unwrap();
        match cli.command {
            Some(Command::Stats { issue, .. }) => assert_eq!(issue, Some(40)),
            other => panic!("expected stats: {other:?}"),
        }
        assert_eq!(cli.thread, None);

        let clash = Cli::try_parse_from([
            "aigentic",
            "stats",
            "--thread",
            &id.to_string(),
            "--issue",
            "40",
        ])
        .unwrap_err();
        assert_eq!(clash.kind(), clap::error::ErrorKind::ArgumentConflict);

        // #40's estimate lever, parsed here and priced in `stats`.
        let cli =
            Cli::try_parse_from(["aigentic", "stats", "--assume-profile", "tensorx"]).unwrap();
        match cli.command {
            Some(Command::Stats { assume_profile, .. }) => {
                assert_eq!(assume_profile.as_deref(), Some("tensorx"));
            }
            other => panic!("expected stats: {other:?}"),
        }
    }

    /// Issue #74: `stats --decisions` parses, and asking for it together
    /// with a drill-down is an error rather than a silent winner.
    #[test]
    fn stats_parses_the_decisions_flag() {
        let cli = Cli::try_parse_from(["aigentic", "stats", "--decisions"]).unwrap();
        match cli.command {
            Some(Command::Stats {
                decisions, issue, ..
            }) => {
                assert!(decisions);
                assert_eq!(issue, None);
            }
            other => panic!("expected stats: {other:?}"),
        }

        let id = Ulid::generate();
        let id = id.to_string();
        let thread = vec!["aigentic", "stats", "--decisions", "--thread", &id];
        let issue = vec!["aigentic", "stats", "--decisions", "--issue", "74"];
        for args in [thread, issue] {
            let clash = Cli::try_parse_from(args).unwrap_err();
            assert_eq!(clash.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    /// Issue #37: `doctor` takes `--strict` (exit non-zero on a warning,
    /// unknown config keys included) and leaves the rows at `warn`;
    /// `--probe` and both flags together parse too.
    #[test]
    fn doctor_parses_strict_and_defaults_to_lenient() {
        let cli = Cli::try_parse_from(["aigentic", "doctor"]).unwrap();
        match cli.command {
            Some(Command::Doctor {
                probe,
                probe_effort,
                strict,
            }) => {
                assert!(!probe);
                assert!(probe_effort.is_none());
                assert!(!strict, "warnings pass unless --strict asks for more");
            }
            other => panic!("expected doctor: {other:?}"),
        }

        let cli = Cli::try_parse_from(["aigentic", "doctor", "--strict"]).unwrap();
        match cli.command {
            Some(Command::Doctor {
                probe,
                probe_effort,
                strict,
            }) => {
                assert!(!probe);
                assert!(probe_effort.is_none());
                assert!(strict);
            }
            other => panic!("expected doctor: {other:?}"),
        }

        let cli = Cli::try_parse_from(["aigentic", "doctor", "--probe", "--strict"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Doctor {
                probe: true,
                probe_effort: _,
                strict: true
            })
        ));

        // The comparison's pair is parsed off the flag unchanged.
        let cli = Cli::try_parse_from(["aigentic", "doctor", "--probe-effort", "1,100"]).unwrap();
        match cli.command {
            Some(Command::Doctor { probe_effort, .. }) => {
                assert_eq!(probe_effort.as_deref(), Some("1,100"));
            }
            other => panic!("expected doctor: {other:?}"),
        }
    }
}
