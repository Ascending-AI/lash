//! Refreshes the kernel corpus against the current library without
//! changing its unordered-comparator and unchanged-input oracle.
//!
//! Run `kiln run //crates/lash-kernel-conformance:sort_corpus__example -- <checkout root>`.

#![allow(clippy::disallowed_methods, reason = "explicit corpus recorder")]

use lash_kernel_doc::{FunctionRegistry, parse_document, print_document};

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .ok_or("usage: sort_corpus <checkout root>")?;
    let mut registry = FunctionRegistry::declarations();
    lash_kernel_vm::register_machine_functions(&mut registry)?;
    lash_kernel_lib::register_numbers(&mut registry)?;
    lash_kernel_lib::register_text_json(&mut registry)?;
    lash_kernel_lib::register_collections(&mut registry)?;
    lash_ext_regex_ecma::register(&mut registry)?;
    lash_ext_regex_ecma::implement(
        &mut registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(4)),
    )?;

    if std::env::args().nth(2).as_deref() == Some("--regex") {
        return regex(std::path::Path::new(&root), &registry);
    }
    if std::env::args().nth(2).as_deref() == Some("--charges") {
        return charges(std::path::Path::new(&root), registry);
    }
    let path = std::path::PathBuf::from(root)
        .join("crates/lash-kernel-conformance/corpus/val/K-VAL-033.json");
    let original = std::fs::read_to_string(&path)?;
    let shard: lash_kernel_conformance::Shard = serde_json::from_str(&original)?;
    let case = shard
        .cases
        .iter()
        .find(|case| {
            case.name == "sort refuses an unordered comparator and leaves its input unchanged"
        })
        .ok_or("missing unordered sort case")?;
    let mut document = parse_document(&case.document)?;
    refresh_definitions(&mut document, &registry);
    let environment = lash_kernel_check::Environment::new(&registry);
    lash_kernel_check::admit(&document, &environment)?;
    // Replace only this document's JSON string; the oracle and every other
    // case keep their original bytes and formatting.
    let updated = original.replacen(
        &serde_json::to_string(&case.document)?,
        &serde_json::to_string(&print_document(&document))?,
        1,
    );
    std::fs::write(path, updated)?;
    Ok(())
}

fn charges(
    root: &std::path::Path,
    registry: FunctionRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let names: std::collections::BTreeMap<_, _> = registry
        .iter()
        .map(|(id, function)| (function.definition().name.clone(), *id))
        .collect();
    let mut runner = lash_kernel_conformance::MachineRunner::<lash_kernel_vm::KernelMachine>::new(
        Arc::new(registry),
    );
    fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                files(&path, out)?;
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    files(
        &root.join("crates/lash-kernel-conformance/corpus"),
        &mut paths,
    )?;
    let mut changed = 0;
    for path in paths {
        let original = std::fs::read_to_string(&path)?;
        let Ok(mut shard) = serde_json::from_str::<lash_kernel_conformance::Shard>(&original)
        else {
            continue;
        };
        let mut updated = original.clone();
        for case in &mut shard.cases {
            let before = case.document.clone();
            // Refusal cases intentionally contain invalid source. Leave them
            // and unknown/regex manifests to their own recorder.
            if let Ok(document) = parse_document(&before) {
                for (id, definition) in &document.functions {
                    if let Some(current) = names.get(&definition.name) {
                        case.document =
                            case.document.replace(&id.to_string(), &current.to_string());
                    }
                }
            }
            let pinned = case.expected.charged.take();
            if pinned.is_some() {
                let actual = lash_kernel_conformance::check_case(&mut runner, case)?;
                case.expected.charged = actual.charged;
                if pinned != case.expected.charged {
                    // Replace within this case only: other charge pins may
                    // have the same old number.
                    let old_case: serde_json::Value = serde_json::from_str(&original)?;
                    let old = old_case["cases"]
                        .as_array()
                        .ok_or("missing cases")?
                        .iter()
                        .find(|value| value["name"] == case.name)
                        .ok_or("missing case")?;
                    let mut new = old.clone();
                    new["expected"]["charged"] = serde_json::json!(actual.charged);
                    new["document"] = serde_json::json!(case.document);
                    let mut whole: serde_json::Value = serde_json::from_str(&updated)?;
                    let target = whole["cases"]
                        .as_array_mut()
                        .ok_or("missing cases")?
                        .iter_mut()
                        .find(|value| value["name"] == case.name)
                        .ok_or("missing case")?;
                    *target = new;
                    updated = serde_json::to_string_pretty(&whole)? + "\n";
                }
            }
            updated = updated.replace(
                &serde_json::to_string(&before)?,
                &serde_json::to_string(&case.document)?,
            );
        }
        if updated != original {
            std::fs::write(&path, updated)?;
            changed += 1;
        }
    }
    println!("refreshed {changed} charge/identity shards");
    Ok(())
}

/// Records the canonical regex representation and native identity while
/// retaining the guard limit, failure/cleanup and determinism oracles.
fn regex(
    root: &std::path::Path,
    registry: &FunctionRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = root.join("crates/lash-kernel-conformance/corpus/lib/K-LIB-008.json");
    let original = std::fs::read_to_string(&path)?;
    let mut shard: serde_json::Value = serde_json::from_str(&original)?;
    let current = registry
        .named("regex.ecma.test")
        .next()
        .ok_or("regex test declaration")?
        .to_string();
    for case in shard["cases"].as_array_mut().ok_or("corpus cases")? {
        let source = case["document"].as_str().ok_or("corpus document")?;
        let document = parse_document(source)?;
        let old = document
            .functions
            .iter()
            .find(|(_, name)| name.name.as_str() == "regex.ecma.test")
            .map(|(id, _)| id.to_string())
            .ok_or("regex test reference")?;
        let source = source.replace(&old, &current).replace(
            "{\"brand\": \"regex.ecma\",",
            "{\"__lash_brand\": (\"regex.ecma\", \"reg_exp\"), \"__lash_built\": b\"5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a\",",
        );
        case["document"] = serde_json::Value::String(source);
        if let Some(name) = case["expected"]["end"]["bound"]["name"].as_str() {
            case["expected"]["end"]["bound"]["name"] =
                serde_json::Value::String(name.replace(&old, &current));
        }
    }
    std::fs::write(path, serde_json::to_string_pretty(&shard)? + "\n")?;
    Ok(())
}

/// Replace complete local definitions from the build registry. Import slots stay document-owned.
fn refresh_definitions(document: &mut lash_kernel_doc::Document, registry: &FunctionRegistry) {
    let definitions: Vec<_> = document
        .functions
        .values()
        .map(|definition| {
            registry
                .named(definition.name.as_str())
                .next()
                .and_then(|id| registry.get(&id))
                .map_or_else(
                    || definition.clone(),
                    |registered| (**registered.definition()).clone(),
                )
        })
        .collect();
    document.functions = definitions
        .into_iter()
        .map(|definition| (definition.identity(), definition))
        .collect();
}
