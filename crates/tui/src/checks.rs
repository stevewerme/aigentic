//! The checks `aigentic doctor` runs, one function each, so `aigentic init`
//! (plan section 10 d) can reuse them. Every check returns a `Check`; none
//! panics, and none touches the network except `check_probe`, which the
//! caller runs only behind `--probe`. No check prints, logs or returns an
//! API key: `check_api_key_env` names the variable and says whether it is
//! set, nothing more.

use std::path::Path;
use std::time::Instant;

use aigentic_runtime::aigentic_core::{
    Author, CompletionRequest, ContentBlock, Message, Provider, ProviderEvent, Role, UserId,
};
use aigentic_runtime::aigentic_tools::{ToolRegistry, Workdir};
use aigentic_runtime::project::{DOT_DIR, FILE_NAME, INSTRUCTIONS_FILE};
use aigentic_runtime::{GlobalLayer, Knowledge, KnowledgeMode, Layers, Project};
use futures_util::StreamExt;

use crate::config::{Config, Profile, ProviderKind};
use crate::skills_cmd::{SkillPaths, load_enabled};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Fail,
    Skip,
    /// Works, but likely not as meant; never fails the doctor.
    Warn,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Fail => "fail",
            Status::Skip => "skip",
            Status::Warn => "warn",
        }
    }
}

/// One line of the doctor's report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub message: String,
}

impl Check {
    fn ok(name: &str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Ok, message)
    }
    fn fail(name: &str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Fail, message)
    }
    fn skip(name: &str, message: impl Into<String>) -> Self {
        Self::new(name, Status::Skip, message)
    }
    fn new(name: &str, status: Status, message: impl Into<String>) -> Self {
        Self {
            name: name.to_owned(),
            status,
            message: message.into(),
        }
    }

    /// `ok    config       parsed /path`.
    pub fn render(&self) -> String {
        format!(
            "{:<5} {:<15} {}",
            self.status.word(),
            self.name,
            self.message
        )
    }
}

/// The `gh` binary behind a seam so tests script it. `args` are the
/// arguments after `gh`; `Ok` is stdout on success, `Err` the failure.
pub trait Gh {
    fn run(&self, args: &[&str]) -> anyhow::Result<String>;
}

/// The real binary.
pub struct GhCli;

impl Gh for GhCli {
    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = std::process::Command::new("gh")
            .args(args)
            .output()
            .map_err(|e| anyhow::anyhow!("running gh: {e}"))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let line = stderr.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            anyhow::bail!("gh {} failed: {}", args.join(" "), line.trim())
        }
    }
}

/// The config file parses. On success the config comes back for the
/// checks that need it.
pub fn check_config(path: &Path) -> (Check, Option<Config>) {
    match Config::load(path) {
        Ok(config) => (
            Check::ok(
                "config",
                format!(
                    "{} ({} profiles, default {})",
                    path.display(),
                    config.profiles.len(),
                    config.default_profile
                ),
            ),
            Some(config),
        ),
        Err(e) => {
            // Only the first line: `Config::load` appends the example file
            // when the config is missing, and the parser never echoes a
            // value.
            let first = e.to_string().lines().next().unwrap_or("").to_owned();
            (Check::fail("config", first), None)
        }
    }
}

/// The profile's key variable is set. The name is reported, never the value.
pub fn check_api_key_env(name: &str, profile: &Profile) -> Check {
    let var = &profile.api_key_env;
    let check_name = format!("key {name}");
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Check::ok(&check_name, format!("{var} is set")),
        _ => Check::fail(
            &check_name,
            format!("{var} is not set (put it in .env or export it)"),
        ),
    }
}

/// An OpenAI-compatible profile states its context window; without it
/// the provider assumes 32 768 tokens, which compaction, the knowledge
/// switch and the status line all measure against.
pub fn check_window(name: &str, profile: &Profile) -> Check {
    let check_name = format!("window {name}");
    match (profile.provider, profile.max_context_tokens) {
        (_, Some(n)) => Check::ok(&check_name, format!("{n} tokens")),
        (ProviderKind::OpenaiCompat, None) => Check::new(
            &check_name,
            Status::Warn,
            "max_context_tokens is not set, so 32768 is assumed; set it to the model's window (the provider's docs say)",
        ),
        (_, None) => Check::ok(&check_name, "the provider's default"),
    }
}

/// The threads directory exists or can be created.
pub fn check_threads_dir(dir: &Path) -> Check {
    if dir.is_dir() {
        return Check::ok("threads", format!("{} exists", dir.display()));
    }
    match std::fs::create_dir_all(dir) {
        Ok(()) => Check::ok("threads", format!("{} created", dir.display())),
        Err(e) => Check::fail("threads", format!("cannot create {}: {e}", dir.display())),
    }
}

