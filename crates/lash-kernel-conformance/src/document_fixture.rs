//! Final document assembly for conformance witnesses.
use std::collections::BTreeMap;
/// Final Documents used by library laws; definitions are explicit local code.
pub(crate) fn authored(
    text: &str,
    registry: &lash_kernel_doc::FunctionRegistry,
) -> lash_kernel_doc::Document {
    // Names come from the registry's index; explicit identities come from
    // the source. Never format every registry identity for each input case.
    let mut selected = std::collections::BTreeSet::new();
    selected.extend(
        registry
            .names()
            .filter_map(|(name, id)| text.contains(name).then_some(id)),
    );
    for digits in text.split(|ch: char| !matches!(ch, '0'..='9' | 'a'..='f')) {
        for start in 0..=digits.len().saturating_sub(64) {
            if let Some(spelling) = digits.get(start..start + 64)
                && let Ok(id) = lash_vocabulary::FunctionId::parse(spelling)
                && registry.get(&id).is_some()
            {
                selected.insert(id);
            }
        }
    }
    let mut definitions: BTreeMap<_, _> = selected
        .into_iter()
        .map(|id| {
            let registered = registry
                .get(&id)
                .unwrap_or_else(|| unreachable!("selected registration"));
            (id, (**registered.definition()).clone())
        })
        .collect();
    let mut required: Vec<_> = definitions.keys().copied().collect();
    while let Some(id) = required.pop() {
        let Some(body) = definitions[&id].body() else {
            continue;
        };
        let callees: Vec<_> = body.functions.keys().copied().collect();
        for callee in callees {
            if let std::collections::btree_map::Entry::Vacant(entry) = definitions.entry(callee) {
                let definition = registry
                    .get(&callee)
                    .unwrap_or_else(|| panic!("fixture dependency {callee}"));
                entry.insert((**definition.definition()).clone());
                required.push(callee);
            }
        }
    }
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
        pending.retain(|source| {
            let uses: String = definitions
                .iter()
                .filter(|(id, f)| {
                    source.contains(f.name.as_str()) || source.contains(&id.to_string())
                })
                .map(|(id, f)| format!("use {} = @{id}\n", f.name))
                .collect();
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
    missing.requires.natives.extend(
        definitions
            .iter()
            .filter_map(|(id, definition)| definition.has_native().then_some(*id)),
    );
    missing.functions = definitions;
    lash_kernel_doc::core_support::parse_document_over(
        &main,
        missing,
        lash_vocabulary::data_limits::KernelDataLimits::standard(),
    )
    .unwrap_or_else(|error| panic!("linked law fixture: {error:?}"))
}

/// FIG-7135: a final law fixture already owns typed definitions. Installing
/// them beside a parsed body must not print and parse those bodies again.
#[test]
fn authored_fixtures_preserve_definitions_without_text_round_trips() {
    use lash_kernel_doc::{FunctionRegistry, NumberPolicy, parse_definition};
    let definition = parse_definition(
        "function law.helper(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }",
    )
    .expect("helper");
    let mut registry = FunctionRegistry::default();
    let function = registry
        .register(definition.clone(), None)
        .expect("register");
    let text = format!("kernel 1\nnumbers by_spelling\nmain {{ return invoke @{function}(1) }}");
    let _ = lash_kernel_doc::document_work::take_definition_parses();
    let actual = authored(&text, &registry);
    let mut expected = lash_kernel_doc::parse_document(&text).expect("explicit-id body");
    expected.functions.insert(function, definition);
    assert_eq!(actual, expected);
    assert_eq!(actual.numbers, NumberPolicy::BySpelling);
    assert_eq!(
        lash_kernel_doc::document_work::take_definition_parses(),
        0,
        "fixture assembly serialized definitions already held as typed facts"
    );
}
