//! The forge seam: the issue a run is for, and the comments a run posts.
//!
//! [`GhForge`] shells out to `gh`, which is why a comment body never goes
//! through `argv`: no quoting rule and no size limit stands between a
//! report and the issue.

use std::io::Write as _;
use std::process::Command;
use std::sync::{Arc, Mutex};

use ulid::Ulid;

/// The issue a run is for, as the forge reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueView {
    /// The issue's title.
    pub title: String,
    /// The issue's body, as written.
    pub body: String,
}

/// Every way a forge can fail.
#[derive(Debug, thiserror::Error)]
pub enum ForgeError {
    /// `gh` ran and said no.
    #[error("gh {args} failed: {message}")]
    Command {
        /// The arguments, space-joined, for the message.
        args: String,
        /// What `gh` wrote on stderr.
        message: String,
    },
    /// `gh` printed something this code cannot read.
    #[error("gh printed no readable JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// `gh` could not be run at all, or its temp file could not be written.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Read the issue and post comments on it.
pub trait Forge {
    /// The issue's title and body.
    fn issue(&self, n: u64) -> Result<IssueView, ForgeError>;
    /// The body of every comment on the issue, oldest first.
    fn comments(&self, n: u64) -> Result<Vec<String>, ForgeError>;
    /// Post one comment.
    fn comment(&self, n: u64, body: &str) -> Result<(), ForgeError>;
}

impl<T: Forge + ?Sized> Forge for Arc<T> {
    fn issue(&self, n: u64) -> Result<IssueView, ForgeError> {
        (**self).issue(n)
    }

    fn comments(&self, n: u64) -> Result<Vec<String>, ForgeError> {
        (**self).comments(n)
    }

    fn comment(&self, n: u64, body: &str) -> Result<(), ForgeError> {
        (**self).comment(n, body)
    }
}

/// The real forge: this repository's GitHub issues, through `gh`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GhForge;

impl GhForge {
    /// A forge that runs `gh` from `PATH`.
    pub fn new() -> Self {
        Self
    }
}

impl Forge for GhForge {
    fn issue(&self, n: u64) -> Result<IssueView, ForgeError> {
        #[derive(serde::Deserialize)]
        struct Wire {
            #[serde(default)]
            title: Option<String>,
            #[serde(default)]
            body: Option<String>,
        }
        let text = gh(&["issue", "view", &n.to_string(), "--json", "title,body"])?;
        let wire: Wire = serde_json::from_str(&text)?;
        Ok(IssueView {
            title: wire.title.unwrap_or_default(),
            body: wire.body.unwrap_or_default(),
        })
    }

    fn comments(&self, n: u64) -> Result<Vec<String>, ForgeError> {
        #[derive(serde::Deserialize)]
        struct Comment {
            #[serde(default)]
            body: Option<String>,
        }
        #[derive(serde::Deserialize)]
        struct Wire {
            #[serde(default)]
            comments: Vec<Comment>,
        }
        let text = gh(&["issue", "view", &n.to_string(), "--json", "comments"])?;
        let wire: Wire = serde_json::from_str(&text)?;
        Ok(wire
            .comments
            .into_iter()
            .map(|c| c.body.unwrap_or_default())
            .collect())
    }

    fn comment(&self, n: u64, body: &str) -> Result<(), ForgeError> {
        // The body goes in a file: a report is long, and argv is not the
        // place for it.
        let path = std::env::temp_dir().join(format!(
            "aigentic-comment-{}-{}.md",
            std::process::id(),
            Ulid::generate()
        ));
        {
            let mut file = std::fs::File::create(&path)?;
            file.write_all(body.as_bytes())?;
            file.flush()?;
        }
        let arg = path.to_string_lossy().into_owned();
        let result = gh(&["issue", "comment", &n.to_string(), "--body-file", &arg]);
        let _ = std::fs::remove_file(&path);
        result.map(|_| ())
    }
}

/// Run one `gh` command and return its stdout.
fn gh(args: &[&str]) -> Result<String, ForgeError> {
    let out = Command::new("gh").args(args).output()?;
    if !out.status.success() {
        return Err(ForgeError::Command {
            args: args.join(" "),
            message: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A forge in memory: one fixed issue and the comments posted on it.
///
/// It lives in the library, and not with the tests, because the tests that
/// need it are a separate crate: an integration test cannot see a
/// `#[cfg(test)]` item.
#[derive(Debug, Default)]
pub struct FakeForge {
    /// What `issue` answers, whatever the number asked for.
    pub issue: IssueView,
    /// Every comment posted, oldest first.
    pub comments: Mutex<Vec<String>>,
}

impl FakeForge {
    /// A forge whose issue is `issue` and which holds no comments yet.
    pub fn new(issue: IssueView) -> Self {
        Self {
            issue,
            comments: Mutex::new(Vec::new()),
        }
    }

    /// A forge that starts with `comments` already on the issue.
    pub fn with_comments(issue: IssueView, comments: Vec<String>) -> Self {
        Self {
            issue,
            comments: Mutex::new(comments),
        }
    }

    /// What has been posted, oldest first.
    pub fn posted(&self) -> Vec<String> {
        self.comments.lock().expect("comments mutex").clone()
    }
}

impl Forge for FakeForge {
    fn issue(&self, _n: u64) -> Result<IssueView, ForgeError> {
        Ok(self.issue.clone())
    }

    fn comments(&self, _n: u64) -> Result<Vec<String>, ForgeError> {
        Ok(self.posted())
    }

    fn comment(&self, _n: u64, body: &str) -> Result<(), ForgeError> {
        self.comments
            .lock()
            .expect("comments mutex")
            .push(body.to_string());
        Ok(())
    }
}