/// The project at or above `cwd` opens; the knowledge folder loads and its
/// mode is decided for `provider`'s window. `skip` outside a project.
/// `[participants]` and the daemon's owner. An empty table gives the
/// owner `admin` alone; a table that names anyone gives the owner
/// nothing unless it names the owner too, which is the trap this line
/// is for. `owner` is the daemon's owner and `source` where that name
/// came from (`server.toml`'s first user, or `config.toml`'s `user` for
/// the embedded daemon).
pub fn check_participants(project: Option<&Project>, owner: &str, source: &str) -> Check {
    let Some(project) = project else {
        return Check::skip("participants", "no project");
    };
    let participants = &project.file.participants;
    if participants.is_empty() {
        return Check::ok(
            "participants",
            format!("none named; the daemon's owner {owner} ({source}) is admin alone"),
        );
    }
    match participants.listed(owner) {
        Some(role) => Check::ok(
            "participants",
            format!(
                "{}; the daemon's owner {owner} ({source}) is {role}",
                participants.describe().join(", ")
            ),
        ),
        None => Check::fail(
            "participants",
            format!(
                "{} named but not the daemon's owner {owner} ({source}): a table that names anyone gives the owner no role; add `{owner} = \"admin\"` to [participants]",
                participants.0.len()
            ),
        ),
    }
}

pub fn check_project(cwd: &Path, provider: &dyn Provider) -> (Check, Option<Project>) {
    let project = match Project::open(cwd) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return (
                Check::skip("project", format!("no {FILE_NAME} here or above")),
                None,
            );
        }
        Err(e) => return (Check::fail("project", e.to_string()), None),
    };
    let count = |text: &str| {
        provider.count_tokens(&[Message {
            role: Role::System,
            author: Author::System,
            blocks: vec![ContentBlock::Text(text.to_owned())],
        }])
    };
    let knowledge = match Knowledge::load(&project.knowledge_dir(), &count) {
        Ok(k) => k,
        Err(e) => return (Check::fail("project", format!("knowledge: {e}")), None),
    };
    let window = provider.capabilities().max_context_tokens;
    let mode = match knowledge.mode(window, project.file.knowledge.threshold_fraction) {
        KnowledgeMode::Inline => "inline",
        KnowledgeMode::Index => "index",
    };
    let memory = project
        .memory
        .iter()
        .filter(|(_, text)| !text.trim().is_empty())
        .count();
    let message = format!(
        "{} at {} · instructions {} · knowledge {mode}, {} files · memory {memory} files",
        project.name,
        project.root.display(),
        instructions_source(&project),
        knowledge.files.len()
    );
    (Check::ok("project", message), Some(project))
}

fn instructions_source(p: &Project) -> String {
    if p.dot_dir().join(INSTRUCTIONS_FILE).is_file() {
        format!("{DOT_DIR}/{INSTRUCTIONS_FILE}")
    } else if p.root.join("AGENTS.md").is_file() {
        "AGENTS.md".into()
    } else {
        "none".into()
    }
}

/// Every enabled skill loads against the lock with the tools the model
/// would see: the REPL's startup check, without starting. `skip` when the
/// project enables none.
pub fn check_skills(
    project: Option<&Project>,
    config: &Config,
    global_instructions: &Path,
    paths: &SkillPaths,
    cwd: &Path,
) -> Check {
    let Some(project) = project else {
        return Check::skip("skills", "no project");
    };
    if project.file.skills.enabled.is_empty() {
        return Check::skip("skills", "none enabled");
    }
    let global = match GlobalLayer::load(
        global_instructions,
        config.denied_tools.clone(),
        config.denied_skills.clone(),
    ) {
        Ok(g) => g,
        Err(e) => return Check::fail("skills", format!("global layer: {e}")),
    };
    let layers = Layers {
        global,
        workspace: None,
        project: Some(project.clone()),
    };
    let mut available = ToolRegistry::builtin(Workdir::new(cwd)).names();
    available.extend(aigentic_runtime::harness_tools::harness_names());
    let available = layers.allowed_tools(&available);
    let enabled = layers.allowed_skills(&project.file.skills.enabled);
    let denied = project.file.skills.enabled.len() - enabled.len();
    match load_enabled(&enabled, paths, &available) {
        Ok(set) => Check::ok(
            "skills",
            format!(
                "{} loaded{}",
                set.len(),
                if denied > 0 {
                    format!(", {denied} denied by global")
                } else {
                    String::new()
                }
            ),
        ),
        Err(e) => Check::fail("skills", e.to_string()),
    }
}

