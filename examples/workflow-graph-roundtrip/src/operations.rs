use crate::display;
use crate::sample_tools;
use crate::{EditableValue, OperationCatalogEntry, OperationField};

pub(crate) fn entries() -> Vec<OperationCatalogEntry> {
    let mut entries = display::OPERATIONS
        .iter()
        .map(|operation| OperationCatalogEntry {
            id: format!("display.{}", operation.operation),
            label: operation.label.to_string(),
            node_kind: "call".to_string(),
            subkind: None,
            operation: Some(operation.operation.to_string()),
            receiver: Some(display::RECEIVER.to_string()),
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
    entries.extend(sample_tools::OPERATIONS.iter().map(|operation| {
        OperationCatalogEntry {
            id: format!("{}.{}", operation.module, operation.operation),
            label: operation.label.to_string(),
            node_kind: "call".to_string(),
            subkind: None,
            operation: Some(operation.operation.to_string()),
            receiver: Some(operation.module.to_string()),
            effect: None,
            terminal_kind: None,
            fields: operation
                .fields
                .iter()
                .map(|field| OperationField {
                    name: field.name.to_string(),
                    field_type: field.field_type.to_string(),
                    default: field.default.editable(),
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
            vec![field("name", "identifier", "my_process")],
        ),
        entry(
            "effect.sleep",
            "Sleep",
            "effect",
            None,
            None,
            Some("sleep_for"),
            None,
            vec![field("duration", "expression", "\"1s\"")],
        ),
        entry(
            "effect.wait_signal",
            "Wait for signal",
            "effect",
            None,
            None,
            Some("wait_signal"),
            None,
            vec![field("signal", "string", "continue")],
        ),
        entry(
            "control.if",
            "If / branch",
            "container",
            Some("if"),
            None,
            None,
            None,
            vec![field("condition", "expression", "true")],
        ),
        entry(
            "control.while",
            "While loop",
            "container",
            Some("while"),
            None,
            None,
            None,
            vec![field("condition", "expression", "false")],
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
                field("binding", "identifier", "item"),
                field("iterable", "expression", "[1, 2, 3]"),
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
                field("target", "assignment_target", "state.count"),
                field("expression", "expression", "0"),
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
                field("binding", "identifier", "value"),
                field("expression", "expression", "0"),
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
            vec![field("expression", "expression", "1 + 1")],
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
                r#"await display.show_message({ text: "raw" })"#,
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
            vec![field("expression", "expression", "0")],
        ),
    ]);
    entries
}

fn display_default(field_type: &str) -> EditableValue {
    match field_type {
        "number" => EditableValue::Number(0.0),
        _ => EditableValue::String(String::new()),
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
        // Only catalog entries that synthesize a receiver call carry a
        // receiver, and every one of those is built above from its own
        // module rather than through this helper.
        receiver: None,
        effect: effect.map(str::to_string),
        terminal_kind: terminal_kind.map(str::to_string),
        fields,
    }
}

fn field(name: &str, field_type: &str, default: &str) -> OperationField {
    OperationField {
        name: name.to_string(),
        field_type: field_type.to_string(),
        default: if field_type == "expression" {
            EditableValue::Expr(default.to_string())
        } else {
            EditableValue::String(default.to_string())
        },
    }
}

use lash::rlm::lang::{
    LashlangAbilities, LashlangHostCatalog, LashlangHostEnvironment, LashlangLanguageFeatures,
    OperationContract,
};

#[expect(
    clippy::expect_used,
    reason = "the declared tool schemas and bindings are valid"
)]
pub(crate) fn host_environment() -> LashlangHostEnvironment {
    use lash::tools::{ToolBindingResolutionExt, ToolManifestBindingExt, ToolOutputContract};
    let mut catalog = LashlangHostCatalog::new();
    for definition in tool_definitions() {
        let binding = definition
            .manifest()
            .tool_binding()
            .expect("tool binding decodes")
            .expect("every example tool declares its binding")
            .executable_for(definition.name())
            .expect("valid binding");
        let contract = definition.contract();
        let operation = match contract.output_contract {
            ToolOutputContract::Static => OperationContract::new(
                contract.input_schema.canonical().clone(),
                contract.output_schema.canonical().clone(),
            ),
            ToolOutputContract::FromInputSchema {
                input_field,
                default_schema,
            } => OperationContract::from_input_field(
                contract.input_schema.canonical().clone(),
                input_field,
                default_schema.map(|schema| schema.as_value().clone()),
            ),
        };
        catalog
            .add_module_operation_contract(
                binding.module_path,
                binding.authority_type,
                binding.operation,
                definition.id().as_str(),
                &operation,
            )
            .expect("unique tool binding");
    }
    LashlangHostEnvironment::new(catalog, LashlangAbilities::all())
        .with_language_features(LashlangLanguageFeatures::default().with_label_annotations())
}

#[expect(
    clippy::expect_used,
    reason = "the example declares valid schemas and bindings"
)]
pub(crate) fn tool_definitions() -> Vec<lash::tools::ToolDefinition> {
    use lash::tools::{ToolBinding, ToolDefinition, ToolDefinitionBindingExt};
    let mut definitions = Vec::new();
    for operation in crate::display::OPERATIONS {
        let properties = operation
            .fields
            .iter()
            .map(|field| {
                let schema = match (operation.operation, field.name, field.field_type) {
                    ("add_item", "item", _) | ("set_light", "state", _) => {
                        serde_json::json!({"type": ["string", "number", "boolean"]})
                    }
                    (_, _, "number") => serde_json::json!({"type": "number"}),
                    _ => serde_json::json!({"type": "string"}),
                };
                (field.name.to_owned(), schema)
            })
            .collect::<serde_json::Map<_, _>>();
        let name = format!("display_{}", operation.operation);
        definitions.push(ToolDefinition::raw(format!("tool:{name}"), name, operation.label,
            serde_json::json!({"type":"object", "properties":properties, "required":operation.fields.iter().map(|field| field.name).collect::<Vec<_>>(), "additionalProperties":false}),
            serde_json::json!({"type":"null"})).expect("display schema")
            .with_tool_binding(ToolBinding::new(["display"], operation.operation).with_authority_type("ToyDisplay")));
    }
    for operation in crate::sample_tools::OPERATIONS {
        let name = operation.host_operation.replace('.', "_");
        let definition = ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            operation.label,
            operation.input_schema(),
            operation.output_schema(),
        )
        .expect("example schema")
        .with_tool_binding(
            ToolBinding::new([operation.module], operation.operation)
                .with_authority_type(operation.resource_type),
        );
        definitions.push(match operation.output_from_input() {
            Some((field, default)) => definition.with_output_from_input_schema(
                field,
                default.map(|schema| {
                    lash::schema::JsonSchema::admit(schema).expect("valid output schema")
                }),
            ),
            None => definition,
        });
    }
    definitions
}
