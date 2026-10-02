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
        registration("lifecycle-child")
            .with_start_key(Some(crate::StartKey::for_host("lifecycle-child"))),
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
pub(super) async fn a_resume_event_cannot_return_an_ended_process_to_running(
    registry: Arc<dyn ProcessRegistry>,
) {
    let registered = registry
        .register_process(executed_registration("resume-after-terminal"))
        .await
        .expect("register the process");
    let process_id = registered.id.clone();
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "resume-after-terminal:execution",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &authority,
        )
        .await
        .expect("record the execution start");
    registry
        .complete_process(
            &process_id,
            settled_success(serde_json::json!("done")),
            ProcessCompletionAuthority::workflow_key(process_id.as_str()),
        )
        .await
        .expect("end the process");
    let ended = registry
        .get_process(&process_id)
        .await
        .expect("read the ended process")
        .expect("the ended process is retained");
    assert_eq!(ended.status(), ProcessStatus::Completed);
    assert_eq!(
        ended.outcome(),
        Some(settled_success(serde_json::json!("done"))),
        "the outcome is the state the status derives from"
    );

    let wait = crate::WaitState {
        since_ms: 1,
        kind: crate::WaitKind::Signal {
            name: "ready".to_string(),
            event_type: "signal.ready".to_string(),
            key: format!("{process_id}:signal.ready:1"),
            ordinal: 1,
        },
    };
    let error = registry
        .append_event(
            &process_id,
            ProcessEventAppendRequest::wait_cleared(&process_id, &wait),
        )
        .await
        .expect_err("a resume cannot take an outcome back");
    assert!(
        matches!(
            &error,
            crate::PluginError::ProcessAlreadyTerminal {
                process_id: refused,
                status: ProcessStatus::Completed,
            } if *refused == process_id
        ),
        "unexpected refusal: {error:?}"
    );
    let after = registry
        .get_process(&process_id)
        .await
        .expect("read the process after the refused resume")
        .expect("the ended process is still retained");
    assert_eq!(
        after, ended,
        "the refused resume wrote nothing: the record keeps its outcome and status"
    );
    assert_eq!(after.status(), ProcessStatus::Completed);
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn lifecycle_transition_refusals_are_backend_invariant(
    registry: Arc<dyn ProcessRegistry>,
) {
    let departed_id = "transition-refusal-departed-wait";
    let departed = registry
        .register_process(executed_registration(departed_id))
        .await
        .expect("register departed-wait-refusal process");
    let departed_id = departed.id.clone();
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        departed_id.clone(),
        "transition-refusal:execution",
    )
    .bind_attempt(1);
    registry
        .record_first_started_with_authority(
            &departed_id,
            authority
                .invocation_started()
                .expect("a bound invocation names its execution"),
            &authority,
        )
        .await
        .expect("record departed-wait process execution start");
    assert_session_refusal(
        registry.record_caller_departure(&departed_id).await,
        &format!(
            "process `{departed_id}` is not externally-owned and cannot record a caller departure"
        ),
    );
    registry
        .complete_process(
            &departed_id,
            settled_success(serde_json::Value::Null),
            ProcessCompletionAuthority::workflow_key(departed_id.as_str()),
        )
        .await
        .expect("reconcile departed-wait-refusal process");

    let external_ref_id = "transition-refusal-external-ref";
    let transition_refusal_external_ref_record = registry
        .register_process(registration(external_ref_id))
        .await
        .expect("register external-ref-refusal process");
    let external_ref_id = transition_refusal_external_ref_record.id.clone();
    registry
        .set_external_ref(
            &external_ref_id,
            crate::ProcessExternalRef {
                backend: "first-backend".to_string(),
                id: "first-id".to_string(),
                metadata: None,
                segment_ordinal: None,
            },
        )
        .await
        .expect("record first external reference");
    assert_session_refusal(
        registry
            .set_external_ref(
                &external_ref_id,
                crate::ProcessExternalRef {
                    backend: "second-backend".to_string(),
                    id: "second-id".to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await,
        &format!(
            "process `{external_ref_id}` external ref conflict: existing first-backend / first-id, requested second-backend / second-id"
        ),
    );
}