/// With `[pocock] issue_tracker = "github"`: `gh auth status` succeeds and
/// `remote` (the origin URL, `None` when there is none) is on GitHub.
/// `skip` for any other tracker or no project.
pub fn check_github(project: Option<&Project>, gh: &dyn Gh, remote: Option<&str>) -> Check {
    let tracker = project
        .and_then(|p| p.file.pocock.as_ref())
        .map(|p| p.issue_tracker.as_str());
    match tracker {
        Some("github") => {}
        Some(other) => return Check::skip("github", format!("issue_tracker = {other:?}")),
        None => return Check::skip("github", "no [pocock] section"),
    }
    if let Err(e) = gh.run(&["auth", "status"]) {
        return Check::fail("github", e.to_string());
    }
    match remote {
        Some(url) if is_github_remote(url) => {
            Check::ok("github", format!("gh authenticated, origin {url}"))
        }
        Some(url) => Check::fail("github", format!("origin {url} is not on GitHub")),
        None => Check::fail("github", "gh authenticated but no git remote named origin"),
    }
}

pub fn is_github_remote(url: &str) -> bool {
    let url = url.trim();
    url.starts_with("git@github.com:")
        || url.starts_with("https://github.com/")
        || url.starts_with("ssh://git@github.com/")
        || url.starts_with("git://github.com/")
}

/// `git config --get remote.origin.url` at `root`, `None` when unset or
/// outside a repository.
pub fn origin_url(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!url.is_empty()).then_some(url)
}

