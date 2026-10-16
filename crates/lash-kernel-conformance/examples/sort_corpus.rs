//! Refreshes K-VAL-033's sort document against the current library without
//! changing its unordered-comparator and unchanged-input oracle.
//!
//! Run `kiln run //crates/lash-kernel-conformance:sort_corpus__example -- <checkout root>`.

#![allow(clippy::disallowed_methods, reason = "explicit corpus recorder")]

use lash_kernel_doc::{FunctionRegistry, parse_document, print_document};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args_os()
        .nth(1)
        .ok_or("usage: sort_corpus <checkout root>")?;
    let mut registry = FunctionRegistry::new();
    lash_kernel_vm::register_machine_functions(&mut registry)?;
    lash_kernel_lib::register_numbers(&mut registry)?;
    lash_kernel_lib::register_text_json(&mut registry)?;
    lash_kernel_lib::register_collections(&mut registry)?;

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
    let main_start = case.document.find("main {").ok_or("missing main block")?;
    let mut text = String::from("kernel 1\nnumbers by_spelling\n");
    for (id, function) in registry.iter() {
        text.push_str(&format!("use {} = @{id}\n", function.definition().name));
    }
    text.push_str(&case.document[main_start..]);
    let mut document = parse_document(&text)?;
    document.manifest.functions = lash_kernel_check::requirements(&document, &registry).functions;
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
