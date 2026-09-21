use std::path::Path;

/// Repository instructions: `AGENTS.md` at `dir`, the vendor-neutral
/// convention. `None` when it does not exist. No tool-specific file is
/// read; a project's own file is `.aigentic/instructions.md` (phase 4).
pub fn load_instructions(dir: impl AsRef<Path>) -> std::io::Result<Option<String>> {
    for name in ["AGENTS.md"] {
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
    fn agents_md_or_none_and_no_vendor_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_instructions(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("CLAUDE.md"), "vendor file").unwrap();
        assert_eq!(load_instructions(dir.path()).unwrap(), None);
        std::fs::write(dir.path().join("AGENTS.md"), "agents").unwrap();
        assert_eq!(
            load_instructions(dir.path()).unwrap().as_deref(),
            Some("agents")
        );
    }
}
