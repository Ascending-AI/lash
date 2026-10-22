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
    if std::env::args().nth(2).as_deref() == Some("--protocol-identities") {
        return protocol_identities(std::path::Path::new(&root));
    }
    if std::env::args().nth(2).as_deref() == Some("--contract-identities") {
        return contract_identities(std::path::Path::new(&root));
    }
    if std::env::args().nth(2).as_deref() == Some("--captions") {
        return caption_sites(std::path::Path::new(&root));
    }
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

    if std::env::args().nth(2).as_deref() == Some("--native-identities") {
        return native_identities(std::path::Path::new(&root), &registry);
    }
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
        if case["expected"]["end"]["bound"]["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("guard:"))
        {
            case["expected"]["end"]["bound"]["name"] =
                serde_json::Value::String(format!("guard:{current}"));
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

/// Refresh numeric fixture identities from the registry, preserving every
/// independently specified argument, outcome, charge and work bound.
fn native_identities(
    root: &std::path::Path,
    registry: &FunctionRegistry,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut changed = 0;
    let mut cases = 0;
    for entry in std::fs::read_dir(root.join("crates/lash-kernel-lib/corpus/native/numbers"))? {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let original = std::fs::read_to_string(&path)?;
        let shard: serde_json::Value = serde_json::from_str(&original)?;
        let mut updated = original.clone();
        for case in shard["cases"].as_array().ok_or("missing native cases")? {
            let label = case["name"].as_str().ok_or("missing native case name")?;
            let name = label
                .split([':', ' '])
                .next()
                .ok_or("missing native name")?;
            let id = registry
                .named(name)
                .next()
                .ok_or_else(|| format!("unknown native {name}"))?;
            let old = case["function"].as_str().ok_or("missing native identity")?;
            updated = updated.replace(
                &format!("\"function\": \"{old}\""),
                &format!("\"function\": \"{id}\""),
            );
            cases += 1;
        }
        // K-VAL-001's function-kind witness now carries the defining Link
        // and function identity. Its tag and independently written oracle stay.
        if shard["rule"] == "K-VAL-001" {
            let callable = lash_vocabulary::names::CallableId {
                link: lash_vocabulary::package::LinkId::from_bytes([0x11; 32]),
                function: registry.named("kind").next().ok_or("missing kind")?,
            };
            updated = updated.replace(
                "\"function\": \"f\"",
                &format!("\"function\": {}", serde_json::to_string(&callable)?),
            );
        }
        if original != updated {
            std::fs::write(path, updated)?;
            changed += 1;
        }
    }
    println!("refreshed {changed} numeric identity shards across {cases} cases");
    Ok(())
}

/// Re-identify protocol probe code without recording any observed charge.
fn protocol_identities(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    for (rule, name, formula) in [
        ("K-CHG-003", "result", "sum(9, size(result))"),
        ("K-CHG-004", "size", "size(x)"),
        ("K-CHG-005", "deep", "deep(x)"),
        ("K-CHG-006", "magnitude", "magnitude(x)"),
    ] {
        let path = root.join(format!(
            "crates/lash-kernel-conformance/protocol/chg/{rule}.json"
        ));
        let original = std::fs::read_to_string(&path)?;
        let mut shard: serde_json::Value = serde_json::from_str(&original)?;
        for case in shard["cases"].as_array_mut().ok_or("protocol cases")? {
            let source = case["document"].as_str().ok_or("protocol document")?;
            let written = parse_document(source)?;
            let old = *written
                .requires
                .natives
                .iter()
                .next()
                .ok_or("probe native")?;
            if written.requires.natives.len() != 1 {
                return Err("expected one probe native".into());
            }
            let params = if rule == "K-CHG-003" { "" } else { "x: Any" };
            let body = if rule == "K-CHG-003" {
                if case["name"]
                    .as_str()
                    .ok_or("case name")?
                    .starts_with("return")
                {
                    "body { return 7 }"
                } else {
                    "body { throw 7 }"
                }
            } else {
                ""
            };
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function probe.{name}({params}) -> Any\nkernel 1\ncharge {formula}\nnative\n{body}"
            ))?;
            let id = definition.identity();
            let mut source = source.replace(&old.to_string(), &id.to_string());
            loop {
                let document = parse_document(&source)?;
                let names: std::collections::BTreeMap<_, _> = document
                    .functions
                    .iter()
                    .map(|(id, definition)| (definition.name.clone(), *id))
                    .collect();
                if names.len() != document.functions.len() {
                    return Err("protocol local names must be unique".into());
                }
                let mut updated = source.clone();
                for definition in document.functions.values() {
                    if let Some(body) = definition.body() {
                        for (old, name) in &body.functions {
                            if let Some(current) = names.get(name) {
                                updated = updated.replace(&old.to_string(), &current.to_string());
                            }
                        }
                    }
                }
                if updated == source {
                    break;
                }
                source = updated;
            }
            let mut document = parse_document(&source)?;
            document.functions.insert(id, definition);
            case["document"] = serde_json::Value::String(print_document(&document));
        }
        std::fs::write(path, serde_json::to_string_pretty(&shard)? + "\n")?;
    }
    Ok(())
}

