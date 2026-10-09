use lash::vm::{ExecutionHostError, Record, Value};

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

/// Example-owned records, deduplicated by Lash's stable call id. Display
/// delivery reads these after the observed call completes from its recorded result.
/// A production host persists this ledger alongside its external effects.
#[derive(Clone, Default)]
pub(crate) struct HostTools {
    display: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<DisplayCall>>>>,
    approvals: std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, ApprovalCall>>>,
}

#[derive(Clone)]
pub(crate) struct DisplayCall {
    pub call_id: String,
    pub operation: String,
    pub args: serde_json::Value,
}

struct ApprovalCall {
    process: lash::ProcessId,
    key: String,
}

impl HostTools {
    pub(crate) fn display_calls(&self, process: &lash::ProcessId) -> Vec<DisplayCall> {
        use lash::sync::MutexExt;
        self.display
            .lock_recover()
            .get(process.as_str())
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn approval_key(&self, process: &lash::ProcessId, call_id: &str) -> Option<String> {
        use lash::sync::MutexExt;
        self.approvals
            .lock_recover()
            .get(call_id)
            .filter(|call| call.process == *process)
            .map(|call| call.key.clone())
    }

    pub(crate) fn forget_approval(&self, key: &str) {
        use lash::sync::MutexExt;
        self.approvals
            .lock_recover()
            .retain(|_, call| call.key != key);
    }
}

#[lash::async_trait]
impl lash::tools::StaticToolExecute for HostTools {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        use lash::sync::MutexExt;
        use lash::tools::{PendingCompletion, ToolAttemptOutcome, ToolOutcome};
        if call.name() == "host_approval" {
            let Some(process) = call.context.enclosing_process() else {
                return ToolOutcome::err_fmt("approval requires a workflow process").into();
            };
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(error) => return ToolOutcome::err_fmt(error).into(),
            };
            self.approvals
                .lock_recover()
                .entry(call.context.call_id().to_string())
                .or_insert_with(|| ApprovalCall {
                    process: process.clone(),
                    key: key.as_str().into(),
                });
            return ToolAttemptOutcome::pending(PendingCompletion::new());
        }
        let Some(operation) = call.name().strip_prefix("display_") else {
            return match crate::sample_tools::apply_tool(
                call.name(),
                &[lash::vm::from_json(call.args.clone())],
            ) {
                Ok(value) => ToolOutcome::ok(value).into(),
                Err(error) => ToolOutcome::err_fmt(error).into(),
            };
        };
        if let Err(error) = apply_tool(
            &mut DisplayState::default(),
            operation,
            &[lash::vm::from_json(call.args.clone())],
        ) {
            return ToolOutcome::err_fmt(error).into();
        }
        let Some(process) = call.context.enclosing_process() else {
            return ToolOutcome::err_fmt("display tools require a durable workflow process").into();
        };
        let mut ledger = self.display.lock_recover();
        let records = ledger.entry(process.to_string()).or_default();
        let call_id = call.context.call_id().to_string();
        if !records.iter().any(|record| record.call_id == call_id) {
            records.push(DisplayCall {
                call_id,
                operation: operation.into(),
                args: call.args.clone(),
            });
        }
        ToolOutcome::ok(serde_json::Value::Null).into()
    }
}
