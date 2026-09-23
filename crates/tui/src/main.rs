//! `aigentic`: a plain streaming REPL over the runtime, and the `skills`
//! subcommands. No full-screen mode; the terminal keeps its scrollback.

mod app;
mod checks;
mod config;
mod doctor;
mod exec;
mod init_cmd;
mod pocock;
mod pocock_templates;
mod project_cmd;
mod skills_cmd;

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

use crate::app::engine::ClientRepl;
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
    /// List this project's threads, newest first.
    Threads,
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
    if let Some(Command::Doctor { probe }) = cli.command {
        let code = doctor::run(&config_path, &cwd, probe).await?;
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
            let threads = project_cmd::list_threads(&threads_dir)?;
            println!("{}", project_cmd::render_threads(&threads, &threads_dir));
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
                ToolRegistry::builtin(Workdir::new(&cwd)),
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
            Command::Doctor { .. } | Command::Init | Command::Serve { .. } | Command::Exec { .. },
        )
        | None => {}
    }

    // `exec` reads its prompt before anything connects, so a missing
    // prompt fails fast.
    let exec_args = match cli.command {
        Some(Command::Exec {
            prompt,
            json,
            output_last,
        }) => Some(exec::ExecArgs {
            prompt: exec::prompt_from(prompt)?,
            json,
            output_last,
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
    let project_name = cli
        .project
        .clone()
        .or_else(|| opened.as_ref().map(|p| p.name.clone()))
        .or_else(|| embedded.as_ref().map(|e| e.project.clone()))
        .or_else(|| welcome.projects.first().map(|p| p.name.clone()));
    let Some(project_name) = project_name else {
        bail!(
            "no project to work in: {} has no role anywhere on {}",
            welcome.user,
            welcome.server
        );
    };
    let role = welcome
        .projects
        .iter()
        .find(|p| p.name == project_name)
        .and_then(|p| p.role.clone());
    let thread_id = match cli.thread {
        Some(id) => id,
        None => match client
            .request(Request::CreateThread {
                project: project_name.clone(),
            })
            .await?
        {
            Response::Thread { thread } => thread.id,
            Response::Refused { reason } => {
                bail!("cannot start a thread in {project_name}: {reason}")
            }
            other => bail!("unexpected reply creating a thread: {other:?}"),
        },
    };
    let (state, events, mode) = match client
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
        } => (state, events, mode),
        Response::Refused { reason } => bail!("cannot open thread {thread_id}: {reason}"),
        other => bail!("unexpected reply opening the thread: {other:?}"),
    };
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
        ThreadState::Running { by, queued } => Some(format!(
            "a turn is running for {}; {queued} message(s) queued",
            app::engine::author_name(by)
        )),
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
        if cli.thread.is_some() {
            lines.push(format!("resumed {thread_id} · {} events", events.len()));
            for line in app::engine::recent_lines(&events, 3) {
                lines.push(format!("  {line}"));
            }
        } else {
            lines.push(format!(
                "new thread · resume later with --thread {thread_id}"
            ));
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
        if cli.thread.is_some() {
            println!("thread {thread_id} resumed with {} events", events.len());
            for line in app::engine::recent_lines(&events, 3) {
                println!("  {line}");
            }
        } else {
            println!("new thread {thread_id} (resume with --thread {thread_id})");
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
    let repl =
        ClientRepl::new(client, thread_id, &welcome.user, role, state, mode).with_skills(skills);
    app::run(repl, notices, history, project_name, project_root).await?;
    drop(embedded);
    Ok(())
}