/// Record the fixed canonical identity vectors against the current 1.0 shape.
fn contract_identities(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let path = root.join("crates/lash-kernel-conformance/src/contract_cases.rs");
    let mut source = std::fs::read_to_string(&path)?;
    for rule in ["K-ID-001", "K-ID-003"] {
        let shard: lash_kernel_conformance::Shard =
            serde_json::from_str(&std::fs::read_to_string(root.join(format!(
                "crates/lash-kernel-conformance/corpus/id/{rule}.json"
            )))?)?;
        let id = parse_document(&shard.cases[0].document)?.identity();
        let marker = if rule == "K-ID-001" {
            r#"let expected = if shard.rule == "K-ID-001" {"#
        } else {
            "\"\n    } else {"
        };
        record_identity(&mut source, marker, &id.to_string())?;
    }
    let native = lash_kernel_doc::parse_definition(
        "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\n",
    )?;
    record_identity(
        &mut source,
        "original.identity().to_string(),",
        &native.identity().to_string(),
    )?;
    std::fs::write(path, source)?;
    Ok(())
}

/// Locate a golden vector by its assertion, so recording does not depend
/// on a previous digest or carry a second identity inventory.
fn record_identity(
    source: &mut String,
    marker: &str,
    id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let after = source.find(marker).ok_or("identity assertion marker")? + marker.len();
    let start = after + source[after..].find('"').ok_or("identity opening quote")? + 1;
    let end = start + source[start..].find('"').ok_or("identity closing quote")?;
    let old = &source[start..end];
    if old.len() != 64 || !old.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("identity assertion is not a digest".into());
    }
    source.replace_range(start..end, id);
    Ok(())
}

/// Project authored named sites to exact lowered bodies; caption text and paths stay independent.
fn caption_sites(root: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    use lash_kernel_dialect::{
        Environment, FrontEnd, Mode, NamespaceKind, NamespaceMember, Namespaces, OperationSignature,
    };
    use lash_vocabulary::{OperationMode, TurnControls, package::Visibility};
    let namespaces = Namespaces::new(
        std::collections::BTreeMap::from([(
            lash_vocabulary::PackagePath::parse("corpus")?,
            std::collections::BTreeMap::from([(
                lash_vocabulary::Segment::new("echo")?,
                NamespaceMember {
                    visibility: Visibility::Public,
                    kind: NamespaceKind::Operation(OperationSignature {
                        signature: lash_kernel_doc::parse_signature("(input: Any) -> Any")?,
                        controls: TurnControls::none(),
                        mode: OperationMode::Waited,
                    }),
                },
            )]),
        )]),
        Default::default(),
        &|_| false,
    )?;
    for dialect in lash_ext_helper_compiler::library::Dialect::ALL {
        let built = lash_ext_helper_compiler::library::Library::of(dialect)?;
        let frontend: Box<dyn FrontEnd> = match dialect {
            lash_ext_helper_compiler::library::Dialect::TypeScript => {
                Box::new(lash_dialect_typescript::TypeScript::default())
            }
            lash_ext_helper_compiler::library::Dialect::Python => {
                Box::new(lash_dialect_python::Python::default())
            }
        };
        let path = root.join(format!(
            "crates/lash-kernel-conformance/captions/{}.json",
            dialect.name()
        ));
        let mut corpus: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        for case in corpus["authored"].as_array_mut().ok_or("authored cases")? {
            let source = case["source"].as_str().ok_or("caption source")?;
            let lowered = frontend
                .lower(
                    source,
                    &Environment::new(&built.names, &namespaces),
                    Mode::Module,
                )
                .map_err(|error| format!("{}: {error:?}", case["name"]))?;
            for entry in case["captions"].as_array_mut().ok_or("caption entries")? {
                let caption: lash_kernel_doc::Caption =
                    serde_json::from_value(entry.get("caption").ok_or("caption text")?.clone())?;
                let Some(site) = entry.get_mut("site") else {
                    continue;
                };
                // Source text and every caption string stay independent. A
                // unique authored caption identifies the construct whose
                // producer now owns an exact retained-body site; old inline
                // paths cannot be traversed after closure extraction.
                let mut owners = lowered
                    .captions
                    .entries()
                    .iter()
                    .filter(|owned| owned.caption == caption)
                    .filter_map(|owned| owned.target.site());
                let owner = owners.next().ok_or("authored caption has no site owner")?;
                if owners.next().is_some() {
                    return Err("authored site caption must be unique within its case".into());
                }
                *site = serde_json::to_value(owner)?;
            }
        }
        std::fs::write(path, serde_json::to_string_pretty(&corpus)? + "\n")?;
    }
    Ok(())
}
