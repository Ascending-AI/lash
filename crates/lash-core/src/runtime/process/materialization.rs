use crate::plugin::PluginError;

use super::events::{
    AbandonEvidence, AbandonWriter, ProcessAwaitOutput, ProcessEventSemantics,
    ProcessEventSemanticsSpec, ProcessTerminalSemantics, ProcessTerminalSpec, ProcessTerminalState,
    ProcessValueSelector, ProcessWake, ProcessWakeSpec,
};

pub fn materialize_process_event_semantics(
    process_id: &str,
    sequence: u64,
    payload: &serde_json::Value,
    spec: &ProcessEventSemanticsSpec,
) -> Result<ProcessEventSemantics, PluginError> {
    materialize_event_semantics(process_id, sequence, payload, spec)
}

pub(super) fn materialize_event_semantics(
    process_id: &str,
    sequence: u64,
    payload: &serde_json::Value,
    spec: &ProcessEventSemanticsSpec,
) -> Result<ProcessEventSemantics, PluginError> {
    let terminal = spec
        .terminal
        .as_ref()
        .map(|terminal| materialize_terminal_semantics(payload, terminal))
        .transpose()?;
    let wake = spec
        .wake
        .as_ref()
        .map(|wake| materialize_wake(process_id, sequence, payload, wake))
        .transpose()?
        .flatten();
    Ok(ProcessEventSemantics { terminal, wake })
}

fn materialize_terminal_semantics(
    payload: &serde_json::Value,
    terminal: &ProcessTerminalSpec,
) -> Result<ProcessTerminalSemantics, PluginError> {
    let await_output = match &terminal.await_output {
        Some(selector) => {
            let selected = select_value(payload, selector)?;
            serde_json::from_value::<ProcessAwaitOutput>(selected.clone())
                .unwrap_or_else(|_| selected_value_to_await_output(terminal.state, selected))
        }
        None if terminal.state == ProcessTerminalState::Completed => ProcessAwaitOutput::Success {
            value: payload.clone(),
            control: None,
        },
        None => {
            return Err(PluginError::Session(
                "failed or cancelled terminal events must declare await output".to_string(),
            ));
        }
    };
    Ok(ProcessTerminalSemantics {
        state: terminal.state,
        await_output,
    })
}

fn selected_value_to_await_output(
    state: ProcessTerminalState,
    value: serde_json::Value,
) -> ProcessAwaitOutput {
    match state {
        ProcessTerminalState::Completed => ProcessAwaitOutput::Success {
            value,
            control: None,
        },
        ProcessTerminalState::Failed => ProcessAwaitOutput::Failure {
            class: crate::ToolFailureClass::Execution,
            code: "process_failed".to_string(),
            message: selector_value_to_string(&value),
            raw: Some(value),
            control: None,
        },
        ProcessTerminalState::Cancelled => ProcessAwaitOutput::Cancelled {
            message: selector_value_to_string(&value),
            raw: Some(value),
            control: None,
        },
        // Reached only if a producer declares its own `Abandoned` terminal event
        // and emits a raw value (not a serialized `ProcessAwaitOutput`); the
        // sweep/drain path writes structured evidence through `complete_process`,
        // which deserializes directly above. With no structured evidence to carry,
        // synthesize a minimal owner-drain marker.
        ProcessTerminalState::Abandoned => ProcessAwaitOutput::Abandoned {
            evidence: Box::new(AbandonEvidence {
                writer: AbandonWriter::OwnerDrain,
                owner: None,
                epoch_ms: 0,
            }),
            control: None,
        },
    }
}

fn materialize_wake(
    process_id: &str,
    sequence: u64,
    payload: &serde_json::Value,
    wake: &ProcessWakeSpec,
) -> Result<Option<ProcessWake>, PluginError> {
    if let Some(when) = &wake.when {
        let selected = select_value(payload, when)?;
        if !selector_value_is_truthy(&selected) {
            return Ok(None);
        }
    }
    let input = selector_value_to_string(&select_value(payload, &wake.input)?);
    let _ = (process_id, sequence);
    Ok(Some(ProcessWake { input }))
}

pub(super) fn select_value(
    payload: &serde_json::Value,
    selector: &ProcessValueSelector,
) -> Result<serde_json::Value, PluginError> {
    match selector {
        ProcessValueSelector::Payload => Ok(payload.clone()),
        ProcessValueSelector::Pointer(pointer) => {
            payload.pointer(pointer).cloned().ok_or_else(|| {
                PluginError::Session(format!("payload pointer `{pointer}` did not match"))
            })
        }
        ProcessValueSelector::Const(value) => Ok(value.clone()),
        ProcessValueSelector::Template { template, fields } => {
            let mut rendered = template.clone();
            for (name, selector) in fields {
                let value = select_value(payload, selector)?;
                rendered =
                    rendered.replace(&format!("{{{name}}}"), &selector_value_to_string(&value));
            }
            Ok(serde_json::Value::String(rendered))
        }
        ProcessValueSelector::Present(pointer) => {
            Ok(serde_json::Value::Bool(payload.pointer(pointer).is_some()))
        }
    }
}

fn selector_value_to_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn selector_value_is_truthy(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::String(value) => !value.is_empty(),
        serde_json::Value::Array(value) => !value.is_empty(),
        serde_json::Value::Object(value) => !value.is_empty(),
        serde_json::Value::Number(_) => true,
    }
}
