use super::*;
use std::ffi::OsString;

#[test]
fn completed_workflow_manifest_path_resolves_unset_empty_and_set_values() {
    assert_eq!(completed_workflow_manifest_path(None), None);
    assert_eq!(
        completed_workflow_manifest_path(Some(OsString::new())),
        None
    );
    assert_eq!(
        completed_workflow_manifest_path(Some(OsString::from("/tmp/completed.txt"))),
        Some(PathBuf::from("/tmp/completed.txt"))
    );
}

#[test]
fn captured_settled_shape_is_success() {
    let await_output = serde_json::json!({
        "type": "settled",
        "output": {
            "outcome": {
                "status": "success",
                "payload": {
                    "$lash_tool_value": "untrusted_json",
                    "value": {
                        "first": {"phase": "first"},
                        "second": {"phase": "second"}
                    }
                }
            }
        }
    });
    assert_eq!(
        signal_process_output_value(await_output).unwrap(),
        json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })
    );
}

#[test]
fn constructed_success_is_unwrapped_and_classified() {
    let await_output = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::success(json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })),
    );

    assert_eq!(
        signal_process_output_value(serde_json::to_value(await_output).unwrap()).unwrap(),
        json!({
            "first": {"phase": "first"},
            "second": {"phase": "second"}
        })
    );
}

#[test]
fn failure_and_abandoned_outputs_are_rejected() {
    let failure = lash_core::ProcessAwaitOutput::from_tool_output(
        lash_core::ToolCallOutput::failure(lash_core::ToolFailure::runtime(
            lash_core::ToolFailureClass::External,
            "signal_failed",
            "signal failed",
        )),
    );
    let abandoned = lash_core::ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::EngineGaveUp,
            owner: None,
            epoch_ms: 1,
        }),
        control: None,
    };

    assert!(signal_process_output_value(serde_json::to_value(failure).unwrap()).is_err());
    assert!(signal_process_output_value(serde_json::to_value(abandoned).unwrap()).is_err());
}
