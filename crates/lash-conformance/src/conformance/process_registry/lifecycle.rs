use super::*;
use lash_core::{LifetimeDecision, ScopeGrant, ScopeId};
use pretty_assertions::assert_eq;

/// The recorded lifetime and ancestry of a registration, and admission
/// against closure (FIG-3607 R3, R4b, R11).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn registration_contract(registry: Arc<dyn crate::ConformanceProcessRegistry>) {
    let parent = registry
        .register_process(registration("lifecycle-parent"))
        .await
        .expect("register parent");
    let parent_scope = ScopeId::process(parent.id.clone());
    // The child starts under a key, so a replay of its start after the
    // parent ended returns the retained child instead of starting a new one.
    let child = crate::started_until_starter(
        registration("lifecycle-child").with_start_key(Some(crate::StartKey::for_host(
            crate::StartKeyOwner::HOST,
            "lifecycle-child",
        ))),
        parent_scope.clone(),
    );
    let admitted = registry
        .register_process(child.clone())
        .await
        .expect("a live starter admits its child");
    assert_eq!(
        admitted.lifetime,
        LifetimeDecision::Until {
            scope: parent_scope.clone(),
            grant: ScopeGrant::Ancestor,
        },
        "the recorded lifetime is the decision the start carried"
    );
    assert_eq!(
        admitted.ancestry.scopes(),
        std::slice::from_ref(&parent_scope)
    );
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
            .get_parent_end_plan(&parent_scope)
            .await
            .expect("read scope-close row")
            .is_some(),
        "a terminal process closes its own scope with the terminal append"
    );
    assert_eq!(
        registry
            .register_process(child)
            .await
            .expect("a retained key's replay after the starter ended"),
        admitted
    );
    // A new start under the ended process is refused whatever its lifetime:
    // `Until` it, and `Detached` alike (R11).
    let late =
        crate::started_until_starter(registration("lifecycle-late-child"), parent_scope.clone());
    assert!(
        matches!(registry.register_process(late).await, Err(PluginError::ParentEnded { parent: ref scope, .. }) if scope == &parent_scope)
    );
    let late_detached = crate::started_detached(
        registration("lifecycle-late-detached"),
        parent_scope.clone(),
    );
    assert!(
        matches!(registry.register_process(late_detached).await, Err(PluginError::ParentEnded { parent: ref scope, .. }) if scope == &parent_scope),
        "a detached start is refused once its starter has ended"
    );
    let root = registry
        .register_process(registration("lifecycle-detached-root"))
        .await
        .expect("a detached root is admitted");
    assert_eq!(root.lifetime, LifetimeDecision::Detached);
    assert!(root.ancestry.is_root());
    // A root cannot name a scope it was never admitted under (R3).
    let mut unreachable = registration("lifecycle-unreachable");
    unreachable.lifetime = LifetimeDecision::Until {
        scope: ScopeId::turn("lifecycle-session", "lifecycle-turn"),
        grant: ScopeGrant::Ancestor,
    };
    assert!(
        registry.register_process(unreachable).await.is_err(),
        "a lifetime scope outside the ancestry is refused"
    );
    // A host session grant is a root's alone.
    let mut escaped = crate::started_detached(
        registration("lifecycle-escaped-grant"),
        ScopeId::turn("lifecycle-session", "lifecycle-turn"),
    );
    escaped.lifetime = LifetimeDecision::Until {
        scope: ScopeId::session("lifecycle-session"),
        grant: ScopeGrant::HostSessionLookup,
    };
    assert!(
        registry.register_process(escaped).await.is_err(),
        "a host session grant on a runtime start is refused"
    );
    let turn = ScopeId::turn("lifecycle-session", "lifecycle-turn");
    let turn_child = crate::started_until(
        registration("lifecycle-turn-child"),
        turn.clone(),
        ScopeId::session("lifecycle-session"),
    );
    assert_eq!(
        registry
            .register_process(turn_child)
            .await
            .expect("a turn's child may live until the turn's session")
            .lifetime
            .scope(),
        Some(&ScopeId::session("lifecycle-session"))
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn empty_tool_call_identifiers_leave_no_row(
    registry: Arc<dyn crate::ConformanceProcessRegistry>,
) {
    let cases = [
        (
            "empty-call-id",
            "",
            "tool",
            "process `keyless start` tool call must carry a call id",
        ),
        (
            "whitespace-call-id",
            "  ",
            "tool",
            "process `keyless start` tool call must carry a call id",
        ),
        (
            "empty-tool-name",
            "call",
            "",
            "process `keyless start` tool call must carry a tool name",
        ),
        (
            "whitespace-tool-name",
            "call",
            "\t",
            "process `keyless start` tool call must carry a tool name",
        ),
    ];

    for (label, call_id, tool_name, expected) in cases {
        let before = registry
            .list_processes(&ProcessListFilter::default())
            .await
            .expect("list before refused tool-call registration")
            .len();
        let registration = ProcessRegistration::new(
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
            ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
        .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(format!(
            "process-env:{label}"
        ))));

        assert_session_refusal(registry.register_process(registration).await, expected);
        assert_eq!(
            registry
                .list_processes(&ProcessListFilter::default())
                .await
                .expect("list after refused tool-call registration")
                .len(),
            before,
            "a refused tool-call registration must not leave a listed row"
        );
    }
}
