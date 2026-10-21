//! Records reviewed helper data explicitly, outside the correctness laws.
//!
//! From the repository root:
//! `kiln run //crates/lash-vm-releases:record-helper-data__bin -- record_helper_classes`
//! The kernel catalog recorder is `python3 scripts/record-kernel-helper-catalog.py`.
//! An optional second argument selects the repository root.

#![allow(clippy::disallowed_methods, reason = "explicit source-data recorder")]

use std::path::PathBuf;

use lash_vm_releases::{BuiltPackage, PACKAGES, PYTHON, TYPESCRIPT, package};

#[path = "../helper_classes.rs"]
mod helper_classes;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let command = args
        .next()
        .ok_or("expected record_helper_classes, then an optional repository root")?;
    let root = args
        .next()
        .or_else(|| std::env::var_os("BUILD_WORKSPACE_DIRECTORY"))
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    if args.next().is_some() {
        return Err("expected at most a command and a repository root".into());
    }
    let (path, text) = match command.to_str() {
        Some("record_helper_classes") => (
            "crates/lash-vm-releases/src/helper-classes.tsv",
            helper_classes::current_census()?,
        ),
        _ => return Err("unknown helper recorder command".into()),
    };
    std::fs::write(root.join(path), text)?;
    println!("recorded {path}");
    Ok(())
}
