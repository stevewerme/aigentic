//! Building a thread's `Runtime` on the daemon: the project at a root,
//! the profile's provider, the layers, the skills, the policy. What the
//! terminal binary did in `main` through phase 4, so a client needs
//! nothing but a socket.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use aigentic_runtime::aigentic_core::{AgentId, Provider};
use aigentic_runtime::aigentic_log::{Repair, ThreadLog};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::{GlobalLayer, Layers, Project, ProjectFile, Runtime};
use ulid::Ulid;

use crate::config::{Config, ConfigError, Profile};
use crate::skills::{SkillPaths, load_enabled};

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

/// Open or create a thread's log under `root` and build its runtime.
pub async fn build_thread(
    config: &Config,
    config_dir: &Path,
    providers: &dyn ProviderFactory,
    root: &Root,
    thread: Ulid,
    profile_override: Option<&str>,
) -> Result<Built, BuildError> {
    // The project, when the root holds an aigentic.toml; else a bare
    // working directory with no layers, as the REPL outside a project.
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
    let profile: Option<&Profile> = config.profiles.get(&profile_name);

    let mut tools = ToolRegistry::builtin(Workdir::new(&root.root));
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
    let layers = Layers {
        global,
        project: opened.clone(),
    };

    let skill_paths = SkillPaths::new(&root.root, config_dir, config.bundled_dir.as_deref());
    let mut available = tools.names();
    available.extend(aigentic_runtime::harness_tools::harness_names());
    let available = layers.allowed_tools(&available);
    let enabled = layers.allowed_skills(&file.skills.enabled);
    let skills = load_enabled(&enabled, &skill_paths, &available)?;

    std::fs::create_dir_all(&root.threads_dir)?;
    let (log, torn) = ThreadLog::open_with(&root.threads_dir, thread, Repair::TruncateTornTail)?;

    let mut runtime = Runtime::new(provider, tools, log, AgentId("assistant".into()))
        .with_layers(layers)
        .with_model_label(&model)
        .with_policy(file.policy().with_root(&root.root, &root.root))
        .with_skills(skills);
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
