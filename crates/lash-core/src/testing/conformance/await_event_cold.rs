//! Cold-instance AwaitEvent conformance for durable effect hosts.
//!
//! Every factory call must return an independent host object connected to the
//! same substrate. The suite deliberately never shares an `Arc` between roles.

use super::*;
use crate::runtime::turn_control::ActiveTurnControl;

/// Number of named Layer-A vector groups executed by
/// [`effect_host_await_events_cold_instance`].
pub const COLD_INSTANCE_AWAIT_EVENT_VECTOR_COUNT: usize = 8;

/// Run the durable multi-host AwaitEvent suite.
///
/// Hosts with runtime-owned replay are intentionally ineligible. Store and
/// engine adapters call this only after they can reopen independent host
/// objects over one substrate.
pub async fn effect_host_await_events_cold_instance<F>(make: F)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "effect_host_await_events_cold_instance");
    drop((first, second));
    let prefix = format!("cold-await-{}", uuid::Uuid::new_v4());
    cold_mint_resolve_observe_all_identities(&make, &prefix).await;
    cold_first_writer_wins(&make, &prefix).await;
    cold_replayed_parked_owner(&make, &prefix).await;
    cold_key_stability(&make, &prefix).await;
    cold_auth_tamper_matrix(&make, &prefix).await;
    cold_revocation_survives_reopen(&make, &prefix).await;
    cold_cancel_sweep_excludes_turn_control(&make, &prefix).await;
    cold_terminal_attach_both_orders(&make, &prefix).await;
}

async fn cold_replayed_parked_owner<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let session_id = format!("{prefix}-parked-session");
    let turn_id = format!("{prefix}-parked-turn");
    let scope = durable_turn_scope(&session_id, &turn_id);
    let key = make()
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(format!("{prefix}-parked-call")),
        )
        .await
        .expect("host A mints parked-owner key");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope {
                session_id: session_id.clone(),
                turn_id: Some(turn_id),
                turn_index: None,
                protocol_iteration: None,
            },
            "cold_await_event.parked_owner",
            RuntimeEffectKind::AwaitEvent,
            "cold_await_event.parked_owner",
        ),
        RuntimeEffectCommand::AwaitEvent { key: key.clone() },
    );

    let owner = make()
        .scoped_static(scope.clone())
        .expect("host A creates static controller")
        .expect("durable host exposes a static controller");
    let owner_envelope = envelope.clone();
    let owner_task = crate::task::spawn(async move {
        owner
            .controller()
            .execute_effect(
                owner_envelope,
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                ),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_millis(250), async {
        while !owner_task.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect_err("host A must park while the promise is unresolved");
    owner_task.abort();
    assert!(owner_task.await.is_err(), "parked host A task aborts");

    let terminal = Resolution::Ok(serde_json::json!({ "owner": "cold-replayed" }));
    assert_eq!(
        make()
            .resolve_await_event(&key, terminal.clone())
            .await
            .expect("host B resolves parked owner"),
        ResolveOutcome::Accepted
    );
    for role in ["C", "D"] {
        let controller = make()
            .scoped_static(scope.clone())
            .expect("redrive creates static controller")
            .expect("durable host exposes a static controller");
        let outcome = controller
            .controller()
            .execute_effect(
                envelope.clone(),
                RuntimeEffectLocalExecutor::await_event(
                    tokio_util::sync::CancellationToken::new(),
                    None,
                ),
            )
            .await
            .unwrap_or_else(|error| panic!("host {role} redrives parked owner: {error}"));
        let RuntimeEffectOutcome::AwaitEvent { resolution } = outcome else {
            panic!("host {role} returned the wrong parked-owner outcome");
        };
        assert_eq!(resolution, terminal, "host {role} replays one terminal");
        if role == "C" {
            make()
                .revoke_await_events_for_session(&session_id)
                .await
                .expect("tombstone after the first cold redrive finalized");
        }
    }
}

async fn cold_mint_resolve_observe_all_identities<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let identities = [
        AwaitEventWaitIdentity::tool_completion(format!("{prefix}-tool")),
        AwaitEventWaitIdentity::process_signal(format!("{prefix}-process"), "ready", 1),
        AwaitEventWaitIdentity::TurnCancelGate,
        AwaitEventWaitIdentity::TurnTerminal,
    ];
    for (index, wait) in identities.into_iter().enumerate() {
        let scope = durable_turn_scope(
            format!("{prefix}-identity-session-{index}"),
            format!("{prefix}-identity-turn-{index}"),
        );
        let host_a = make();
        let key = host_a
            .await_event_key(&scope, wait)
            .await
            .expect("host A mints key");
        drop(host_a);

        let terminal = Resolution::Ok(serde_json::json!({ "identity": index }));
        assert_eq!(
            make()
                .resolve_await_event(&key, terminal.clone())
                .await
                .expect("host B resolves"),
            ResolveOutcome::Accepted
        );
        let host_c = make();
        assert_eq!(
            host_c.peek_await_event(&key).await.expect("host C peeks"),
            Some(terminal.clone())
        );
        assert_eq!(
            host_c
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None,)
                .await
                .expect("host C observes"),
            terminal.clone()
        );
        assert_eq!(
            host_c
                .resolve_await_event(&key, Resolution::Cancelled)
                .await
                .expect("host C duplicate resolve"),
            ResolveOutcome::AlreadyResolved { terminal }
        );
    }
}

