//! Record admission witnesses in the document-local grammar; keep their outcome oracles.
#![allow(clippy::disallowed_methods, reason = "explicit corpus recorder")]
use lash_kernel_doc::{Export, parse_document, print_document};
use std::{error::Error, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(std::env::args_os().nth(1).ok_or("checkout root")?);
    for shard in [
        "adm/K-ADM-005",
        "fn/K-FN-002",
        "fn/K-FN-004",
        "doc/K-DOC-003",
    ] {
        let path = root.join(format!(
            "crates/lash-kernel-conformance/corpus/{shard}.json"
        ));
        let original = std::fs::read_to_string(&path)?;
        let mut updated = original.clone();
        let mut value: serde_json::Value = serde_json::from_str(&original)?;
        for case in value["cases"].as_array_mut().ok_or("cases")? {
            let before = case["document"].as_str().ok_or("document")?.to_owned();
            let mut source = String::new();
            for line in case["document"].as_str().ok_or("document")?.lines() {
                if let Some(text) = line.strip_prefix("definition ") {
                    let mut definition: String = serde_json::from_str(text)?;
                    definition = definition.replace("\\n", "\n");
                    if shard == "fn/K-FN-004" {
                        definition =
                            definition.replace("pair(a: Any, b: Any)", "pair(a: Any, b?: Any)");
                    }
                    if case["name"] == "entry arity agrees with the declaration" {
                        definition = definition.replace("start(x: Int)", "start()");
                    }
                    source.push_str(&format!(
                        "definition {}\n",
                        serde_json::to_string(&definition)?
                    ));
                } else {
                    source.push_str(line);
                    source.push('\n');
                }
            }
            let mut document = parse_document(&source)?;
            for (name, export) in &mut document.exports {
                if let Export::Function(export) = export
                    && let Some((id, _)) = document
                        .functions
                        .iter()
                        .find(|(_, f)| f.name.as_str() == name.as_str())
                {
                    export.function = *id;
                }
            }
            updated = updated.replacen(
                &serde_json::to_string(&before)?,
                &serde_json::to_string(&print_document(&document))?,
                1,
            );
        }
        if updated != original {
            std::fs::write(path, updated)?;
        }
    }
    let path = root.join("crates/lash-kernel-conformance/supplements/json/K-LJSON-005.json");
    let mut value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
    let mut registry = lash_kernel_doc::FunctionRegistry::declarations();
    lash_kernel_lib::register_numbers(&mut registry)?;
    lash_kernel_lib::register_text_json(&mut registry)?;
    let id = registry.named("json.stringify").next().ok_or("stringify")?;
    let definition = registry.get(&id).ok_or("stringify definition")?;
    for case in value["cases"].as_array_mut().ok_or("cases")? {
        let source = case["document"]
            .as_str()
            .ok_or("document")?
            .lines()
            .filter(|line| {
                !line.starts_with("use ")
                    && !line.starts_with("definition ")
                    && !line.starts_with("native ")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let (header, main) = source.split_once("main ").ok_or("main")?;
        let source = format!(
            "{header}native @{id}\ndefinition {}\nmain {main}",
            serde_json::to_string(&lash_kernel_doc::print_definition(definition.definition()))?
        );
        case["document"] = serde_json::json!(print_document(&parse_document(&source)?));
    }
    std::fs::write(path, serde_json::to_string_pretty(&value)? + "\n")?;
    let path = root.join("crates/lash-kernel-conformance/corpus/lib/K-LIB-009.json");
    let original = std::fs::read_to_string(&path)?;
    let mut updated = original.clone();
    let shard: lash_kernel_conformance::Shard = serde_json::from_str(&original)?;
    for case in shard.cases {
        let document = parse_document(&case.document)?;
        let expected = registry.named("eq").next().ok_or("eq")?;
        let used =
            lash_kernel_doc::Node::Block(document.main.as_ref().ok_or("main")?).functions_within();
        let mut source = case.document.clone();
        for id in used {
            if !document.functions.contains_key(&id) {
                source = source.replace(&id.to_string(), &expected.to_string());
            }
        }
        updated = updated.replacen(
            &serde_json::to_string(&case.document)?,
            &serde_json::to_string(&source)?,
            1,
        );
    }
    if updated != original {
        std::fs::write(path, updated)?;
    }
    Ok(())
}
