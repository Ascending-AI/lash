use serde_json::{Value, json};

use crate::display;
use crate::mock_tools;
use crate::{OperationCatalogEntry, OperationField};

pub(crate) fn entries() -> Vec<OperationCatalogEntry> {
    let mut entries = display::OPERATIONS
        .iter()
        .map(|operation| OperationCatalogEntry {
            id: format!("display.{}", operation.operation),
            label: operation.label.to_string(),
            node_kind: "call".to_string(),
            subkind: None,
            operation: Some(operation.operation.to_string()),
            effect: None,
            terminal_kind: None,
            fields: operation
                .fields
                .iter()
                .map(|field| OperationField {
                    name: field.name.to_string(),
                    field_type: field.field_type.to_string(),
                    default: display_default(field.field_type),
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    entries.extend(mock_tools::OPERATIONS.iter().map(|operation| {
        OperationCatalogEntry {
            id: format!("{}.{}", operation.module, operation.operation),
            label: operation.label.to_string(),
            node_kind: "call".to_string(),
            subkind: None,
            operation: Some(operation.operation.to_string()),
            effect: None,
            terminal_kind: None,
            fields: operation
                .fields
                .iter()
                .map(|field| OperationField {
                    name: field.name.to_string(),
                    field_type: field.field_type.to_string(),
                    default: field.default.json(),
                })
                .collect(),
        }
    }));
    entries.extend([
        entry(
            "proc.process",
            "Process",
            "process",
            None,
            None,
            None,
            None,
            vec![field("name", "identifier", json!("my_process"))],
        ),
        entry(
            "effect.sleep",
            "Sleep",
            "effect",
            None,
            None,
            Some("sleep"),
            None,
            vec![field("duration", "expression", json!("\"1s\""))],
        ),
        entry(
            "effect.wait_signal",
            "Wait for signal",
            "effect",
            None,
            None,
            Some("wait_signal"),
            None,
            vec![field("signal", "string", json!("continue"))],
        ),
        entry(
            "control.if",
            "If / branch",
            "container",
            Some("if"),
            None,
            None,
            None,
            vec![field("condition", "expression", json!("true"))],
        ),
        entry(
            "control.while",
            "While loop",
            "container",
            Some("while"),
            None,
            None,
            None,
            vec![field("condition", "expression", json!("false"))],
        ),
        entry(
            "control.for",
            "For each",
            "container",
            Some("for"),
            None,
            None,
            None,
            vec![
                field("binding", "identifier", json!("item")),
                field("iterable", "expression", json!("[1, 2, 3]")),
            ],
        ),
        entry(
            "stmt.assign",
            "Set variable",
            "state_update",
            None,
            None,
            None,
            None,
            vec![
                field("target", "assignment_target", json!("state.count")),
                field("expression", "expression", json!("0")),
            ],
        ),
        entry(
            "stmt.let",
            "Define value",
            "data",
            None,
            None,
            None,
            None,
            vec![
                field("binding", "identifier", json!("value")),
                field("expression", "expression", json!("0")),
            ],
        ),
        entry(
            "stmt.compute",
            "Compute",
            "computation",
            None,
            None,
            None,
            None,
            vec![field("expression", "expression", json!("1 + 1"))],
        ),
        entry(
            "stmt.opaque",
            "Raw statement",
            "opaque",
            None,
            None,
            None,
            None,
            vec![field(
                "source",
                "expression",
                json!(r#"await display.show_message({ text: "raw" })"#),
            )],
        ),
        entry(
            "stmt.finish",
            "Finish",
            "terminal",
            None,
            None,
            None,
            Some("finish"),
            vec![field("expression", "expression", json!("0"))],
        ),
    ]);
    entries
}

fn display_default(field_type: &str) -> Value {
    match field_type {
        "number" => json!(0),
        _ => json!(""),
    }
}

#[allow(clippy::too_many_arguments)]
fn entry(
    id: &str,
    label: &str,
    node_kind: &str,
    subkind: Option<&str>,
    operation: Option<&str>,
    effect: Option<&str>,
    terminal_kind: Option<&str>,
    fields: Vec<OperationField>,
) -> OperationCatalogEntry {
    OperationCatalogEntry {
        id: id.to_string(),
        label: label.to_string(),
        node_kind: node_kind.to_string(),
        subkind: subkind.map(str::to_string),
        operation: operation.map(str::to_string),
        effect: effect.map(str::to_string),
        terminal_kind: terminal_kind.map(str::to_string),
        fields,
    }
}

fn field(name: &str, field_type: &str, default: Value) -> OperationField {
    OperationField {
        name: name.to_string(),
        field_type: field_type.to_string(),
        default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_display_tool_has_exactly_one_catalog_entry() {
        let entries = entries();
        let display_entries = entries
            .iter()
            .filter(|entry| entry.id.starts_with("display."))
            .collect::<Vec<_>>();
        assert_eq!(display_entries.len(), display::OPERATIONS.len());
        for operation in display::OPERATIONS {
            let entry = display_entries
                .iter()
                .find(|entry| entry.operation.as_deref() == Some(operation.operation))
                .expect("display operation catalog entry");
            assert_eq!(entry.id, format!("display.{}", operation.operation));
            assert_eq!(entry.fields.len(), operation.fields.len());
        }
    }

    #[test]
    fn every_mocked_tool_has_exactly_one_catalog_entry() {
        let entries = entries();
        for operation in mock_tools::OPERATIONS {
            let id = format!("{}.{}", operation.module, operation.operation);
            let matching = entries
                .iter()
                .filter(|entry| entry.id == id)
                .collect::<Vec<_>>();
            assert_eq!(matching.len(), 1, "catalog entry for {id}");
            assert_eq!(matching[0].fields.len(), operation.fields.len());
        }
    }
}