async fn cold_first_writer_wins<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let address = durable_turn_address(
        format!("{prefix}-race-session"),
        format!("{prefix}-race-turn"),
    );
    let owner_host = make();
    let active = ActiveTurnControl::new(owner_host.as_ref(), address.clone())
        .await
        .expect("owner creates cancellation gate");
    let cancel_driver = crate::TurnWorkDriver::new(make());
    let (settled, cancelled) = tokio::join!(
        active.settle_before_commit(owner_host.as_ref(), false, None),
        cancel_driver.request_cancel(
            crate::TurnCancelRequest::new(address, format!("{prefix}-race-request"), None)
                .with_reason("cold-instance conformance race"),
        ),
    );
    match (
        settled.expect("owner settle contender"),
        cancelled.expect("cancellation contender").outcome,
    ) {
        (None, crate::TurnCancelOutcome::CompletionWonRace)
        | (Some(_), crate::TurnCancelOutcome::Requested(_)) => {}
        other => panic!("cross-instance settle/cancel race disagreed: {other:?}"),
    }
}

async fn cold_key_stability<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let scope = durable_turn_scope(
        format!("{prefix}-stable-session"),
        format!("{prefix}-stable-turn"),
    );
    let wait = AwaitEventWaitIdentity::tool_completion(format!("{prefix}-stable-call"));
    let first = make()
        .await_event_key(&scope, wait.clone())
        .await
        .expect("first key");
    let reopened = make()
        .await_event_key(&scope, wait)
        .await
        .expect("reopened key");
    assert_eq!(first, reopened, "key bytes must survive host reopen");
}

async fn cold_auth_tamper_matrix<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let scope = durable_turn_scope(
        format!("{prefix}-auth-session"),
        format!("{prefix}-auth-turn"),
    );
    let key = make()
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(format!("{prefix}-auth-call")),
        )
        .await
        .expect("auth key");
    let mut variants = Vec::new();
    let mut signature = key.clone();
    signature.signature.push('0');
    variants.push(signature);
    let mut key_id = key.clone();
    key_id.key_id.push('0');
    variants.push(key_id);
    let mut scope = key.clone();
    scope.scope = durable_turn_scope(
        format!("{prefix}-auth-session"),
        format!("{prefix}-auth-other-turn"),
    );
    variants.push(scope);
    let mut wait = key.clone();
    wait.wait = AwaitEventWaitIdentity::tool_completion(format!("{prefix}-other-call"));
    variants.push(wait);
    let mut session = key.clone();
    session.scope = durable_turn_scope(
        format!("{prefix}-auth-session-tampered"),
        format!("{prefix}-auth-turn"),
    );
    variants.push(session);

    for tampered in variants {
        let host = make();
        assert_eq!(
            host.resolve_await_event(&tampered, Resolution::Cancelled)
                .await
                .expect("tampered resolve has canonical outcome"),
            ResolveOutcome::UnknownOrRevoked
        );
        let peek_error = host
            .peek_await_event(&tampered)
            .await
            .expect_err("tampered peek must fail");
        assert_eq!(peek_error.code.as_str(), "await_event_unknown_or_revoked");
        let await_error = host
            .await_await_event(&tampered, tokio_util::sync::CancellationToken::new(), None)
            .await
            .expect_err("tampered await must fail");
        assert_eq!(await_error.code.as_str(), "await_event_unknown_or_revoked");
        assert_eq!(
            make()
                .peek_await_event(&key)
                .await
                .expect("valid key remains untouched"),
            None,
            "failed authentication must not materialize a promise terminal"
        );
    }
}

