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
            RecoveryContract::Rerunnable,
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

/// FIG-3388: both release paths decide through the same verdict, so a
/// superseded lease can neither release its successor's claim nor complete the
/// process, and a legitimate release leaves a row holding only the retained
/// fencing token.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn superseded_process_lease_cannot_release_or_complete(
    registry: Arc<dyn ProcessRegistry>,
) {
    const SHORT_TTL_MS: u64 = 20;

    async fn assert_lease_is(
        registry: &dyn ProcessRegistry,
        process_id: &ProcessId,
        expected: &crate::ProcessLease,
    ) {
        let stored = registry
            .get_process_lease(process_id)
            .await
            .expect("read current lease")
            .expect("current lease remains held");
        assert_eq!(stored.lease_token, expected.lease_token);
        assert_eq!(stored.fencing_token, expected.fencing_token);
        assert_eq!(stored.owner.owner_id, expected.owner.owner_id);
        assert_eq!(stored.owner.incarnation_id, expected.owner.incarnation_id);
        assert_eq!(stored.expires_at_epoch_ms, expected.expires_at_epoch_ms);
    }
    let process_id = registry
        .register_process(registration("lease-takeover-release"))
        .await
        .expect("register takeover process")
        .id;

    // A claims P under L1, stalls past TTL, and B takes over under L2.
    let owner_a = process_lease_owner("owner-a");
    let stale = registry
        .claim_process_lease(&process_id, &owner_a, SHORT_TTL_MS)
        .await
        .expect("claim superseded lease")
        .acquired()
        .expect("superseded lease acquired");
    let current = claim_after_expiry(
        registry.as_ref(),
        &process_id,
        &process_lease_owner("owner-b"),
    )
    .await;
    assert!(
        current.fencing_token > stale.fencing_token,
        "takeover must advance the retained fencing generation"
    );
    assert_ne!(current.lease_token, stale.lease_token);

    // The lease token's durable preimage is
    // `blake3("{process_id}:{owner_id}:{incarnation_id}:{claimed_at}:{fencing_token}")`
    // under the `lash-process-lease/v2` domain. Recomputing it here pins the
    // generation's membership in the preimage — a mint that drops the fencing
    // token breaks the release backstop's redundancy and must fail this law.
    let expected_token = lash_sansio::core_support::blake3_domain_hash_hex(
        "lash-process-lease/v2",
        format!(
            "{process_id}:{}:{}:{}:{}",
            current.owner.owner_id,
            current.owner.incarnation_id,
            current.claimed_at_epoch_ms,
            current.fencing_token,
        ),
    );
    assert_eq!(
        current.lease_token, expected_token,
        "the minted lease token must commit to the fencing generation"
    );

    // A presents L1 to `complete_process_lease`: release is idempotent, so the
    // stale presentation is ignored and the successor's row is unchanged.
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&stale))
        .await
        .expect("stale release is idempotently ignored");
    assert_lease_is(registry.as_ref(), &process_id, &current).await;

    // A presents L1 to `complete_process_with_lease`: the site's refusal, zero
    // rows written, and the successor's row still unchanged.
    let error = registry
        .complete_process_with_lease(
            &stale,
            settled_success(serde_json::json!({"writer": "stale"})),
        )
        .await
        .expect_err("a superseded lease must not complete the process");
    assert!(
        matches!(error, crate::PluginError::ProcessLeaseSuperseded { .. }),
        "stale completion must fail with the site's refusal, got {error:?}"
    );
    assert!(
        !registry
            .get_process(&process_id)
            .await
            .expect("read takeover process")
            .expect("takeover process exists")
            .is_terminal(),
        "a refused completion must not terminate the process"
    );
    assert_lease_is(registry.as_ref(), &process_id, &current).await;

    // The legitimate release through `complete_process_lease` leaves a row
    // holding only the retained fencing token: no holder projects, and the
    // next claim builds on the retained generation.
    registry
        .complete_process_lease(&crate::ProcessLeaseCompletion::from_lease(&current))
        .await
        .expect("release the successor lease");
    assert!(
        registry
            .get_process_lease(&process_id)
            .await
            .expect("read released lease")
            .is_none(),
        "a released row must project no holder"
    );
    let after_release = registry
        .claim_process_lease(&process_id, &process_lease_owner("owner-c"), 60_000)
        .await
        .expect("claim after release")
        .acquired()
        .expect("post-release claim acquired");
    assert!(
        after_release.fencing_token > current.fencing_token,
        "the released row's retained fencing token must fence the next holder"
    );

    // The same released-row shape through `complete_process_with_lease`.
    let outcome = registry
        .complete_process_with_lease(
            &after_release,
            settled_success(serde_json::json!({"writer": "owner-c"})),
        )
        .await
        .expect("legitimate leased completion");
    assert!(matches!(
        outcome,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    assert!(
        registry
            .get_process_lease(&process_id)
            .await
            .expect("read lease after leased completion")
            .is_none(),
        "leased completion must leave the same released row"
    );
}
