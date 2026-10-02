use super::*;
use lash_core::{LifetimeDecision, ScopeGrant, ScopeId};
use pretty_assertions::assert_eq;

/// Lifecycle event timestamps belong to the injected registry clock, including
/// wait entry and exit. Advancing it between writes pins each write's source.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn lifecycle_event_timestamps_follow_the_registry_clock(
    registry: Arc<dyn ProcessRegistry>,
) {
    let clock = Arc::new(lash_core::testing::TestClock::new(10_000));
    let registry = registry
        .with_runtime_clock(clock.clone())
        .expect("persistent registries support clock rebinding");
    let process_id = registry
        .register_process(executed_registration("lifecycle-clock"))
        .await
        .expect("register clock process")
        .id;
    let authority = crate::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "lifecycle-clock:execution",
    )
    .bind_attempt(1);
    clock.advance(10);
    registry
        .record_first_started_with_authority(
            &process_id,
            authority.invocation_started().expect("bound invocation"),
            &authority,
        )
        .await
        .expect("record first start");
    clock.advance(10);
    registry
        .set_process_wait_with_authority(
            &process_id,
            crate::WaitState {
                since_ms: 10_020,
                kind: crate::WaitKind::Signal {
                    name: "ready".to_string(),
                    event_type: "signal.ready".to_string(),
                    key: lash_core::runtime::process_signal_wait_key(&process_id, "ready", 1),
                    ordinal: 1,
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("enter wait");
    clock.advance(10);
    registry
        .clear_process_wait_with_authority(&process_id, Vec::new(), &authority)
        .await
        .expect("clear wait");
    clock.advance(10);
    registry
        .park_process_with_authority(
            &process_id,
            crate::store::ParkReason::ReplayDivergence {
                message: "clock provenance fixture".to_string(),
            }
            .into(),
            &authority,
        )
        .await
        .map(lash_core::store::StoreTransition::into_record)
        .expect("park process");
    clock.advance(10);
    registry
        .begin_parked_rerun_with_authority(&process_id, &authority)
        .await
        .expect("begin parked rerun");
    let page = registry
        .event_page(
            &process_id,
            std::num::NonZeroUsize::new(10).expect("nonzero page bound"),
            crate::ProcessEventQueryMode::Full,
        )
        .await
        .expect("read durable lifecycle events");
    let crate::ProcessEventReadOutcome::Retained(crate::ProcessEventPage {
        events: crate::ProcessEventPageEvents::Full(events),
        more: crate::ProcessEventPageMore::Complete,
    }) = page
    else {
        panic!("the complete lifecycle history must be retained");
    };
    assert_eq!(
        events
            .iter()
            .map(|event| (event.event_type.as_str(), event.occurred_at))
            .collect::<Vec<_>>(),
        vec![
            ("process.first_started", 10_010),
            ("process.waiting", 10_020),
            ("process.resumed", 10_030),
            ("process.parked", 10_040),
            ("process.park_rerun_began", 10_050),
        ],
        "every lifecycle event must retain its injected clock instant"
    );
}

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
