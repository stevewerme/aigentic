//! Puts the commit this binary is built from into `aigentic --version`.
//!
//! `AIGENTIC_GIT_SHA` is the first twelve characters of `git rev-parse
//! HEAD`, or `unknown` when that cannot be read (a source tarball, no
//! `git` on the path). The rerun lines make cargo rebuild when the commit
//! changes; a stale sha in a fresh binary would be worse than none.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/packed-refs");
    if let Some(reference) = head_reference() {
        println!("cargo:rerun-if-changed={}", reference.display());
    }
    println!("cargo:rustc-env=AIGENTIC_GIT_SHA={}", short_head());
}

/// The first twelve characters of `git rev-parse HEAD`, or `unknown`.
fn short_head() -> String {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(".")
        .output();
    let Ok(out) = out else {
        return "unknown".to_owned();
    };
    if !out.status.success() {
        return "unknown".to_owned();
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if sha.len() < 12 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return "unknown".to_owned();
    }
    sha[..12].to_owned()
}

/// The file under `.git` that holds the checked-out commit: `.git/<ref>`
/// when `HEAD` is a symbolic ref, else `HEAD` itself.
fn head_reference() -> Option<PathBuf> {
    let git_dir = Path::new("../../.git");
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(name) = head.strip_prefix("ref:") else {
        return Some(git_dir.join("HEAD"));
    };
    Some(git_dir.join(name.trim()))
}
