use super::*;
use lash_core::{OnParentEnd, ParentScope, ProcessLifecyclePolicy};
use pretty_assertions::assert_eq;

pub(super) async fn registration_contract(registry: Arc<dyn crate::ConformanceProcessRegistry>) {
    let parent = registry
        .register_process(registration(&ProcessId::from("lifecycle-parent")))
        .await
        .expect("register parent");
    let policy = ProcessLifecyclePolicy::new(
        ParentScope::Process {
            process_id: parent.id.clone(),
            incarnation: parent.incarnation,
        },
        OnParentEnd::Cancel,
    );
    let mut child = registration(&ProcessId::from("lifecycle-child"));
    child.lifecycle = policy.clone();
    let admitted = registry
        .register_process(child.clone())
        .await
        .expect("live parent admits child");
    assert_eq!(admitted.lifecycle, policy);
    registry
        .complete_process(
            &parent.id,
            settled_success(serde_json::json!("done")),
            crate::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("complete action-free parent");
    assert!(
        registry
            .get_process(&parent.id)
            .await
            .expect("read parent")
            .expect("retained parent")
            .is_terminal()
    );
    assert!(
        registry
            .get_pending_parent_end_plan(&parent.id)
            .await
            .expect("read legacy plan")
            .is_none(),
        "the refusal must not depend on a pending action list"
    );
    assert_eq!(
        registry
            .register_process(child)
            .await
            .expect("identical replay after parent end"),
        admitted
    );
    let mut late = registration(&ProcessId::from("lifecycle-late-child"));
    late.lifecycle = policy.clone();
    assert!(
        matches!(registry.register_process(late).await, Err(PluginError::ParentEnded { parent: ref scope, .. }) if scope == &policy.parent)
    );
    let mut detached = registration(&ProcessId::from("lifecycle-detached-child"));
    detached.lifecycle = ProcessLifecyclePolicy::new(policy.parent, OnParentEnd::Abandon);
    let detached_policy = detached.lifecycle.clone();
    assert_eq!(
        registry
            .register_process(detached)
            .await
            .expect("Abandon child is host managed")
            .lifecycle,
        detached_policy
    );
    let mut invalid = registration(&ProcessId::from("lifecycle-invalid-host"));
    invalid.lifecycle.on_parent_end = OnParentEnd::Cancel;
    assert!(
        registry.register_process(invalid).await.is_err(),
        "Host never ends"
    );
    let mut turn_child = registration(&ProcessId::from("lifecycle-turn-child"));
    turn_child.lifecycle = ProcessLifecyclePolicy::new(
        ParentScope::Turn {
            session_id: crate::SessionId::from("lifecycle-session"),
            turn_id: crate::TurnId::from("lifecycle-turn"),
        },
        OnParentEnd::Cancel,
    );
    assert!(matches!(
        turn_child.provenance.originator,
        crate::ProcessOriginator::Host { .. }
    ));
    assert!(
        registry.register_process(turn_child.clone()).await.is_err(),
        "turn parent must match session originator"
    );
    turn_child.provenance.originator =
        crate::ProcessOriginator::session(crate::SessionScope::new("lifecycle-session"));
    let turn_policy = turn_child.lifecycle.clone();
    assert_eq!(
        registry
            .register_process(turn_child)
            .await
            .expect("matching turn parent is admitted")
            .lifecycle,
        turn_policy
    );
}

pub(super) async fn empty_tool_call_identifiers_leave_no_row(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let cases = [
        (
            "empty-call-id",
            "",
            "tool",
            "process `empty-call-id` tool call must carry a call id",
        ),
        (
            "whitespace-call-id",
            "  ",
            "tool",
            "process `whitespace-call-id` tool call must carry a call id",
        ),
        (
            "empty-tool-name",
            "call",
            "",
            "process `empty-tool-name` tool call must carry a tool name",
        ),
        (
            "whitespace-tool-name",
            "call",
            "\t",
            "process `whitespace-tool-name` tool call must carry a tool name",
        ),
    ];

    for (process_id, call_id, tool_name, expected) in cases {
        let process_id = ProcessId::from(process_id);
        let registration = ProcessRegistration::new(
            &process_id,
            ProcessInput::ToolCall {
                call: crate::PreparedToolCall::from_parts(
                    call_id,
                    crate::ToolId::new("tool-id"),
                    tool_name,
                    serde_json::json!({}),
                    None,
                    serde_json::Value::Null,
                ),
            },
            RecoveryContract::Rerunnable,
            ProcessProvenance::host(),
            ProcessLifecyclePolicy::new(ParentScope::Host, OnParentEnd::Abandon),
        )
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:{process_id}"
        ))));

        assert_session_refusal(registry.register_process(registration).await, expected);
        assert!(
            registry
                .get_process(&process_id)
                .await
                .expect("read refused tool-call process")
                .is_none(),
            "a refused tool-call registration must not leave a point-readable row"
        );
        assert!(
            !registry
                .list_processes(&ProcessListFilter::default())
                .await
                .expect("list after refused tool-call registration")
                .iter()
                .any(|record| record.id == process_id),
            "a refused tool-call registration must not leave a listed row"
        );
    }
}
