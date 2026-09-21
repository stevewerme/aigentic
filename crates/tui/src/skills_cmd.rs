//! `aigentic skills list | check | vendor | update`, and the skill roots
//! and lock the REPL loads from. Network only in `vendor` and `update`,
//! through the `git` binary, never at thread time.

use std::path::{Path, PathBuf};
use std::process::Command;

use aigentic_runtime::aigentic_skills::{
    Invocation, LockEntry, Lockfile, Manifest, Origin, Review, Roots, SkillError, blocking, check,
    discover_roots, hash_manifest, lock_root, render_review, walk_root,
};
use anyhow::{Context, bail};

pub const LOCK_FILE: &str = "skills.lock.toml";

/// Where skills and their locks live for one run.
#[derive(Debug, Clone)]
pub struct SkillPaths {
    /// The working directory: `./skills` and `./skills.lock.toml`.
    pub project: PathBuf,
    /// `~/.config/aigentic`: `skills/` and `skills.lock.toml` there.
    pub user: PathBuf,
    /// The repository the binary was built from, or `bundled_dir` in the
    /// config: `skills/` and `skills.lock.toml` there.
    pub bundled: PathBuf,
}

impl SkillPaths {
    pub fn new(cwd: &Path, config_dir: &Path, bundled: Option<&Path>) -> Self {
        Self {
            project: cwd.to_path_buf(),
            user: config_dir.to_path_buf(),
            bundled: bundled.map_or_else(default_bundled_dir, Path::to_path_buf),
        }
    }

    pub fn roots(&self) -> Roots {
        Roots {
            project: Some(self.project.join("skills")),
            user: Some(self.user.join("skills")),
            bundled: Some(self.bundled.join("skills")),
        }
    }

    /// The three lockfiles merged, closer winning by name. A missing file
    /// is empty.
    pub fn lock(&self) -> anyhow::Result<Lockfile> {
        let mut merged = Lockfile::default();
        for dir in [&self.bundled, &self.user, &self.project] {
            let path = dir.join(LOCK_FILE);
            if path.exists() {
                let lock = Lockfile::load(&path)?;
                for entry in lock.skills {
                    merged.upsert(entry);
                }
            }
        }
        Ok(merged)
    }

    fn dir_for(&self, origin: Origin) -> &Path {
        match origin {
            Origin::Project => &self.project,
            Origin::User => &self.user,
            Origin::Bundled => &self.bundled,
        }
    }
}

/// The repository this binary was built from.
pub fn default_bundled_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

#[derive(Debug, clap::Subcommand)]
pub enum SkillsCommand {
    /// Every skill the roots resolve to, with where it came from.
    List,
    /// Verify hashes and run the static check; non-zero while a pending
    /// skill has findings.
    Check,
    /// Clone a repository at a commit and copy its skills in, with lock
    /// entries marked pending.
    Vendor {
        /// `<git-url>@<commit>`
        source: String,
        /// `bundled` (default), `user` or `project`.
        #[arg(long, default_value = "bundled")]
        into: String,
    },
    /// Re-fetch each recorded source at its head, show a diff per changed
    /// skill, and apply what you accept.
    Update,
}

pub fn run(command: SkillsCommand, paths: &SkillPaths) -> anyhow::Result<i32> {
    match command {
        SkillsCommand::List => list(paths),
        SkillsCommand::Check => check_all(paths),
        SkillsCommand::Vendor { source, into } => vendor(paths, &source, &into),
        SkillsCommand::Update => update(paths),
    }
}

fn origin_name(origin: Origin) -> &'static str {
    match origin {
        Origin::Project => "project",
        Origin::User => "user",
        Origin::Bundled => "bundled",
    }
}

fn invocation_name(i: Invocation) -> &'static str {
    match i {
        Invocation::User => "user",
        Invocation::Model => "model",
    }
}

fn list(paths: &SkillPaths) -> anyhow::Result<i32> {
    let lock = paths.lock()?;
    let manifests = discover_roots(&paths.roots())?;
    if manifests.is_empty() {
        println!("no skills found under {:?}", paths.roots());
        return Ok(0);
    }
    println!(
        "{:<28} {:<6} {:<9} {:<22} source",
        "name", "invoke", "from", "review"
    );
    for m in &manifests {
        let (review, source) = match lock.get(&m.name) {
            Some(e) => (
                match &e.review {
                    Review::Pending => "pending".to_owned(),
                    Review::Accepted { by, on } => format!("accepted {by} {on}"),
                    Review::Rejected { by, on } => format!("rejected {by} {on}"),
                },
                format!("{}@{}", e.source, e.commit.get(..7).unwrap_or(&e.commit)),
            ),
            None => ("unlocked".into(), "-".into()),
        };
        println!(
            "{:<28} {:<6} {:<9} {:<22} {source}",
            m.name,
            invocation_name(m.invocation),
            origin_name(m.origin),
            review
        );
    }
    Ok(0)
}

