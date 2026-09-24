//! Building a thread's `Runtime` on the daemon: the project at a root,
//! the profile's provider, the layers, the skills, the policy. What the
//! terminal binary did in `main` through phase 4, so a client needs
//! nothing but a socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aigentic_runtime::aigentic_core::{AgentId, Provider};
use aigentic_runtime::aigentic_log::{Repair, ThreadLog};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{
    GlobalLayer, Layers, Project, ProjectContext, ProjectFile, Runtime, WorkspaceLayer,
};
use ulid::Ulid;

use crate::config::{Config, ConfigError, Profile};
use crate::skills::{SkillPaths, load_enabled};
use crate::workspaces::{self, Workspace};

/// Where a thread's provider comes from. The daemon builds it from the
/// profile with the key from its own environment; tests script one.
pub trait ProviderFactory: Send + Sync {
    /// The provider for `profile_name`, or why not (a missing key, an
    /// unknown profile). The error text never holds a key.
    fn build(&self, profile_name: &str) -> Result<(Box<dyn Provider>, String), BuildError>;
}

/// The default: `config.toml`'s profiles.
pub struct Profiles(pub Arc<Config>);

impl ProviderFactory for Profiles {
    fn build(&self, profile_name: &str) -> Result<(Box<dyn Provider>, String), BuildError> {
        let (_, profile) = self
            .0
            .select(Some(profile_name))
            .map_err(|e| BuildError::Config(e.to_string()))?;
        let key = profile
            .api_key()
            .map_err(|e| BuildError::Config(e.to_string()))?;
        Ok((profile.build_provider(key), profile.model.clone()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("{0}")]
    Config(String),
    #[error("project: {0}")]
    Project(#[from] aigentic_runtime::ProjectError),
    #[error("log: {0}")]
    Log(#[from] aigentic_runtime::aigentic_log::LogError),
    #[error("skills: {0}")]
    Skills(#[from] aigentic_runtime::aigentic_skills::SkillError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<ConfigError> for BuildError {
    fn from(e: ConfigError) -> Self {
        BuildError::Config(e.to_string())
    }
}

/// What a built thread carries besides its runtime.
pub struct Built {
    pub runtime: Runtime,
    /// Bytes a torn last line lost, for the actor's resume.
    pub torn: Option<u64>,
    pub profile: String,
    /// MCP servers that failed to connect, with why; the thread runs on.
    pub mcp_skipped: Vec<(String, String)>,
}

/// The daemon's view of one project root: the checkout and where its
/// threads live.
pub struct Root {
    pub name: String,
    pub root: PathBuf,
    pub threads_dir: PathBuf,
}

/// Everything a thread takes from the project at `root`: the layers
/// (global, the workspace's when the root is in one, the project's), the
/// profile's provider, tools rooted there with its MCP servers, the
/// skills and the policy. For a new or reloaded thread (`build_thread`)
/// and for a switch (`Runtime::set_project`).
pub struct Context {
    pub ctx: ProjectContext,
    pub profile: String,
    pub mcp_skipped: Vec<(String, String)>,
}

pub async fn project_context(
    config: &Config,
    config_dir: &Path,
    providers: &dyn ProviderFactory,
    root: &Root,
    workspaces: &[Workspace],
    profile_override: Option<&str>,
) -> Result<Context, BuildError> {
    // The project, when the root holds an aigentic.toml; else a bare
    // working directory with no project layer.
    let opened: Option<Project> = if root
        .root
        .join(aigentic_runtime::project::FILE_NAME)
        .is_file()
    {
        Some(Project::open_root(&root.root)?)
    } else {
        None
    };
    let file: ProjectFile = opened
        .as_ref()
        .map_or_else(ProjectFile::default, |p| p.file.clone());
    let profile_name = profile_override
        .map(str::to_owned)
        .or_else(|| file.model.as_ref().map(|m| m.profile.clone()))
        .unwrap_or_else(|| config.default_profile.clone());
    let (provider, model) = providers.build(&profile_name)?;

    let mut tools = ToolRegistry::builtin(Workdir::new(&root.root), file.tools.bash_timeout());
    let mut mcp_skipped = Vec::new();
    for server in &file.mcp_servers {
        if let Err(e) = tools.connect_mcp(server).await {
            mcp_skipped.push((server.name.clone(), e.to_string()));
        }
    }

    let global_instructions = config
        .global_instructions
        .clone()
        .unwrap_or_else(|| config_dir.join("instructions.md"));
    let global = GlobalLayer::load(
        &global_instructions,
        config.denied_tools.clone(),
        config.denied_skills.clone(),
    )?;
    let workspace = match workspaces::workspace_of(workspaces, &root.root) {
        Some(w) => Some(WorkspaceLayer::load(&w.name, w.shared.as_deref())?),
        None => None,
    };
    let layers = Layers {
        global,
        workspace: workspace.clone(),
        project: opened.clone(),
    };

    let skill_paths = SkillPaths::new(&root.root, config_dir, config.bundled_dir.as_deref());
    // `p` answers land in the project's rules file, or the personal one
    // when the root has no project file.
    let rules_file = if opened.is_some() {
        root.root.join(".aigentic").join("rules.toml")
    } else {
        config_dir.join("rules.toml")
    };
    let mut available = tools.names();
    available.extend(aigentic_runtime::harness_tools::harness_names());
    let available = layers.allowed_tools(&available);
    let enabled = layers.allowed_skills(&file.skills.enabled);
    let skills = load_enabled(&enabled, &skill_paths, &available)?;
    let policy = file
        .policy()
        .with_root(&root.root, &root.root)
        .with_rules_file(&rules_file);
    Ok(Context {
        ctx: ProjectContext {
            name: opened.as_ref().map(|p| p.name.clone()),
            workspace: workspace.map(|w| w.name),
            root: root.root.clone(),
            layers,
            policy,
            skills,
            registry: tools,
            provider,
            model_label: model,
        },
        profile: profile_name,
        mcp_skipped,
    })
}

/// Open or create a thread's log in `root.threads_dir` and build its
/// runtime from the project at `root.root`.
pub async fn build_thread(
    config: &Config,
    config_dir: &Path,
    providers: &dyn ProviderFactory,
    root: &Root,
    workspaces: &[Workspace],
    thread: Ulid,
    profile_override: Option<&str>,
) -> Result<Built, BuildError> {
    let Context {
        ctx,
        profile: profile_name,
        mcp_skipped,
    } = project_context(
        config,
        config_dir,
        providers,
        root,
        workspaces,
        profile_override,
    )
    .await?;
    let profile: Option<&Profile> = config.profiles.get(&profile_name);

    std::fs::create_dir_all(&root.threads_dir)?;
    let (log, torn) = ThreadLog::open_with(&root.threads_dir, thread, Repair::TruncateTornTail)?;

    let mut runtime = Runtime::new(ctx.provider, ctx.registry, log, AgentId("assistant".into()))
        .with_layers(ctx.layers)
        .with_model_label(&ctx.model_label)
        .with_policy(ctx.policy)
        .with_skills(ctx.skills)
        .with_harness_instructions();
    if let Some(utility) = &config.utility_profile
        && *utility != profile_name
    {
        // A missing key for the utility profile is not fatal: the
        // thread's own model does the side jobs instead.
        if let Ok((provider, _)) = providers.build(utility) {
            runtime = runtime.with_utility(provider);
        }
    }
    if let Some(p) = profile {
        runtime = runtime
            .with_compaction(p.compaction_settings())
            .with_budget(p.budget());
    }
    Ok(Built {
        runtime,
        torn,
        profile: profile_name,
        mcp_skipped,
    })
}
