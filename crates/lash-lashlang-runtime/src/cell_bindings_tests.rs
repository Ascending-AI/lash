use super::*;
use crate::ToolDefinitionBindingExt;

fn tool(id: &str, operation: &str, description: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        operation,
        description,
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::Value::Null,
    )
    .with_tool_binding(crate::ToolBinding::new(["app"], operation).with_authority_type("App"))
}

fn referenced(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|path| path.to_string()).collect()
}

fn recorded(
    paths: &[&str],
    catalog: &lash_core::ToolCatalog,
    live: &lash_core::ToolCatalog,
) -> CellToolBindings {
    let record = resolve_ambient_bindings(&referenced(paths), catalog, &BTreeSet::new())
        .expect("the binding set encodes");
    compare(record, live).expect("the binding set reads back")
}

fn catalog(tools: Vec<lash_core::ToolDefinition>) -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

/// The record names every referenced ambient path — the bound tool's whole
/// definition, or null — and leaves out paths a deferred resolution records.
#[test]
fn the_binding_set_records_each_referenced_path_once() {
    let live = catalog(vec![
        tool("tool:a", "a", "first"),
        tool("tool:b", "b", "second"),
    ]);
    let record = resolve_ambient_bindings(
        &referenced(&["app.a", "app.b", "app.gone", "app.deferred"]),
        &live,
        &referenced(&["app.deferred"]),
    )
    .expect("the binding set encodes");
    let serde_json::Value::Object(entries) = &record else {
        panic!("the record is an object");
    };
    assert_eq!(
        entries.keys().cloned().collect::<Vec<_>>(),
        ["app.a", "app.b", "app.gone"]
    );
    assert!(entries["app.gone"].is_null());
    assert_eq!(
        entries["app.a"],
        serde_json::to_value(tool("tool:a", "a", "first")).expect("a definition encodes"),
        "the whole definition is recorded"
    );
    let bindings = compare(record, &live).expect("the binding set reads back");
    assert!(!bindings.has_drift(), "a first pass is its own record");
}

/// A recorded tool the live registry no longer holds is missing; one it holds
/// under the same id with another definition is changed. Both link against
/// the recorded definition.
#[test]
fn a_removed_or_changed_tool_is_drift_and_links_as_recorded() {
    let original = catalog(vec![
        tool("tool:a", "a", "first"),
        tool("tool:b", "b", "second"),
    ]);
    let removed = catalog(vec![tool("tool:b", "b", "second")]);
    let bindings = recorded(&["app.a", "app.b"], &original, &removed);
    let drift = bindings
        .drift_for(&lash_core::ToolId::from("tool:a"))
        .expect("a removed tool drifted");
    assert_eq!(drift.kind, CellBindingDriftKind::Missing);
    assert_eq!(drift.path, "app.a");
    assert!(
        bindings
            .drift_for(&lash_core::ToolId::from("tool:b"))
            .is_none()
    );
    let refusal = drift.refusal();
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::LashlangCellBindingDrift
    );
    assert!(refusal.code.parks_turn());
    assert!(refusal.message.contains("`app.a`") && refusal.message.contains("missing"));
    let linked = bindings.link_catalog(&removed);
    assert!(
        linked
            .tools
            .iter()
            .any(|entry| entry.manifest.id.as_str() == "tool:a")
    );

    let changed = catalog(vec![
        retried(tool("tool:a", "a", "first")),
        tool("tool:b", "b", "second"),
    ]);
    let bindings = recorded(&["app.a", "app.b"], &original, &changed);
    let drift = bindings
        .drift_for(&lash_core::ToolId::from("tool:a"))
        .expect("a changed tool drifted");
    assert_eq!(drift.kind, CellBindingDriftKind::Changed);
    assert!(drift.refusal().message.contains("changed"));
    let linked = bindings.link_catalog(&changed);
    let linked_a = linked
        .tools
        .iter()
        .filter(|entry| entry.manifest.id.as_str() == "tool:a")
        .collect::<Vec<_>>();
    assert_eq!(linked_a.len(), 1, "the recorded tool replaces the live one");
    assert_eq!(
        linked_a[0].manifest.retry_policy,
        lash_core::ToolRetryPolicy::Never,
        "the recorded definition is linked"
    );
}

fn retried(mut definition: lash_core::ToolDefinition) -> lash_core::ToolDefinition {
    definition.manifest.retry_policy = lash_core::ToolRetryPolicy::Safe {
        max_attempts: 3,
        base_delay_ms: 10,
        max_delay_ms: 100,
    };
    definition
}

/// A reworded description or new examples only reach the model's prompt,
/// which a redrive serves from the journal: never drift.
#[test]
fn a_reworded_descriptor_is_not_drift() {
    let original = catalog(vec![tool("tool:a", "a", "first")]);
    let mut reworded = tool("tool:a", "a", "first, reworded at length");
    reworded.contract.examples = vec!["app.a({})".to_string()];
    let bindings = recorded(&["app.a"], &original, &catalog(vec![reworded]));
    assert!(!bindings.has_drift());
}

/// A live tool claiming a recorded path under another id links as recorded.
#[test]
fn another_tool_at_a_recorded_path_links_as_recorded() {
    let original = catalog(vec![tool("tool:a", "a", "first")]);
    let crowded = catalog(vec![
        tool("tool:a", "a", "first"),
        tool("tool:a2", "a", "a newcomer at the same path"),
    ]);
    let bindings = recorded(&["app.a"], &original, &crowded);
    assert!(!bindings.has_drift());
    let linked = bindings.link_catalog(&crowded);
    assert_eq!(
        linked
            .tools
            .iter()
            .map(|entry| entry.manifest.id.as_str())
            .collect::<Vec<_>>(),
        ["tool:a"],
        "the newcomer is masked at the recorded path"
    );
}

/// A path the record found unbound stays unbound when the live registry has
/// since bound it: the redrive links as the recorded pass did.
#[test]
fn a_path_recorded_unbound_stays_unbound() {
    let original = catalog(vec![tool("tool:b", "b", "second")]);
    let grown = catalog(vec![
        tool("tool:a", "a", "first"),
        tool("tool:b", "b", "second"),
    ]);
    let bindings = recorded(&["app.a", "app.b"], &original, &grown);
    assert!(!bindings.has_drift());
    let linked = bindings.link_catalog(&grown);
    assert!(
        linked
            .tools
            .iter()
            .all(|entry| entry.manifest.id.as_str() != "tool:a")
    );
}