async fn cold_revocation_survives_reopen<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let session_id = format!("{prefix}-revoked-session");
    let scope = durable_turn_scope(&session_id, format!("{prefix}-revoked-turn"));
    let key = make()
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(format!("{prefix}-revoked-call")),
        )
        .await
        .expect("pre-revocation key");
    make()
        .revoke_await_events_for_session(&session_id)
        .await
        .expect("host A revokes");
    assert_eq!(
        make()
            .resolve_await_event(&key, Resolution::Cancelled)
            .await
            .expect("host B sees revocation"),
        ResolveOutcome::UnknownOrRevoked
    );
    let error = make()
        .peek_await_event(&key)
        .await
        .expect_err("host C sees durable tombstone");
    assert_eq!(error.code.as_str(), "await_event_unknown_or_revoked");
    let mint_error = make()
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(format!("{prefix}-post-revoke-call")),
        )
        .await
        .expect_err("reopened host must reject mint after revocation");
    assert_eq!(mint_error.code.as_str(), "await_event_unknown_or_revoked");
}

async fn cold_cancel_sweep_excludes_turn_control<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let session_id = format!("{prefix}-sweep-session");
    let scope = durable_turn_scope(&session_id, format!("{prefix}-sweep-turn"));
    let ordinary = make()
        .await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(format!("{prefix}-sweep-call")),
        )
        .await
        .expect("ordinary key");
    let waiter_host = make();
    let waiter_key = ordinary.clone();
    let waiter = crate::task::spawn(async move {
        waiter_host
            .await_await_event(
                &waiter_key,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
    });
    let sweeper = make();
    let cancelled = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            sweeper
                .cancel_await_events_for_session(&session_id)
                .await
                .expect("cancel sweep");
            if waiter.is_finished() {
                return waiter.await;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("ordinary waiter cancelled")
    .expect("ordinary waiter joins")
    .expect("ordinary waiter resolves");
    assert_eq!(cancelled, Resolution::Cancelled);

    let gate = make()
        .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
        .await
        .expect("turn gate key");
    make()
        .cancel_await_events_for_session(&session_id)
        .await
        .expect("second sweep");
    assert_eq!(
        make()
            .peek_await_event(&gate)
            .await
            .expect("turn gate survives sweep"),
        None
    );
    assert_eq!(
        make()
            .resolve_await_event(&gate, Resolution::Ok(serde_json::json!("still-open")))
            .await
            .expect("turn gate remains writable"),
        ResolveOutcome::Accepted
    );
}

async fn cold_terminal_attach_both_orders<F>(make: &F, prefix: &str)
where
    F: Fn() -> Arc<dyn EffectHost>,
{
    let after = durable_turn_address(
        format!("{prefix}-attach-after-session"),
        format!("{prefix}-attach-after-turn"),
    );
    let after_terminal = crate::TurnTerminal::Committed {
        outcome: crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage {
            text: "attach-after".to_string(),
        }),
        session_revision: Some(1),
    };
    let after_key = make()
        .await_event_key(
            &ExecutionScope::turn(&after.session_id, &after.turn_id),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
        .expect("attach-after key");
    make()
        .resolve_await_event(
            &after_key,
            Resolution::Ok(serde_json::to_value(&after_terminal).expect("terminal json")),
        )
        .await
        .expect("publish before attach");
    let attached_after = crate::TurnWorkDriver::new(make())
        .await_terminal(&after)
        .await
        .expect("attach after publish");
    assert_eq!(
        serde_json::to_value(attached_after).expect("attached terminal json"),
        serde_json::to_value(after_terminal).expect("expected terminal json")
    );

    let before = durable_turn_address(
        format!("{prefix}-attach-before-session"),
        format!("{prefix}-attach-before-turn"),
    );
    let before_terminal = crate::TurnTerminal::Committed {
        outcome: crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage {
            text: "attach-before".to_string(),
        }),
        session_revision: Some(2),
    };
    let attach_address = before.clone();
    let attach_host = make();
    let attacher = crate::task::spawn(async move {
        crate::TurnWorkDriver::new(attach_host)
            .await_terminal(&attach_address)
            .await
    });
    let before_key = make()
        .await_event_key(
            &ExecutionScope::turn(&before.session_id, &before.turn_id),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
        .expect("attach-before key");
    make()
        .resolve_await_event(
            &before_key,
            Resolution::Ok(serde_json::to_value(&before_terminal).expect("terminal json")),
        )
        .await
        .expect("publish after attach");
    let attached_before = tokio::time::timeout(std::time::Duration::from_secs(10), attacher)
        .await
        .expect("attach-before wakes")
        .expect("attacher joins")
        .expect("attacher resolves");
    assert_eq!(
        serde_json::to_value(attached_before).expect("attached terminal json"),
        serde_json::to_value(before_terminal).expect("expected terminal json")
    );
}
