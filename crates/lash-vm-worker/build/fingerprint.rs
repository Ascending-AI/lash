//! Build-only source closure shared by Cargo, Bazel and the identity laws.
#![allow(
    clippy::disallowed_methods,
    reason = "build-time source inputs, never runtime ambient reads"
)]

use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

type BuildResult<T> = Result<T, Box<dyn std::error::Error>>;

fn normalized(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}

fn rust_sources(directory: &Path, paths: &mut Vec<PathBuf>) -> BuildResult<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_sources(&path, paths)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            paths.push(path);
        }
    }
    Ok(())
}

pub fn inputs(root: &Path) -> BuildResult<(Vec<PathBuf>, Vec<PathBuf>)> {
    let workspace: toml::Table = fs::read_to_string(root.join("Cargo.toml"))?.parse()?;
    let dependencies = workspace["workspace"]["dependencies"]
        .as_table()
        .ok_or("workspace dependencies must be a table")?;
    let mut pending = vec![root.join("crates/lash-vm-worker/Cargo.toml")];
    let mut manifests = BTreeSet::new();
    while let Some(manifest) = pending.pop() {
        if !manifests.insert(manifest.clone()) {
            continue;
        }
        let package = manifest.parent().ok_or("manifest has no parent")?;
        let data: toml::Table = fs::read_to_string(&manifest)?.parse()?;
        let mut tables = vec![data.get("dependencies")];
        if let Some(targets) = data.get("target").and_then(toml::Value::as_table) {
            tables.extend(targets.values().map(|target| target.get("dependencies")));
        }
        for table in tables
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_table)
        {
            for (name, declaration) in table {
                let Some(mut declaration) = declaration.as_table() else {
                    continue;
                };
                let mut base = package;
                if declaration.get("workspace").and_then(toml::Value::as_bool) == Some(true) {
                    let Some(workspace_declaration) =
                        dependencies.get(name).and_then(toml::Value::as_table)
                    else {
                        continue;
                    };
                    declaration = workspace_declaration;
                    base = root;
                }
                if let Some(path) = declaration.get("path").and_then(toml::Value::as_str) {
                    pending.push(normalized(&base.join(path).join("Cargo.toml")));
                }
            }
        }
    }
    let mut paths = vec![
        root.join("Cargo.toml"),
        root.join("Cargo.lock"),
        root.join("rust-toolchain.toml"),
    ];
    let mut directories = Vec::new();
    for manifest in manifests {
        let package = manifest.parent().ok_or("manifest has no parent")?;
        let source = package.join("src");
        directories.push(source.clone());
        let mut sources = Vec::new();
        rust_sources(&source, &mut sources)?;
        sources.sort();
        paths.push(manifest.clone());
        paths.extend(sources);
        let script = package.join("build.rs");
        if script.exists() {
            paths.push(script);
            let build = package.join("build");
            directories.push(build.clone());
            let mut modules = Vec::new();
            rust_sources(&build, &mut modules)?;
            modules.sort();
            paths.extend(modules);
        }
    }
    Ok((paths, directories))
}

pub fn fingerprint(root: &Path, paths: &[PathBuf]) -> BuildResult<String> {
    let mut digest = Sha256::new();
    for path in paths {
        let relative = path
            .strip_prefix(root)?
            .to_str()
            .ok_or("non-UTF-8 source path")?;
        digest.update(relative.replace('\\', "/").as_bytes());
        digest.update(b"\0");
        digest.update(fs::read(path)?);
        digest.update(b"\0");
    }
    Ok(format!("{:x}", digest.finalize()))
}
