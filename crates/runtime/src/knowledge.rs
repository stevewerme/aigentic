//! A project's knowledge folder: every `*.md` under `.aigentic/knowledge/`,
//! inlined in the prefix when small, indexed with a `search_knowledge`
//! tool when over a fraction of the model's window. Decided at startup
//! and when the folder changes, never mid-turn. Symlinks are followed,
//! so a folder can point at docs elsewhere in the repository instead of
//! copying them; a dangling link or a loop is an error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::SystemTime;

use aigentic_tools::{KnowledgeSources, Section, split_sections};
use walkdir::WalkDir;

use crate::layers::WorkspaceLayer;
use crate::project::{DOT_DIR, KNOWLEDGE_DIR, MEMORY_DIR, Project, ProjectError};
use crate::runtime::ProjectRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnowledgeFile {
    /// Relative to the folder, `/` separators.
    pub path: String,
    pub text: String,
    pub sections: Vec<Section>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnowledgeMode {
    Inline,
    Index,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Knowledge {
    pub files: Vec<KnowledgeFile>,
    /// The provider's count over the whole folder.
    pub tokens: u64,
    /// Paths, sizes and mtimes; a change means reload.
    fingerprint: Vec<(String, u64, Option<std::time::SystemTime>)>,
}

impl Knowledge {
    /// Read the folder; a missing one is empty. `count` is the provider's
    /// token count for a text.
    pub fn load(dir: &Path, count: &dyn Fn(&str) -> u64) -> Result<Self, ProjectError> {
        Self::walk(dir, None, count).map(|(knowledge, _)| knowledge)
    }

    /// Read a folder that belongs to someone else — a sibling project —
    /// with `within` its root: a file whose real path is not under
    /// `within` is skipped, never read, and counted in the returned
    /// number. A link out of a project is not that project's knowledge;
    /// the folder is not ours to trust.
    pub fn load_within(
        dir: &Path,
        within: &Path,
        count: &dyn Fn(&str) -> u64,
    ) -> Result<(Self, usize), ProjectError> {
        Self::walk(dir, Some(within), count)
    }

    /// Read the folder, counting the files a boundary kept out. The
    /// fingerprint covers every `*.md` the walk sees, skipped or not, so
    /// `changed` compares like with like.
    fn walk(
        dir: &Path,
        within: Option<&Path>,
        count: &dyn Fn(&str) -> u64,
    ) -> Result<(Self, usize), ProjectError> {
        // Canonicalise the root: a project reached through a link (macOS's
        // `/tmp` beside `/private/tmp`) is the same project, and only
        // real paths compare.
        let root = within.and_then(|r| std::fs::canonicalize(r).ok());
        let mut files = Vec::new();
        let mut fingerprint = Vec::new();
        let mut skipped = 0;
        if dir.is_dir() {
            for entry in WalkDir::new(dir).follow_links(true).sort_by_file_name() {
                let entry = match entry {
                    Ok(entry) => entry,
                    // A loop or a dangling link in another project's
                    // folder is not this search's failure.
                    Err(_) if root.is_some() => continue,
                    Err(e) => {
                        return Err(ProjectError::Io {
                            path: e
                                .path()
                                .map_or_else(|| dir.to_path_buf(), Path::to_path_buf),
                            source: e.into(),
                        });
                    }
                };
                if !entry.file_type().is_file()
                    || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
                {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walked under dir")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let meta = entry.metadata().ok();
                fingerprint.push((
                    rel.clone(),
                    meta.as_ref().map_or(0, |m| m.len()),
                    meta.and_then(|m| m.modified().ok()),
                ));
                let inside = root.as_ref().is_none_or(|root| {
                    std::fs::canonicalize(entry.path()).is_ok_and(|real| real.starts_with(root))
                });
                if !inside {
                    skipped += 1;
                    continue;
                }
                let text =
                    std::fs::read_to_string(entry.path()).map_err(|source| ProjectError::Io {
                        path: entry.path().to_path_buf(),
                        source,
                    })?;
                let sections = split_sections(&rel, &text);
                files.push(KnowledgeFile {
                    path: rel,
                    text,
                    sections,
                });
            }
        }
        let tokens = files.iter().map(|f| count(&f.text)).sum();
        Ok((
            Self {
                files,
                tokens,
                fingerprint,
            },
            skipped,
        ))
    }

    /// Whether the folder on disk differs from what was loaded.
    pub fn changed(&self, dir: &Path) -> bool {
        let mut now = Vec::new();
        if dir.is_dir() {
            for entry in WalkDir::new(dir)
                .follow_links(true)
                .sort_by_file_name()
                .into_iter()
                .flatten()
            {
                if !entry.file_type().is_file()
                    || entry.path().extension().and_then(|e| e.to_str()) != Some("md")
                {
                    continue;
                }
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walked under dir")
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                let meta = entry.metadata().ok();
                now.push((
                    rel,
                    meta.as_ref().map_or(0, |m| m.len()),
                    meta.and_then(|m| m.modified().ok()),
                ));
            }
        }
        now != self.fingerprint
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Inline under `threshold` of `window`, else index.
    pub fn mode(&self, window: u64, threshold: f32) -> KnowledgeMode {
        let line = (window as f64 * f64::from(threshold)).round() as u64;
        if self.tokens <= line {
            KnowledgeMode::Inline
        } else {
            KnowledgeMode::Index
        }
    }

    /// The prefix block: the files themselves, or one line per file with
    /// a pointer to `search_knowledge`. `None` for an empty folder.
    pub fn prefix(&self, mode: KnowledgeMode) -> Option<String> {
        self.block("# Project knowledge", "to read the sections you need", mode)
    }

    /// The workspace's block: the same shape as the project's, headed by
    /// the workspace's name and pointing `workspace: true` at it when the
    /// folder is indexed.
    pub fn workspace_prefix(&self, name: &str, mode: KnowledgeMode) -> Option<String> {
        self.block(
            &format!("# Workspace knowledge: {name}"),
            "with `workspace: true` to read the sections you need",
            mode,
        )
    }

    /// The block under `heading`, with `how` telling the model what to do
    /// when the folder is too large to inline.
    fn block(&self, heading: &str, how: &str, mode: KnowledgeMode) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let mut out = String::new();
        match mode {
            KnowledgeMode::Inline => {
                out.push_str(heading);
                out.push('\n');
                for f in &self.files {
                    out.push_str(&format!("\n## {}\n\n{}\n", f.path, f.text.trim_end()));
                }
            }
            KnowledgeMode::Index => {
                out.push_str(&format!(
                    "{heading} (index)\n\nThe folder is too large to include; use the `search_knowledge` tool {how}.\n\n"
                ));
                for f in &self.files {
                    let first = f
                        .sections
                        .iter()
                        .find(|s| s.heading != f.path)
                        .map_or("(no heading)", |s| s.heading.as_str());
                    out.push_str(&format!(
                        "- {}: {first} ({} sections)\n",
                        f.path,
                        f.sections.len()
                    ));
                }
            }
        }
        Some(out.trim_end().to_owned())
    }

    /// Every section in folder order, for the tool's snapshot.
    pub fn sections(&self) -> Vec<Section> {
        self.files.iter().flat_map(|f| f.sections.clone()).collect()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.files.iter().map(|f| PathBuf::from(&f.path)).collect()
    }
}

/// A memory file as sections, located `memory/<file>#<heading>` so a hit
/// says whether it came from knowledge or memory.
pub fn memory_sections(file: &str, text: &str) -> Vec<Section> {
    split_sections(&format!("memory/{file}"), text)
}

/// Whether `dir` holds any `*.md` at all, following links: the cheap
/// existence check that decides whether a scope is worth offering. It
/// reads no file and parses nothing.
pub fn holds_markdown(dir: &Path) -> bool {
    WalkDir::new(dir)
        .follow_links(true)
        .into_iter()
        .flatten()
        .any(|e| {
            e.file_type().is_file() && e.path().extension().and_then(|x| x.to_str()) == Some("md")
        })
}

/// Whether a project other than our own has knowledge or memory: what
/// `search_knowledge` would reach there.
pub fn project_has_corpus(root: &Path) -> bool {
    holds_markdown(&root.join(DOT_DIR).join(KNOWLEDGE_DIR))
        || holds_markdown(&root.join(DOT_DIR).join(MEMORY_DIR))
}

/// What a thread may search beyond its own knowledge snapshot: its own
/// project's memory, the sibling projects it understands, and its
/// workspace. The runtime installs the rows and layers; the tool asks for
/// one scope per call. A sibling's folder is read once and then served
/// from here until a file it holds changes.
#[derive(Debug)]
pub struct ScopeSources {
    /// Sibling corpora read since this was built; `loads` reports it.
    loads: AtomicUsize,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// The thread's own project: name, root, its knowledge and memory as
    /// sections.
    own: Option<(String, PathBuf, Vec<Section>)>,
    /// The understood sibling rows: name and root.
    siblings: Vec<(String, PathBuf)>,
    /// The workspace's name and its knowledge-plus-memory corpus.
    workspace: Option<(String, Vec<Section>)>,
    /// Sibling corpora read so far, by root.
    cache: HashMap<PathBuf, Sibling>,
}

/// One sibling project's corpus, kept until one of its files changes.
#[derive(Debug)]
struct Sibling {
    /// The sibling's knowledge folder, as loaded.
    dir: PathBuf,
    knowledge: Knowledge,
    /// Its memory files: path, mtime, sections.
    memory: Vec<(PathBuf, Option<SystemTime>, Vec<Section>)>,
    /// Files outside the sibling's root that its folders pointed at.
    skipped: usize,
    sections: Vec<Section>,
}

impl Sibling {
    /// Read one sibling's knowledge and memory under `root`. A file
    /// whose real path leaves `root` is skipped, never read: another
    /// project's folder is not ours to trust.
    fn load(root: &Path) -> Result<Self, ProjectError> {
        let dir = root.join(DOT_DIR).join(KNOWLEDGE_DIR);
        let (knowledge, mut skipped) = Knowledge::load_within(&dir, root, &|_| 0)?;
        let root_real = std::fs::canonicalize(root).ok();
        let mut memory = Vec::new();
        let mut paths: Vec<PathBuf> = std::fs::read_dir(root.join(DOT_DIR).join(MEMORY_DIR))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
            .collect();
        paths.sort();
        for path in paths {
            let inside = root_real.as_ref().is_none_or(|root| {
                std::fs::canonicalize(&path).is_ok_and(|real| real.starts_with(root))
            });
            if !inside {
                skipped += 1;
                continue;
            }
            let text = std::fs::read_to_string(&path).map_err(|source| ProjectError::Io {
                path: path.clone(),
                source,
            })?;
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            memory.push((path.clone(), modified(&path), memory_sections(&name, &text)));
        }
        let mut sections = knowledge.sections();
        sections.extend(memory.iter().flat_map(|(_, _, s)| s.clone()));
        Ok(Self {
            dir,
            knowledge,
            memory,
            skipped,
            sections,
        })
    }

    /// Whether the sibling's folder or one of its memory files moved on.
    fn changed(&self) -> bool {
        self.knowledge.changed(&self.dir)
            || self
                .memory
                .iter()
                .any(|(path, when, _)| modified(path) != *when)
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Default for ScopeSources {
    fn default() -> Self {
        Self::new()
    }
}

impl ScopeSources {
    /// A resolver that knows nothing until `install` hands it the scopes
    /// the thread may search.
    pub fn new() -> Self {
        Self {
            loads: AtomicUsize::new(0),
            state: Mutex::new(State::default()),
        }
    }

    /// How many sibling corpora have been read. A sibling whose files
    /// have not changed is served from the cache and adds nothing.
    pub fn loads(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    /// Install the scopes the thread may search: its own project and the
    /// knowledge the tool already holds for it, the rows it understands,
    /// and its workspace. The runtime calls this whenever a layer, a row
    /// or the folder changes.
    pub fn install(
        &self,
        project: Option<&Project>,
        own_knowledge: &[Section],
        rows: &[ProjectRow],
        workspace: Option<&WorkspaceLayer>,
    ) {
        let mut state = self.lock();
        state.own = project.map(|p| {
            let mut sections = own_knowledge.to_vec();
            sections.extend(
                p.memory
                    .iter()
                    .flat_map(|(name, text)| memory_sections(name, text)),
            );
            (p.name.clone(), p.root.clone(), sections)
        });
        state.siblings = rows
            .iter()
            .filter(|r| r.understood && project.is_none_or(|p| p.name != r.name))
            .map(|r| (r.name.clone(), r.root.clone()))
            .collect();
        state.workspace = workspace.map(|w| {
            let mut sections = w
                .knowledge
                .as_ref()
                .map(Knowledge::sections)
                .unwrap_or_default();
            sections.extend(
                w.memory
                    .iter()
                    .flat_map(|(name, text)| memory_sections(name, text)),
            );
            (w.name.clone(), sections)
        });
        let roots: Vec<PathBuf> = state.siblings.iter().map(|(_, r)| r.clone()).collect();
        state.cache.retain(|root, _| roots.contains(root));
    }

    /// Whether some understood sibling or the workspace has knowledge or
    /// memory: the cheap check that decides whether the tool is offered.
    pub fn has_reach(&self) -> bool {
        let state = self.lock();
        state
            .siblings
            .iter()
            .any(|(_, root)| project_has_corpus(root))
            || state
                .workspace
                .as_ref()
                .is_some_and(|(_, sections)| !sections.is_empty())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The sibling's corpus, read now if its files changed since last
    /// time, else the copy kept here.
    fn sibling(&self, state: &mut State, root: &Path) -> Result<(Vec<Section>, usize), String> {
        let fresh = state.cache.get(root).is_some_and(|s| !s.changed());
        if !fresh {
            let loaded = Sibling::load(root).map_err(|e| e.to_string())?;
            self.loads.fetch_add(1, Ordering::Relaxed);
            state.cache.insert(root.to_path_buf(), loaded);
        }
        let sibling = &state.cache[root];
        Ok((sibling.sections.clone(), sibling.skipped))
    }

    /// The names a refusal may offer: the understood projects with a
    /// corpus to search.
    fn searchable(state: &State) -> Vec<String> {
        state
            .own
            .iter()
            .map(|(name, root, _)| (name, root))
            .chain(state.siblings.iter().map(|(name, root)| (name, root)))
            .filter(|(_, root)| project_has_corpus(root))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// What a refusal says: what is wrong with `name`, and the names that
    /// would have worked.
    fn refusal(state: &State, name: &str, why: &str) -> String {
        let names = Self::searchable(state);
        let list = if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(", ")
        };
        format!("`{name}` {why}; searchable: {list}")
    }
}

impl KnowledgeSources for ScopeSources {
    fn resolve(
        &self,
        project: Option<&str>,
        workspace: bool,
    ) -> Result<(String, Vec<Section>), String> {
        let mut state = self.lock();
        if workspace {
            let (name, sections) = state.workspace.clone().ok_or_else(|| {
                "this thread is in no workspace: `search_knowledge` has no workspace knowledge or memory to search".to_owned()
            })?;
            if sections.is_empty() {
                return Err(format!(
                    "the workspace {name} has no knowledge or memory to search"
                ));
            }
            return Ok((format!("workspace {name}"), sections));
        }
        let name = project.expect("the tool sets one of project and workspace");
        if name.contains('/') || name.contains('\\') {
            return Err(format!(
                "`{name}` is a path, not a project name: `search_knowledge` takes the name the projects block lists"
            ));
        }
        if state.own.as_ref().is_some_and(|(own, _, _)| own == name) {
            let Some((_, _, sections)) = state
                .own
                .as_ref()
                .filter(|(_, _, sections)| !sections.is_empty())
            else {
                return Err(Self::refusal(
                    &state,
                    name,
                    "has no knowledge or memory to search",
                ));
            };
            return Ok((name.to_owned(), sections.clone()));
        }
        let root = state
            .siblings
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, root)| root.clone());
        let Some(root) = root else {
            return Err(Self::refusal(
                &state,
                name,
                "is not a project this thread understands",
            ));
        };
        let (sections, skipped) = self.sibling(&mut state, &root)?;
        if sections.is_empty() {
            return Err(Self::refusal(
                &state,
                name,
                "has no knowledge or memory to search",
            ));
        }
        let label = if skipped > 0 {
            format!("{name}\nskipped {skipped} files outside {name}")
        } else {
            name.to_owned()
        };
        Ok((label, sections))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ops")).unwrap();
        std::fs::write(dir.path().join("intro.md"), "# Intro\n\nHello.\n").unwrap();
        std::fs::write(
            dir.path().join("ops/deploy.md"),
            "# Deploys\n\nFridays.\n\n## Rollback\n\nvercel rollback.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();
        dir
    }

    #[test]
    fn loads_sorted_md_files_recursively_and_counts_tokens() {
        let dir = folder();
        let k = Knowledge::load(dir.path(), &|t| t.len() as u64).unwrap();
        assert_eq!(
            k.paths(),
            vec![PathBuf::from("intro.md"), PathBuf::from("ops/deploy.md")]
        );
        assert_eq!(k.tokens, 16 + 51);
        assert_eq!(k.sections().len(), 3);
        assert!(!k.changed(dir.path()));
        std::fs::write(dir.path().join("new.md"), "# New\n").unwrap();
        assert!(k.changed(dir.path()));
        let empty = Knowledge::load(&dir.path().join("nope"), &|_| 1).unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.prefix(KnowledgeMode::Inline), None);
    }

    #[test]
    fn symlinks_are_followed_and_a_bad_link_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("docs");
        std::fs::create_dir_all(docs.join("adr")).unwrap();
        std::fs::write(docs.join("adr/0001.md"), "# One\n\nfirst\n").unwrap();
        std::fs::write(docs.join("map.md"), "# Map\n\nhere\n").unwrap();
        let kd = dir.path().join(".aigentic/knowledge");
        std::fs::create_dir_all(&kd).unwrap();
        std::os::unix::fs::symlink("../../docs/adr", kd.join("adr")).unwrap();
        std::os::unix::fs::symlink("../../docs/map.md", kd.join("map.md")).unwrap();
        let k = Knowledge::load(&kd, &|t| t.len() as u64).unwrap();
        assert_eq!(
            k.paths(),
            vec![PathBuf::from("adr/0001.md"), PathBuf::from("map.md")]
        );
        assert_eq!(k.tokens, 13 + 12);
        assert!(!k.changed(&kd));
        std::fs::write(docs.join("map.md"), "# Map\n\nhere, edited\n").unwrap();
        assert!(k.changed(&kd), "a change behind the link is seen");
        std::os::unix::fs::symlink("../../docs/nope.md", kd.join("gone.md")).unwrap();
        let err = Knowledge::load(&kd, &|_| 1).unwrap_err();
        assert!(err.to_string().contains("gone.md"), "{err}");
        std::fs::remove_file(kd.join("gone.md")).unwrap();
        std::os::unix::fs::symlink(".", kd.join("loop")).unwrap();
        assert!(Knowledge::load(&kd, &|_| 1).is_err(), "a loop is refused");
    }

    #[test]
    fn mode_and_both_prefixes() {
        let dir = folder();
        let k = Knowledge::load(dir.path(), &|t| t.len() as u64).unwrap();
        assert_eq!(k.mode(1000, 0.4), KnowledgeMode::Inline, "67 <= 400");
        assert_eq!(k.mode(100, 0.4), KnowledgeMode::Index, "67 > 40");
        assert_eq!(
            k.prefix(KnowledgeMode::Inline).unwrap(),
            "# Project knowledge\n\n## intro.md\n\n# Intro\n\nHello.\n\n## ops/deploy.md\n\n# Deploys\n\nFridays.\n\n## Rollback\n\nvercel rollback."
        );
        let index = k.prefix(KnowledgeMode::Index).unwrap();
        assert!(index.starts_with("# Project knowledge (index)"));
        assert!(index.contains("`search_knowledge`"));
        assert!(
            index
                .ends_with("- intro.md: Intro (1 sections)\n- ops/deploy.md: Deploys (2 sections)"),
            "{index}"
        );
    }
}
