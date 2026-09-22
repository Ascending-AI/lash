use lash::rlm::lang::{ExecutionHostError, Record, Value};

use crate::{DisplayDelta, DisplayState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayOperation {
    kind: DisplayOperationKind,
    pub operation: &'static str,
    pub label: &'static str,
    pub fields: &'static [DisplayField],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayOperationKind {
    ShowMessage,
    SetStatus,
    AddItem,
    SetLight,
    SetProgress,
    Highlight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisplayField {
    pub name: &'static str,
    pub field_type: &'static str,
}

/// The host receiver every display operation is reached through. The catalog
/// serves it so the editor and the backend synthesize the same receiver call
/// instead of both hardcoding the name (FIG-3178).
pub(crate) const RECEIVER: &str = "display";

pub(crate) const OPERATIONS: &[DisplayOperation] = &[
    DisplayOperation {
        kind: DisplayOperationKind::ShowMessage,
        operation: "show_message",
        label: "Show message",
        fields: &[DisplayField {
            name: "text",
            field_type: "string",
        }],
    },
    DisplayOperation {
        kind: DisplayOperationKind::SetStatus,
        operation: "set_status",
        label: "Set status",
        fields: &[
            DisplayField {
                name: "key",
                field_type: "string",
            },
            DisplayField {
                name: "value",
                field_type: "string",
            },
        ],
    },
    DisplayOperation {
        kind: DisplayOperationKind::AddItem,
        operation: "add_item",
        label: "Add item",
        fields: &[
            DisplayField {
                name: "list",
                field_type: "string",
            },
            DisplayField {
                name: "item",
                field_type: "string",
            },
        ],
    },
    DisplayOperation {
        kind: DisplayOperationKind::SetLight,
        operation: "set_light",
        label: "Set light",
        fields: &[
            DisplayField {
                name: "name",
                field_type: "string",
            },
            DisplayField {
                name: "state",
                field_type: "string",
            },
        ],
    },
    DisplayOperation {
        kind: DisplayOperationKind::SetProgress,
        operation: "set_progress",
        label: "Set progress",
        fields: &[DisplayField {
            name: "pct",
            field_type: "number",
        }],
    },
    DisplayOperation {
        kind: DisplayOperationKind::Highlight,
        operation: "highlight",
        label: "Highlight",
        fields: &[DisplayField {
            name: "target",
            field_type: "string",
        }],
    },
];

pub(crate) fn apply_tool(
    display: &mut DisplayState,
    operation: &str,
    args: &[Value],
) -> Result<(Value, DisplayDelta), ExecutionHostError> {
    let display_operation = OPERATIONS
        .iter()
        .find(|candidate| candidate.operation == operation)
        .ok_or_else(|| {
            ExecutionHostError::new(format!("unknown display operation `{operation}`"))
        })?;
    let args = args.first().and_then(Value::as_record).ok_or_else(|| {
        ExecutionHostError::new(format!("{operation} expects one record argument"))
    })?;
    let mut delta = DisplayDelta::default();
    match display_operation.kind {
        DisplayOperationKind::ShowMessage => {
            let text = string_arg(args, "text")?;
            display.messages.push(text.clone());
            delta.messages_appended.push(text);
        }
        DisplayOperationKind::SetStatus => {
            let key = string_arg(args, "key")?;
            let value = string_arg(args, "value")?;
            display.statuses.insert(key.clone(), value.clone());
            delta.statuses.insert(key, value);
        }
        DisplayOperationKind::AddItem => {
            let list = string_arg(args, "list")?;
            let item = scalar_text_arg(args, "item")?;
            display
                .lists
                .entry(list.clone())
                .or_default()
                .push(item.clone());
            delta
                .list_items_appended
                .entry(list)
                .or_default()
                .push(item);
        }
        DisplayOperationKind::SetLight => {
            let name = string_arg(args, "name")?;
            let state = scalar_text_arg(args, "state")?;
            display.lights.insert(name.clone(), state.clone());
            delta.lights.insert(name, state);
        }
        DisplayOperationKind::SetProgress => {
            let pct = number_arg(args, "pct")?.clamp(0.0, 100.0);
            display.progress = pct;
            delta.progress = Some(pct);
        }
        DisplayOperationKind::Highlight => {
            let target = string_arg(args, "target")?;
            display.highlighted = Some(target.clone());
            delta.highlighted = Some(target);
        }
    }
    Ok((Value::Null, delta))
}

fn string_arg(args: &Record, key: &str) -> Result<String, ExecutionHostError> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value.to_string()),
        _ => Err(ExecutionHostError::new(format!(
            "missing string argument `{key}`"
        ))),
    }
}

fn number_arg(args: &Record, key: &str) -> Result<f64, ExecutionHostError> {
    match args.get(key) {
        Some(Value::Number(value)) => Ok(*value),
        _ => Err(ExecutionHostError::new(format!(
            "missing number argument `{key}`"
        ))),
    }
}

fn scalar_text_arg(args: &Record, key: &str) -> Result<String, ExecutionHostError> {
    match args.get(key) {
        Some(Value::String(value)) => Ok(value.to_string()),
        Some(Value::Number(value)) => Ok(value.to_string()),
        Some(Value::Bool(value)) => Ok(value.to_string()),
        _ => Err(ExecutionHostError::new(format!(
            "missing scalar argument `{key}`"
        ))),
    }
}
