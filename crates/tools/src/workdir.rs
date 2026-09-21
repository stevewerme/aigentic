use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The working directory shared by the built-in tools. Cheap to clone; all
/// clones see the same directory.
#[derive(Debug, Clone)]
pub struct Workdir(Arc<Mutex<PathBuf>>);

impl Workdir {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(Arc::new(Mutex::new(path.into())))
    }

    pub fn current(&self) -> PathBuf {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, path: impl Into<PathBuf>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = path.into();
    }

    /// Resolve `path` against the current directory. Absolute paths pass through.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        self.current().join(path)
    }
}