/// One completion with `max_output_tokens = 1`: the endpoint answers for
/// this profile. The only check that uses the network. The key is taken
/// from the environment and handed to the adapter; it never appears in
/// the report.
pub async fn check_probe(name: &str, profile: &Profile) -> Check {
    let check_name = format!("probe {name}");
    let key = match profile.api_key() {
        Ok(k) => k,
        Err(_) => {
            return Check::skip(&check_name, format!("{} is not set", profile.api_key_env));
        }
    };
    let provider = profile.build_provider(key);
    let messages = [Message {
        role: Role::User,
        author: Author::User(UserId("doctor".into())),
        blocks: vec![ContentBlock::Text("Reply with one word.".into())],
    }];
    let request = CompletionRequest {
        messages: &messages,
        tools: &[],
        max_output_tokens: Some(1),
    };
    let started = Instant::now();
    let mut stream = provider.complete(&request);
    let mut finish = None;
    while let Some(event) = stream.next().await {
        match event {
            ProviderEvent::Done { finish_reason } => finish = Some(finish_reason),
            ProviderEvent::Error(e) => {
                return Check::fail(&check_name, format!("{} · {e}", profile.model));
            }
            _ => {}
        }
    }
    let ms = started.elapsed().as_millis();
    match finish {
        Some(reason) => Check::ok(
            &check_name,
            format!("{} answered in {ms} ms (finish {reason})", profile.model),
        ),
        None => Check::fail(
            &check_name,
            format!(
                "{} · stream ended without a done event after {ms} ms",
                profile.model
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aigentic_runtime::aigentic_core::Capabilities;
    use futures_core::Stream;
    use std::pin::Pin;

    struct Window(u64);
    impl Provider for Window {
        fn complete(
            &self,
            _: &CompletionRequest<'_>,
        ) -> Pin<Box<dyn Stream<Item = ProviderEvent> + Send + '_>> {
            Box::pin(futures_util::stream::empty())
        }
        fn count_tokens(&self, m: &[Message]) -> u64 {
            m.iter()
                .flat_map(|m| &m.blocks)
                .map(|b| match b {
                    ContentBlock::Text(t) => t.len() as u64,
                    _ => 0,
                })
                .sum()
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                supports_tools: true,
                supports_images: false,
                supports_caching: false,
                supports_structured_output: false,
                max_context_tokens: self.0,
            }
        }
    }

    struct ScriptedGh {
        auth: Result<String, String>,
    }
    impl Gh for ScriptedGh {
        fn run(&self, args: &[&str]) -> anyhow::Result<String> {
            assert_eq!(args, ["auth", "status"]);
            self.auth
                .clone()
                .map_err(|e| anyhow::anyhow!("gh auth status failed: {e}"))
        }
    }

    const GOOD: &str = "[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"AIGENTIC_DOCTOR_TEST_KEY\"\n";

    #[test]
    fn config_parses_or_fails_without_echoing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, GOOD).unwrap();
        let (c, config) = check_config(&path);
        assert_eq!(c.status, Status::Ok);
        assert!(config.is_some());
        assert!(c.message.contains("1 profiles, default a"), "{}", c.message);
        assert_eq!(c.render(), format!("ok    config          {}", c.message));

        let (c, config) = check_config(&dir.path().join("missing.toml"));
        assert_eq!(c.status, Status::Fail);
        assert!(config.is_none());
        assert_eq!(c.message.lines().count(), 1, "{}", c.message);

        std::fs::write(&path, format!("{GOOD}api_key = \"sk-oops\"\n")).unwrap();
        let (c, _) = check_config(&path);
        assert_eq!(c.status, Status::Fail);
        assert!(!c.message.contains("sk-oops"), "{}", c.message);
    }

    #[test]
    fn key_check_names_the_variable_never_the_value() {
        let config = Config::parse(GOOD).unwrap();
        let profile = config.profiles["a"].clone();
        // Unset by construction in a fresh process.
        let c = check_api_key_env("a", &profile);
        assert_eq!(c.status, Status::Fail);
        assert_eq!(c.name, "key a");
        assert!(c.message.starts_with("AIGENTIC_DOCTOR_TEST_KEY is not set"));

        let mut set = profile.clone();
        set.api_key_env = "PATH".into();
        let c = check_api_key_env("a", &set);
        assert_eq!(c.status, Status::Ok);
        assert_eq!(c.message, "PATH is set");
        assert!(!c.message.contains(&std::env::var("PATH").unwrap()));
    }

    #[test]
    fn threads_dir_exists_or_is_created_or_fails() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_threads_dir(dir.path());
        assert_eq!(c.status, Status::Ok);
        assert!(c.message.ends_with("exists"));
        let fresh = dir.path().join("a/b");
        let c = check_threads_dir(&fresh);
        assert_eq!(c.status, Status::Ok);
        assert!(c.message.ends_with("created"));
        assert!(fresh.is_dir());
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let c = check_threads_dir(&file.join("under"));
        assert_eq!(c.status, Status::Fail);
    }

    #[test]
    fn participants_check_wants_the_owner_listed_once_anyone_is() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            check_participants(None, "steve", "server.toml").status,
            Status::Skip
        );
        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"p\"\n").unwrap();
        let open = || Project::open(dir.path()).unwrap().unwrap();
        let c = check_participants(Some(&open()), "steve", "server.toml");
        assert_eq!(c.status, Status::Ok);
        assert!(c.message.contains("admin alone"), "{}", c.message);

        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[participants]\nmagnus = \"approve\"\n",
        )
        .unwrap();
        let c = check_participants(Some(&open()), "steve", "config.toml's user");
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.message
                .contains("not the daemon's owner steve (config.toml's user)")
                && c.message.contains("steve = \"admin\""),
            "{}",
            c.message
        );

        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[participants]\nsteve = \"admin\"\nmagnus = \"approve\"\n",
        )
        .unwrap();
        let c = check_participants(Some(&open()), "steve", "server.toml");
        assert_eq!(c.status, Status::Ok);
        assert_eq!(
            c.message,
            "magnus (approve), steve (admin); the daemon's owner steve (server.toml) is admin"
        );
    }

    #[test]
    fn project_check_reports_layers_or_skips_or_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (c, p) = check_project(dir.path(), &Window(1000));
        assert_eq!(c.status, Status::Skip);
        assert!(p.is_none());

        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"p\"\n").unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# rules").unwrap();
        let know = dir.path().join(".aigentic/knowledge");
        let mem = dir.path().join(".aigentic/memory");
        std::fs::create_dir_all(&know).unwrap();
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(know.join("a.md"), "# A\n\n".to_owned() + &"x".repeat(500)).unwrap();
        std::fs::write(mem.join("decisions.md"), "- a\n").unwrap();
        std::fs::write(mem.join("empty.md"), "\n").unwrap();
        let (c, p) = check_project(dir.path(), &Window(10_000));
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        assert!(p.is_some());
        assert!(c.message.starts_with("p at "), "{}", c.message);
        assert!(
            c.message.contains("instructions AGENTS.md"),
            "{}",
            c.message
        );
        assert!(
            c.message.contains("knowledge inline, 1 files"),
            "{}",
            c.message
        );
        assert!(c.message.contains("memory 1 files"), "{}", c.message);
        // A small window flips the mode.
        let (c, _) = check_project(dir.path(), &Window(100));
        assert!(
            c.message.contains("knowledge index, 1 files"),
            "{}",
            c.message
        );

        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"\"\n").unwrap();
        let (c, p) = check_project(dir.path(), &Window(1000));
        assert_eq!(c.status, Status::Fail);
        assert!(p.is_none());
    }

    #[test]
    fn skills_check_loads_against_the_lock_or_skips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SkillPaths {
            project: dir.path().to_path_buf(),
            user: dir.path().join("u"),
            bundled: dir.path().join("b"),
        };
        let config = Config::parse(GOOD).unwrap();
        let global = dir.path().join("instructions.md");
        assert_eq!(
            check_skills(None, &config, &global, &paths, dir.path()).status,
            Status::Skip
        );

        std::fs::write(dir.path().join(FILE_NAME), "[project]\nname = \"p\"\n").unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        let c = check_skills(Some(&p), &config, &global, &paths, dir.path());
        assert_eq!(c.status, Status::Skip);
        assert_eq!(c.message, "none enabled");

        // An enabled skill that no root holds fails, naming it.
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[skills]\nenabled = [\"ghost\"]\n",
        )
        .unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        let c = check_skills(Some(&p), &config, &global, &paths, dir.path());
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("ghost"), "{}", c.message);

        // A locked skill in the project root loads.
        let skill = dir.path().join("skills/hello");
        std::fs::create_dir_all(&skill).unwrap();
        let manifest = "---\nname: hello\ndescription: says hello\n---\nSay hello.\n";
        std::fs::write(skill.join("SKILL.md"), manifest).unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[skills]\nenabled = [\"hello\"]\n",
        )
        .unwrap();
        let mut lock = aigentic_runtime::aigentic_skills::Lockfile::default();
        aigentic_runtime::aigentic_skills::lock_root(
            &dir.path().join("skills"),
            "skills",
            "local",
            "0",
            &mut lock,
        )
        .unwrap();
        lock.save(&dir.path().join(crate::skills_cmd::LOCK_FILE))
            .unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        let c = check_skills(Some(&p), &config, &global, &paths, dir.path());
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        assert_eq!(c.message, "1 loaded");

        // A global denial hides it: zero loaded, one denied.
        let mut denied = config.clone();
        denied.denied_skills = vec!["hello".into()];
        let c = check_skills(Some(&p), &denied, &global, &paths, dir.path());
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        assert_eq!(c.message, "0 loaded, 1 denied by global");
    }

    #[test]
    fn github_check_needs_auth_and_a_github_remote() {
        let dir = tempfile::tempdir().unwrap();
        let ok = ScriptedGh {
            auth: Ok("Logged in".into()),
        };
        let bad = ScriptedGh {
            auth: Err("not logged in".into()),
        };
        assert_eq!(check_github(None, &ok, None).status, Status::Skip);

        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[pocock]\nissue_tracker = \"local\"\n",
        )
        .unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        let c = check_github(Some(&p), &ok, Some("git@github.com:x/y.git"));
        assert_eq!(c.status, Status::Skip);
        assert!(c.message.contains("local"));

        std::fs::write(
            dir.path().join(FILE_NAME),
            "[project]\nname = \"p\"\n[pocock]\nissue_tracker = \"github\"\n",
        )
        .unwrap();
        let p = Project::open_root(dir.path()).unwrap();
        let c = check_github(Some(&p), &ok, Some("git@github.com:x/y.git"));
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        let c = check_github(Some(&p), &ok, Some("https://github.com/x/y"));
        assert_eq!(c.status, Status::Ok, "{}", c.message);
        let c = check_github(Some(&p), &ok, Some("https://gitlab.com/x/y.git"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("not on GitHub"));
        let c = check_github(Some(&p), &ok, None);
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("no git remote"));
        let c = check_github(Some(&p), &bad, Some("git@github.com:x/y.git"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("not logged in"), "{}", c.message);
    }

    #[tokio::test]
    async fn probe_skips_without_a_key_and_never_touches_the_network() {
        let config = Config::parse(GOOD).unwrap();
        let c = check_probe("a", &config.profiles["a"]).await;
        assert_eq!(c.status, Status::Skip);
        assert!(c.message.contains("AIGENTIC_DOCTOR_TEST_KEY"));
    }

    #[test]
    fn a_compat_profile_without_a_window_warns() {
        let c = Config::parse(
            "default_profile = \"a\"\n[profiles.a]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\n[profiles.b]\nbase_url = \"u\"\nmodel = \"m\"\napi_key_env = \"K\"\nmax_context_tokens = 1048576\n",
        )
        .unwrap();
        let a = check_window("a", &c.profiles["a"]);
        assert_eq!(a.status, Status::Warn);
        assert!(a.message.contains("32768"), "{}", a.message);
        let b = check_window("b", &c.profiles["b"]);
        assert_eq!(b.status, Status::Ok);
        assert_eq!(b.message, "1048576 tokens");
    }
}