fn check_all(paths: &SkillPaths) -> anyhow::Result<i32> {
    let lock = paths.lock()?;
    let manifests = discover_roots(&paths.roots())?;
    let mut failed = 0;
    for m in &manifests {
        match lock.get(&m.name) {
            None => {
                println!(
                    "{}: not in any lockfile (run `aigentic skills vendor`)",
                    m.name
                );
                failed += 1;
            }
            Some(entry) => {
                if let Err(e) = entry.verify(m) {
                    println!("{e}");
                    failed += 1;
                }
            }
        }
    }
    let blocked = blocking(&manifests, &lock);
    let mut total = 0;
    for m in &manifests {
        let findings = check(m);
        total += findings.len();
        for f in findings {
            println!(
                "{}: {}:{} {:?} {}",
                m.name,
                f.file.display(),
                f.line,
                f.kind,
                f.text
            );
        }
    }
    println!(
        "{} skills, {failed} hash or lock failures, {total} findings, {} pending with findings",
        manifests.len(),
        blocked.len()
    );
    if !blocked.is_empty() {
        println!(
            "review needed (set `review = {{ by, on }}` in the lock): {}",
            blocked.join(", ")
        );
    }
    Ok(if failed > 0 || !blocked.is_empty() {
        1
    } else {
        0
    })
}

/// `<git-url>@<commit>` split; the commit is required so a vendor is
/// reproducible.
pub fn split_source(source: &str) -> anyhow::Result<(&str, &str)> {
    let Some((url, commit)) = source.rsplit_once('@') else {
        bail!("expected <git-url>@<commit>, got {source}");
    };
    if url.is_empty() || commit.is_empty() || url.ends_with(':') {
        bail!("expected <git-url>@<commit>, got {source}");
    }
    Ok((url, commit))
}

/// The last path segment of a git URL without `.git`.
pub fn repo_name(url: &str) -> String {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let name = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed)
        .to_owned();
    if name.is_empty() {
        "skills".into()
    } else {
        name
    }
}

/// The directory holding the skills in a checkout: `skills/` when it
/// exists, else the root.
fn skills_dir_in(checkout: &Path) -> PathBuf {
    let sub = checkout.join("skills");
    if sub.is_dir() {
        sub
    } else {
        checkout.to_path_buf()
    }
}

fn git_clone(url: &str, commit: &str, into: &Path) -> anyhow::Result<()> {
    let status = Command::new("git")
        .args(["clone", "--quiet", url])
        .arg(into)
        .status()
        .context("running git clone")?;
    if !status.success() {
        bail!("git clone {url} failed");
    }
    let status = Command::new("git")
        .args(["-C"])
        .arg(into)
        .args(["checkout", "--quiet", commit])
        .status()
        .context("running git checkout")?;
    if !status.success() {
        bail!("git checkout {commit} failed");
    }
    Ok(())
}

fn git_head(checkout: &Path) -> anyhow::Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("running git rev-parse")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

fn vendor(paths: &SkillPaths, source: &str, into: &str) -> anyhow::Result<i32> {
    let (url, commit) = split_source(source)?;
    let dest = match into {
        "bundled" => &paths.bundled,
        "user" => &paths.user,
        "project" => &paths.project,
        other => bail!("--into must be bundled, user or project, got {other}"),
    };
    let name = repo_name(url);
    let tmp = tempfile::tempdir()?;
    let checkout = tmp.path().join("checkout");
    git_clone(url, commit, &checkout)?;
    let commit = git_head(&checkout)?;
    let from = skills_dir_in(&checkout);
    let target = dest.join("skills").join(&name);
    if target.exists() {
        bail!(
            "{} exists; use `aigentic skills update` or remove it first",
            target.display()
        );
    }
    copy_dir(&from, &target)?;
    if let Ok(license) = std::fs::read(checkout.join("LICENSE")) {
        std::fs::write(target.join("LICENSE"), license)?;
    }
    let lock_path = dest.join(LOCK_FILE);
    let mut lock = if lock_path.exists() {
        Lockfile::load(&lock_path)?
    } else {
        Lockfile::default()
    };
    let manifests = lock_root(&target, &format!("skills/{name}"), url, &commit, &mut lock)?;
    lock.save(&lock_path)?;
    println!(
        "vendored {} skills from {url}@{} into {}; lock entries written as pending to {}",
        manifests.len(),
        commit.get(..7).unwrap_or(&commit),
        target.display(),
        lock_path.display()
    );
    print!("{}", render_review(&manifests, &lock));
    Ok(0)
}

