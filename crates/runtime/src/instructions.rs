use std::path::Path;

/// Repository instructions: `AGENTS.md` at `dir`, falling back to
/// `CLAUDE.md`. `None` when neither exists.
pub fn load_instructions(dir: impl AsRef<Path>) -> std::io::Result<Option<String>> {
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = dir.as_ref().join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => return Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_md_wins_then_claude_md_then_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_instructions(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("CLAUDE.md"), "claude").unwrap();
        assert_eq!(
            load_instructions(dir.path()).unwrap().as_deref(),
            Some("claude")
        );
        std::fs::write(dir.path().join("AGENTS.md"), "agents").unwrap();
        assert_eq!(
            load_instructions(dir.path()).unwrap().as_deref(),
            Some("agents")
        );
    }
}
