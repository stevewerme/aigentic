//! `SKILL.md`: a flat YAML-style frontmatter between `---` lines, then the
//! body. Upstream uses only single-line `key: value` pairs, so no YAML
//! library is needed; a multi-line value is rejected loudly.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

use crate::SkillError;

/// Who may invoke a skill. `disable-model-invocation: true` in the
/// frontmatter makes it `User`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Invocation {
    /// A slash command in the client; never offered through `load_skill`.
    User,
    /// Offered to the model through `load_skill`.
    Model,
}

/// Where a skill resolved from. Closer wins by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// `./skills/` in the working directory.
    Project,
    /// `~/.config/aigentic/skills/`.
    User,
    /// The vendored set shipped with the binary's repository.
    Bundled,
}

/// A parsed skill folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    pub invocation: Invocation,
    /// Upstream's `argument-hint`, shown next to a slash command.
    pub argument_hint: Option<String>,
    /// Everything after the frontmatter, leading blank lines trimmed.
    pub body: String,
    /// The folder holding `SKILL.md`.
    pub path: PathBuf,
    /// Every file beside `SKILL.md`, relative to `path`, sorted: scripts,
    /// references, agent metadata. The static check scans them all.
    pub files: Vec<PathBuf>,
    pub origin: Origin,
}

impl Manifest {
    /// Parse `dir/SKILL.md` and list the folder's other files.
    pub fn parse(dir: &Path, origin: Origin) -> Result<Self, SkillError> {
        let skill_md = dir.join("SKILL.md");
        let text = std::fs::read_to_string(&skill_md).map_err(|source| SkillError::Io {
            path: skill_md.clone(),
            source,
        })?;
        let (front, body) = split_frontmatter(&text).ok_or_else(|| SkillError::Frontmatter {
            path: skill_md.clone(),
            message: "no frontmatter: SKILL.md must start with a `---` line and close it".into(),
        })?;

        let mut name = None;
        let mut description = None;
        let mut argument_hint = None;
        let mut disable_model = false;
        for (n, line) in front.lines().enumerate() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                return Err(SkillError::Frontmatter {
                    path: skill_md,
                    message: format!("line {}: expected `key: value`, got {line:?}", n + 2),
                });
            };
            if line.starts_with(char::is_whitespace) {
                return Err(SkillError::Frontmatter {
                    path: skill_md,
                    message: format!(
                        "line {}: multi-line values are not supported ({line:?})",
                        n + 2
                    ),
                });
            }
            let value = unquote(value.trim());
            match key.trim() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                "argument-hint" => argument_hint = Some(value),
                "disable-model-invocation" => disable_model = value == "true",
                _ => {} // upstream may add keys; they are ignored, not rejected
            }
        }
        let name = name
            .filter(|n| !n.is_empty())
            .ok_or_else(|| SkillError::Frontmatter {
                path: skill_md.clone(),
                message: "missing `name`".into(),
            })?;
        let description = description.ok_or_else(|| SkillError::Frontmatter {
            path: skill_md.clone(),
            message: "missing `description`".into(),
        })?;

        let mut files = Vec::new();
        for entry in WalkDir::new(dir).sort_by_file_name().min_depth(1) {
            let entry = entry.map_err(|e| SkillError::Io {
                path: dir.to_path_buf(),
                source: e.into(),
            })?;
            if entry.file_type().is_file() {
                let rel = entry
                    .path()
                    .strip_prefix(dir)
                    .expect("walked under dir")
                    .to_path_buf();
                if rel != Path::new("SKILL.md") {
                    files.push(rel);
                }
            }
        }
        files.sort();

        Ok(Self {
            name,
            description,
            invocation: if disable_model {
                Invocation::User
            } else {
                Invocation::Model
            },
            argument_hint,
            body: body.trim_start_matches('\n').to_owned(),
            path: dir.to_path_buf(),
            files,
            origin,
        })
    }

    /// The files that look like scripts: shell, Python or JavaScript by
    /// extension, or anything with a `#!` first line.
    pub fn scripts(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|f| is_script(&self.path.join(f)))
            .cloned()
            .collect()
    }

    /// One line for the stable prefix.
    pub fn description_line(&self) -> String {
        format!("{}: {}", self.name, self.description)
    }
}

/// `(frontmatter, body)` for a file starting with `---`.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.strip_prefix("\u{feff}").unwrap_or(text);
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Some((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        let inner = &value[1..value.len() - 1];
        return if bytes[0] == b'"' {
            inner.replace("\\\"", "\"").replace("\\\\", "\\")
        } else {
            inner.to_owned()
        };
    }
    value.to_owned()
}

pub(crate) fn is_script(path: &Path) -> bool {
    let by_ext = path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e,
            "sh" | "bash" | "zsh" | "fish" | "py" | "js" | "mjs" | "cjs" | "ts" | "rb" | "pl"
        )
    });
    if by_ext {
        return true;
    }
    let mut head = [0u8; 2];
    std::fs::File::open(path)
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut head)
        })
        .is_ok_and(|_| &head == b"#!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter_and_body() {
        let (f, b) = split_frontmatter("---\nname: x\n---\n\nBody\n").unwrap();
        assert_eq!(f, "name: x\n");
        assert_eq!(b, "\nBody\n");
        assert!(split_frontmatter("no frontmatter").is_none());
        assert!(split_frontmatter("---\nname: x\n").is_none(), "unclosed");
    }

    #[test]
    fn unquotes_both_quote_styles() {
        assert_eq!(unquote("\"a \\\"b\\\"\""), "a \"b\"");
        assert_eq!(unquote("'a'"), "a");
        assert_eq!(unquote("plain"), "plain");
        assert_eq!(unquote("\""), "\"");
    }
}
