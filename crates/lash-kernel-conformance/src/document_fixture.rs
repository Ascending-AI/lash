//! Final document assembly for conformance witnesses.
use std::collections::BTreeMap;
/// Final Documents used by library laws; definitions are explicit local code.
pub(crate) fn authored(
    text: &str,
    registry: &lash_kernel_doc::FunctionRegistry,
) -> lash_kernel_doc::Document {
    let mut definitions: BTreeMap<_, _> = registry
        .iter()
        .map(|(id, f)| (*id, (**f.definition()).clone()))
        .collect();
    let mut pending = Vec::new();
    let mut main = String::new();
    for line in text.lines() {
        if let Some(source) = line.trim().strip_prefix("definition ") {
            pending.push(
                serde_json::from_str::<String>(source)
                    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}")),
            );
        } else {
            main.push_str(line);
            main.push('\n');
        }
    }
    while !pending.is_empty() {
        let before = pending.len();
        let uses: String = definitions
            .iter()
            .map(|(id, f)| format!("use {} = @{id}\n", f.name))
            .collect();
        pending.retain(|source| {
            match lash_kernel_doc::parse_definition_with_locals(&format!("{source}\n{uses}")) {
                Ok((definition, locals)) => {
                    definitions.extend(locals.into_iter().map(|f| (f.identity(), f)));
                    definitions.insert(definition.identity(), definition);
                    false
                }
                Err(_) => true,
            }
        });
        assert!(
            pending.len() < before,
            "invalid final law definitions: {pending:?}"
        );
    }
    let imports_text = main
        .lines()
        .filter(|line| line.trim_start().starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let declared = lash_kernel_doc::parse_document(&imports_text)
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        .imports;
    let mut body_imports = BTreeMap::new();
    for definition in definitions.values() {
        if let Some(body) = definition.body() {
            for (slot, import) in &body.imports {
                if let Some(existing) = body_imports.insert(*slot, import.clone()) {
                    assert_eq!(existing, *import);
                }
            }
        }
    }
    let mut missing =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    missing.main = None;
    missing.imports = body_imports
        .into_iter()
        .filter(|(slot, import)| match declared.get(slot) {
            Some(existing) => {
                assert_eq!(existing, import);
                false
            }
            None => true,
        })
        .collect();
    let mut header = lash_kernel_doc::print_document(&missing);
    for f in definitions.values() {
        header.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(f))
                .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
        ));
        if f.has_native() {
            header.push_str(&format!("native @{}\n", f.identity()));
        }
    }
    lash_kernel_doc::parse_document(&format!("{header}{main}"))
        .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
}
