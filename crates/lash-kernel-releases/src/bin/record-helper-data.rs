//! Records reviewed helper data explicitly, outside the correctness laws.
//!
//! From the repository root:
//! `kiln run //crates/lash-kernel-releases:record-helper-data__bin -- record_helper_classes`
//! The kernel catalog recorder is `python3 scripts/record-kernel-helper-catalog.py`.
//! An optional second argument selects the repository root.

#![allow(clippy::disallowed_methods, reason = "explicit source-data recorder")]

use std::path::PathBuf;

use lash_kernel_releases::{BuiltPackage, PACKAGES, PYTHON, TYPESCRIPT, helper_release, package};

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
    if command == "record_archive" {
        let archive = lash_kernel_releases::helper_archive()?;
        let candidate = lash_kernel_releases::archive_candidate()?;
        archive.check_append(&candidate)?;
        let directory = root.join("crates/lash-kernel-releases/archive");
        std::fs::create_dir_all(&directory)?;
        for (at, release) in candidate
            .releases
            .iter()
            .enumerate()
            .skip(archive.releases.len())
        {
            let path = directory.join(format!("release-{at:04}.json"));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)?;
            use std::io::Write as _;
            file.write_all(&serde_json::to_vec(release)?)?;
            file.write_all(b"\n")?;
        }
        std::fs::write(
            directory.join("ledger.json"),
            format!("{}\n", serde_json::to_string(&candidate.ledger)?),
        )?;
        let includes = (0..candidate.releases.len())
            .map(|at| format!("    include_str!(\"../archive/release-{at:04}.json\"),\n"))
            .collect::<String>();
        std::fs::write(
            root.join("crates/lash-kernel-releases/src/archive_releases.rs"),
            format!("&[\n{includes}]\n"),
        )?;
        println!("recorded {} releases", candidate.releases.len());
        return Ok(());
    }
    let (path, text) = match command.to_str() {
        Some("record_helper_classes") => (
            "crates/lash-kernel-releases/src/helper-classes.tsv",
            helper_classes::current_census()?,
        ),
        _ => return Err("unknown helper recorder command".into()),
    };
    std::fs::write(root.join(path), text)?;
    println!("recorded {path}");
    Ok(())
}