fn update(paths: &SkillPaths) -> anyhow::Result<i32> {
    let manifests = discover_roots(&paths.roots())?;
    let mut sources: Vec<(String, Origin)> = Vec::new();
    for origin in [Origin::Bundled, Origin::User, Origin::Project] {
        let lock_path = paths.dir_for(origin).join(LOCK_FILE);
        if !lock_path.exists() {
            continue;
        }
        for e in Lockfile::load(&lock_path)?.skills {
            if e.source.contains("://") || e.source.contains('@') {
                let key = (e.source.clone(), origin);
                if !sources.contains(&key) {
                    sources.push(key);
                }
            }
        }
    }
    if sources.is_empty() {
        println!("no remote sources in any lockfile");
        return Ok(0);
    }
    let mut changed_total = 0;
    for (url, origin) in sources {
        let dir = paths.dir_for(origin);
        let lock_path = dir.join(LOCK_FILE);
        let mut lock = Lockfile::load(&lock_path)?;
        let tmp = tempfile::tempdir()?;
        let checkout = tmp.path().join("checkout");
        git_clone(&url, "HEAD", &checkout)?;
        let head = git_head(&checkout)?;
        let fresh = walk_root(&skills_dir_in(&checkout), Origin::Bundled)?;
        println!("{url}: head {}", head.get(..7).unwrap_or(&head));
        for new in &fresh {
            let Some(entry) = lock.get(&new.name).cloned() else {
                continue; // not vendored from here
            };
            if entry.source != url {
                continue;
            }
            let Some(current) = manifests.iter().find(|m| m.name == new.name) else {
                continue;
            };
            let (sha, files) = hash_manifest(new)?;
            if sha == entry.sha256 && files == entry.files {
                continue;
            }
            changed_total += 1;
            println!("\n== {} changed ==", new.name);
            show_diff(&current.path, &new.path);
            let findings = check(new);
            println!(
                "static check on the new version: {} findings",
                findings.len()
            );
            for f in &findings {
                println!("  {}:{} {:?} {}", f.file.display(), f.line, f.kind, f.text);
            }
            if !confirm(&format!("apply the update to {}?", new.name))? {
                println!("skipped {}", new.name);
                continue;
            }
            let target = dir.join(&entry.path);
            std::fs::remove_dir_all(&target)?;
            copy_dir(&new.path, &target)?;
            let applied = Manifest::parse(&target, origin)?;
            let mut fresh_entry =
                LockEntry::from_manifest(&applied, entry.path.clone(), &url, &head)?;
            fresh_entry.requires = entry.requires.clone();
            fresh_entry.review = Review::Pending;
            lock.upsert(fresh_entry);
            lock.save(&lock_path)?;
            println!("applied {}; review is pending again", new.name);
        }
    }
    if changed_total == 0 {
        println!("everything is up to date");
    }
    Ok(0)
}

fn show_diff(current: &Path, new: &Path) {
    match Command::new("diff")
        .args(["-ru"])
        .arg(current)
        .arg(new)
        .output()
    {
        Ok(out) => print!("{}", String::from_utf8_lossy(&out.stdout)),
        Err(e) => println!("(diff unavailable: {e})"),
    }
}

fn confirm(question: &str) -> anyhow::Result<bool> {
    use std::io::{BufRead, Write};
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Load the enabled set for the REPL. A hash mismatch or an unlocked
/// skill refuses, naming the skill.
pub fn load_enabled(
    enabled: &[String],
    paths: &SkillPaths,
    tools: &[String],
) -> Result<aigentic_runtime::aigentic_skills::SkillSet, anyhow::Error> {
    let lock = paths.lock()?;
    aigentic_runtime::aigentic_skills::SkillSet::load(enabled, &paths.roots(), &lock, tools)
        .map_err(|e: SkillError| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_splits_and_names() {
        assert_eq!(
            split_source("https://github.com/mattpocock/skills@c55ee46").unwrap(),
            ("https://github.com/mattpocock/skills", "c55ee46")
        );
        assert_eq!(
            split_source("git@github.com:x/y.git@abc").unwrap(),
            ("git@github.com:x/y.git", "abc")
        );
        assert!(split_source("https://github.com/x/y").is_err());
        assert_eq!(repo_name("https://github.com/mattpocock/skills"), "skills");
        assert_eq!(repo_name("git@github.com:x/y.git"), "y");
    }

    #[test]
    fn locks_merge_closer_wins() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = dir.path().join("b");
        let project = dir.path().join("p");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let entry = |commit: &str| {
            format!(
                "[[skill]]\nname = \"a\"\npath = \"skills/a\"\nsource = \"s\"\ncommit = \"{commit}\"\nsha256 = \"0\"\ninvocation = \"model\"\nreview = \"pending\"\n"
            )
        };
        std::fs::write(bundled.join(LOCK_FILE), entry("bundled")).unwrap();
        std::fs::write(project.join(LOCK_FILE), entry("project")).unwrap();
        let paths = SkillPaths {
            project,
            user: dir.path().join("u"),
            bundled,
        };
        let lock = paths.lock().unwrap();
        assert_eq!(lock.skills.len(), 1);
        assert_eq!(lock.get("a").unwrap().commit, "project");
    }

    #[test]
    fn the_bundled_set_lists_and_checks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = SkillPaths {
            project: dir.path().join("p"),
            user: dir.path().join("u"),
            bundled: default_bundled_dir(),
        };
        assert_eq!(list(&paths).unwrap(), 0);
        // Pending entries with findings block until reviewed (step 10).
        let lock = paths.lock().unwrap();
        let manifests = discover_roots(&paths.roots()).unwrap();
        assert_eq!(manifests.len(), 38);
        let expected = if blocking(&manifests, &lock).is_empty() {
            0
        } else {
            1
        };
        assert_eq!(check_all(&paths).unwrap(), expected);
    }
}
