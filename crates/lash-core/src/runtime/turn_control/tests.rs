use super::*;
use crate::runtime::InMemorySessionStore;
use crate::{NativeEffectHost, TurnFinish, TurnInputStore, TurnStop};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CatalogProbeFactory {
    store: Arc<InMemorySessionStore>,
    opens: AtomicUsize,
    fail_open: bool,
}

impl CatalogProbeFactory {
    fn new(fail_open: bool) -> Self {
        Self {
            store: Arc::new(InMemorySessionStore::default()),
            opens: AtomicUsize::new(0),
            fail_open,
        }
    }
}

#[async_trait::async_trait]
impl crate::AttachmentRootSet for CatalogProbeFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<crate::AttachmentId>, crate::StoreError> {
        Ok(BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &crate::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, crate::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl crate::SessionStoreFactory for CatalogProbeFactory {
    async fn create_store(
        &self,
        _request: &crate::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn crate::RuntimePersistence>, crate::StoreError> {
        Ok(self.store.clone())
    }

    async fn open_existing_store_by_id(
        &self,
        _session_id: &crate::SessionId,
    ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, String> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        if self.fail_open {
            return Err("catalog unavailable".to_string());
        }
        Ok(Some(self.store.clone()))
    }

    async fn session_was_deleted(&self, _session_id: &crate::SessionId) -> Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &crate::SessionId,
    ) -> crate::store::MaintenanceResult<crate::SessionBlobReclaimReport> {
        Ok(crate::SessionBlobReclaimReport::default())
    }
}

fn address(label: &str) -> TurnAddress {
    TurnAddress::new(
        format!("turn-control-{label}-{}", uuid::Uuid::new_v4()),
        "turn-a",
    )
}

fn request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
    TurnCancelRequest::new(address, request_id, Some("user".to_string())).with_reason("stop button")
}

fn bound_driver(host: Arc<NativeEffectHost>, address: &TurnAddress) -> TurnWorkDriver {
    TurnWorkDriver::for_session(
        host,
        address.session_id.clone(),
        Arc::new(InMemorySessionStore::default()),
    )
}

fn scoped_turn_controller<'a>(
    host: &'a NativeEffectHost,
    address: &TurnAddress,
) -> ScopedEffectController<'a> {
    host.scoped(address.execution_scope())
        .expect("scope turn controller")
}

#[test]
fn foreground_turn_cancel_peek_replay_keys_remain_unchanged() {
    let address = TurnAddress::new("foreground-session", "foreground-turn");
    let scope = address.execution_scope();
    let identities = [
        (TurnCancelPeekIdentity::StartGate, "turn_cancel.start_gate"),
        (
            TurnCancelPeekIdentity::PostAbortGate,
            "turn_cancel.post_abort_gate",
        ),
        (
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 7,
            },
            "turn_cancel.after_llm.7",
        ),
        (
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 7,
            },
            "turn_cancel.after_step.7",
        ),
    ];

    for (identity, expected) in identities {
        let causal_identity = identity.causal_identity();
        assert_eq!(causal_identity, expected);
        assert_eq!(
            turn_cancel_peek_replay_key(&scope, &address, &causal_identity),
            expected
        );
        let escalation = identity.escalation_causal_identity();
        assert_eq!(
            turn_cancel_peek_replay_key(&scope, &address, &escalation),
            escalation
        );
    }
}

#[test]
fn process_turn_cancel_peek_replay_keys_cover_physical_turn_and_gate() {
    let scope = ExecutionScope::process("process:subagent:peek-key");
    let root = TurnAddress::new("session:subagent:peek-key", "process:subagent:peek-key");
    let follow_on = TurnAddress::new(&root.session_id, "process:subagent:peek-key:agent-frame:1");
    assert_eq!(
        turn_cancel_peek_replay_key(
            &scope,
            &root,
            &TurnCancelPeekIdentity::StartGate.causal_identity(),
        ),
        "turn-cancel-peek:v1:blake3:da49b5246b68613ad7de048d74eca8f0655c4fc2b62db4872af62491b5f7656e",
    );
    let mut keys = BTreeSet::new();

    for address in [&root, &follow_on] {
        for identity in [
            TurnCancelPeekIdentity::StartGate,
            TurnCancelPeekIdentity::PostAbortGate,
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 7,
            },
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 7,
            },
        ] {
            for causal_identity in [
                identity.causal_identity(),
                identity.escalation_causal_identity(),
            ] {
                let key = turn_cancel_peek_replay_key(&scope, address, &causal_identity);
                assert!(
                    key.starts_with("turn-cancel-peek:v1:blake3:"),
                    "unexpected Process peek key: {key}"
                );
                assert!(keys.insert(key), "Process peek identity collided");
            }
        }
    }

    assert_eq!(keys.len(), 16);
}

#[test]
fn every_shared_scope_cancel_peek_key_covers_physical_turn_and_gate() {
    let cases = [
        (
            ExecutionScope::turn("turn-session", "turn-root"),
            TurnAddress::new("turn-session", "turn-root"),
            TurnAddress::new("turn-session", "turn-root:agent-frame:1"),
        ),
        (
            ExecutionScope::queue_drain("queue-session", "queue-drain"),
            TurnAddress::new("queue-session", "queue-root"),
            TurnAddress::new("queue-session", "queue-follow-on"),
        ),
        (
            ExecutionScope::runtime_operation("runtime-operation"),
            TurnAddress::new("runtime-session", "runtime-root"),
            TurnAddress::new("runtime-session", "runtime-follow-on"),
        ),
    ];

    for (scope, root, follow_on) in cases {
        let mut keys = BTreeSet::new();
        for address in [&root, &follow_on] {
            for identity in [
                TurnCancelPeekIdentity::StartGate,
                TurnCancelPeekIdentity::PostAbortGate,
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 7,
                },
                TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: 7,
                },
            ] {
                for causal_identity in [
                    identity.causal_identity(),
                    identity.escalation_causal_identity(),
                ] {
                    let key = turn_cancel_peek_replay_key(&scope, address, &causal_identity);
                    assert!(keys.insert(key), "shared-scope peek identity collided");
                }
            }
        }
        assert_eq!(keys.len(), 16);
        assert_eq!(
            turn_cancel_peek_replay_key(
                &scope,
                &root,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            turn_cancel_peek_replay_key(
                &scope,
                &root,
                &TurnCancelPeekIdentity::StartGate.causal_identity(),
            ),
            "same-frame replay must reconstruct the same key"
        );
    }
}

#[tokio::test]
async fn process_scoped_turn_control_peek_uses_admitted_effect_scope() {
    let host = NativeEffectHost::default();
    let address = TurnAddress::new(
        "session:subagent:scope-probe",
        "process:subagent:scope-probe",
    );
    let active = ActiveTurnControl::new(&host, address.clone())
        .await
        .expect("active process-backed turn control");
    let process_scope = ExecutionScope::process("process:subagent:scope-probe");
    let scoped = host
        .scoped(process_scope.clone())
        .expect("scope process controller");

    assert_eq!(
        active
            .observe_pending_cancel(&scoped, TurnCancelPeekIdentity::StartGate)
            .await
            .expect("peek process-backed turn cancellation"),
        None
    );
    assert_eq!(scoped.execution_scope(), &process_scope);
    assert_eq!(
        address.execution_scope(),
        ExecutionScope::turn(&address.session_id, &address.turn_id)
    );
}

struct TurnAttachProbe {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl TurnAttach for TurnAttachProbe {
    async fn await_terminal(&self, _address: &TurnAddress) -> Result<TurnTerminal, RuntimeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("a foreign exact-session address must be refused before terminal attachment")
    }
}

#[tokio::test]
async fn exact_driver_rejects_foreign_terminal_attach_before_touching_the_host() {
    let host = Arc::new(NativeEffectHost::default());
    let foreign = address("foreign-terminal-attach");
    let active = ActiveTurnControl::new(host.as_ref(), foreign.clone())
        .await
        .expect("active foreign turn control");
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "finished".to_string(),
        }),
        session_revision: Some(9),
    };
    let store = Arc::new(InMemorySessionStore::default());
    let probe = Arc::new(TurnAttachProbe {
        calls: AtomicUsize::new(0),
    });
    let wrong_driver =
        TurnWorkDriver::for_session(host.clone(), "different-bound-session", store.clone())
            .with_test_attach(probe.clone());

    for result in [
        wrong_driver.await_terminal(&foreign).await,
        wrong_driver
            .await_terminal_with_timeout(&foreign, Duration::from_millis(1))
            .await,
    ] {
        let error = result.expect_err("an exact driver cannot attach to another session");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::InvalidTurnCancelRequest
        );
    }
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert!(
        store
            .turn_cancel_request(&foreign)
            .await
            .expect("read cancellation store")
            .is_none(),
        "terminal attachment validation must not write cancellation storage"
    );

    active
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish foreign terminal after both refusals");
    let attached = TurnWorkDriver::for_session(
        host,
        foreign.session_id.clone(),
        Arc::new(InMemorySessionStore::default()),
    )
    .await_terminal(&foreign)
    .await
    .expect("the rejected attaches left the host terminal untouched");
    assert!(matches!(
        attached,
        TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage { ref text }),
            session_revision: Some(9),
        } if text == "finished"
    ));
}

#[tokio::test]
async fn exact_driver_rejects_another_session_before_store_or_gate_effects() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("wrong-exact-session");
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let store = Arc::new(InMemorySessionStore::default());
    let wrong_driver =
        TurnWorkDriver::for_session(host.clone(), "different-bound-session", store.clone());

    let error = wrong_driver
        .request_cancel(request(address.clone(), "wrong-binding"))
        .await
        .expect_err("an exact driver cannot address another session");
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::InvalidTurnCancelRequest
    );
    assert!(
        store
            .turn_cancel_request(&address)
            .await
            .expect("read cancellation row")
            .is_none()
    );

    let receipt = TurnWorkDriver::for_session(host, address.session_id.clone(), store)
        .request_cancel(request(address, "correct-binding"))
        .await
        .expect("the rejected request left the gate available");
    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    drop(active);
}

#[test]
fn legacy_cancel_request_without_disposition_defaults_to_defer() {
    let decoded: TurnCancelRequest = serde_json::from_value(serde_json::json!({
        "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
        "request_id": "legacy-request"
    }))
    .expect("decode a pre-disposition cancel request");
    assert_eq!(decoded.undelivered, TurnCancelDisposition::Defer);
    assert!(
        serde_json::to_value(decoded)
            .expect("encode defaulted request")
            .get("undelivered")
            .is_none(),
        "the legacy Defer default stays sparse on the durable row"
    );
}

#[tokio::test]
async fn cancel_before_start_duplicate_and_terminal_attach() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("before-start");
    let driver = bound_driver(host.clone(), &address);

    let first = driver
        .request_cancel(request(address.clone(), "request-1"))
        .await
        .expect("request cancellation");
    let evidence = match first.outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };
    assert_eq!(evidence.request_id, "request-1");

    let duplicate = driver
        .request_cancel(request(address.clone(), "request-2"))
        .await
        .expect("duplicate cancellation");
    assert!(matches!(
        duplicate.outcome,
        TurnCancelOutcome::AlreadyRequested(TurnCancellationEvidence { ref request_id, .. })
            if request_id == "request-1"
    ));

    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let observed = active
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle")
        .expect("cancellation won");
    assert_eq!(observed, evidence);
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Stopped(TurnStop::Cancelled { evidence: observed }),
        session_revision: Some(7),
    };
    active
        .publish_terminal(host.as_ref(), &terminal)
        .await
        .expect("publish terminal");
    let attached = driver
        .await_terminal(&address)
        .await
        .expect("attach terminal");
    assert!(matches!(
        attached,
        TurnTerminal::Committed {
            outcome: TurnOutcome::Stopped(TurnStop::Cancelled { .. }),
            session_revision: Some(7),
        }
    ));
}

#[tokio::test]
async fn settle_seals_the_assembled_evidence_instead_of_minting_a_second_id() {
    // A provider abort classified as cancelled arrives with evidence the
    // sans-IO machine already put on the streamed outcome. Sealing that
    // value is what keeps one cancellation to one request id: minting
    // `internal:{turn_id}` here would hand the host a second identity for
    // the same fact.
    let host = Arc::new(NativeEffectHost::default());
    let address = address("assembled");
    let driver = bound_driver(host.clone(), &address);
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let assembled = TurnCancellationEvidence::internal("provider-cancelled:3");

    let settled = active
        .settle_before_commit(host.as_ref(), true, Some(assembled.clone()))
        .await
        .expect("settle")
        .expect("a locally cancelled turn settles cancelled");
    assert_eq!(settled, assembled);
    assert_ne!(settled, active.internal_evidence());

    // The durable gate carries the same identity, so a later requester and
    // a replayed owner both read the value the turn streamed.
    let late = driver
        .request_cancel(request(address, "late-request"))
        .await
        .expect("late cancellation");
    assert!(matches!(
        late.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if *evidence == assembled
    ));
}

#[tokio::test]
async fn concurrent_completion_seal_vs_cancel_is_first_writer_wins() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("race");
    let driver = bound_driver(host.clone(), &address);
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");

    let (seal, cancel) = tokio::join!(
        active.settle_before_commit(host.as_ref(), false, None),
        driver.request_cancel(request(address, "race-request")),
    );
    match (seal.expect("seal"), cancel.expect("cancel").outcome) {
        (None, TurnCancelOutcome::CompletionWonRace) => {}
        (Some(evidence), TurnCancelOutcome::Requested(requested)) => {
            assert_eq!(evidence, requested);
        }
        other => panic!("inconsistent gate race result: {other:?}"),
    }
}

#[tokio::test]
async fn recovered_owner_observes_pending_cancel_after_control_recreation() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("replay");
    let driver = bound_driver(host.clone(), &address);
    let requested = driver
        .request_cancel(request(address.clone(), "request-before-replay"))
        .await
        .expect("request cancellation");
    let expected = match requested.outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };

    let scoped = host
        .scoped(address.execution_scope())
        .expect("scope recovered turn controller");
    let recovered = ActiveTurnControl::new(host.as_ref(), address)
        .await
        .expect("recreate active control under the recovered owner");
    let observed = recovered
        .observe_pending_cancel(&scoped, TurnCancelPeekIdentity::StartGate)
        .await
        .expect("read recovered turn start gate")
        .expect("pending cancellation is visible before recovered effects");
    assert_eq!(observed, expected);
    let settled = recovered
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle recovered turn")
        .expect("pending cancellation survives owner loss");
    assert_eq!(settled, expected);
}

#[tokio::test]
async fn turn_control_is_exact_scope_and_excluded_from_wait_cancel_sweep() {
    let host = Arc::new(NativeEffectHost::default());
    let address_a = address("scope");
    let driver = bound_driver(host.clone(), &address_a);
    let address_b = TurnAddress::new(&address_a.session_id, "turn-b");
    let address_future = TurnAddress::new(&address_a.session_id, "turn-future");

    driver
        .request_cancel(request(address_a.clone(), "request-a"))
        .await
        .expect("cancel a");

    let tool_key = host
        .await_event_key(
            &ExecutionScope::turn(&address_a.session_id, "tool-turn"),
            AwaitEventWaitIdentity::tool_completion("tool-call"),
        )
        .await
        .expect("tool key");
    let tool_host = host.clone();
    let tool_wait = crate::task::spawn(async move {
        tool_host
            .await_await_event(&tool_key, CancellationToken::new(), None)
            .await
    });
    tokio::task::yield_now().await;
    host.cancel_await_events_for_session(&address_a.session_id)
        .await
        .expect("cancel durable waits");
    assert!(matches!(
        tool_wait
            .await
            .expect("tool wait task")
            .expect("tool resolution"),
        Resolution::Cancelled
    ));

    assert!(matches!(
        driver
            .request_cancel(request(address_a.clone(), "request-a-duplicate"))
            .await
            .expect("duplicate a")
            .outcome,
        TurnCancelOutcome::AlreadyRequested(_)
    ));
    assert!(matches!(
        driver
            .request_cancel(request(address_b, "request-b"))
            .await
            .expect("cancel b")
            .outcome,
        TurnCancelOutcome::Requested(_)
    ));
    assert!(matches!(
        driver
            .request_cancel(request(address_future, "request-future"))
            .await
            .expect("cancel future")
            .outcome,
        TurnCancelOutcome::Requested(_)
    ));
}

#[tokio::test]
async fn session_deletion_revokes_control_promises() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("revoke");
    let store = Arc::new(InMemorySessionStore::default());
    let driver =
        TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
    host.revoke_await_events_for_session(&address.session_id)
        .await
        .expect("revoke session");
    let request_address = address.clone();
    assert!(matches!(
        driver
            .request_cancel(request(address, "request-after-delete"))
            .await
            .expect("revoked outcome")
            .outcome,
        TurnCancelOutcome::UnknownOrRevoked
    ));
    assert!(
        store
            .turn_cancel_request(&request_address)
            .await
            .expect("read cancellation row")
            .is_none(),
        "a revoked target must not acquire a cancellation row"
    );
}

#[tokio::test]
async fn revoked_target_does_not_resolve_a_failing_catalog() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("revoked-failing-catalog");
    host.revoke_await_events_for_session(&address.session_id)
        .await
        .expect("revoke session");
    let catalog = Arc::new(CatalogProbeFactory::new(true));
    let driver = TurnWorkDriver::for_catalog(host, catalog.clone());

    let receipt = driver
        .request_cancel(request(address, "request-after-delete"))
        .await
        .expect("revoked outcome precedes catalog resolution");

    assert!(matches!(
        receipt.outcome,
        TurnCancelOutcome::UnknownOrRevoked
    ));
    assert!(receipt.record.is_none());
    assert_eq!(catalog.opens.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn catalog_driver_resolves_the_target_store_once_per_request() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("catalog-resolve-once");
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let catalog = Arc::new(CatalogProbeFactory::new(false));
    let driver = TurnWorkDriver::for_catalog(host.clone(), catalog.clone());

    let receipt = driver
        .request_cancel(request(address.clone(), "request-resolve-once"))
        .await
        .expect("request cancellation");

    assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
    assert!(receipt.record.is_some());
    assert_eq!(catalog.opens.load(Ordering::SeqCst), 1);
    assert!(
        catalog
            .store
            .turn_cancel_request(&address)
            .await
            .expect("read canonical cancellation row")
            .is_some()
    );
    drop(active);
}

#[tokio::test]
async fn terminal_attachment_timeout_does_not_poison_later_publication() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("terminal-timeout");
    let driver = bound_driver(host.clone(), &address);
    let error = driver
        .await_terminal_with_timeout(&address, Duration::from_millis(1))
        .await
        .expect_err("unpublished terminal must time out");
    assert_eq!(error.code.as_str(), "turn_terminal_await_timeout");

    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control after timed-out attach");
    active
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("seal after timed-out attach");
    active
        .publish_terminal(
            host.as_ref(),
            &TurnTerminal::Committed {
                outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
                    text: "done".to_string(),
                }),
                session_revision: None,
            },
        )
        .await
        .expect("publish after timed-out attach");
    assert!(matches!(
        driver.await_terminal(&address).await.expect("late attach"),
        TurnTerminal::Committed {
            outcome: TurnOutcome::Finished(_),
            ..
        }
    ));
}

#[test]
fn local_cancel_origin_hint_preserves_first_origin() {
    let hint = TurnCancelOriginHint::default();
    assert!(!hint.was_set());
    hint.set(Some("shutdown".to_string()));
    hint.set(Some("user".to_string()));

    assert!(hint.was_set());
    assert_eq!(hint.get().as_deref(), Some("shutdown"));
}

#[test]
fn local_cancel_origin_hint_preserves_explicit_absence() {
    let hint = TurnCancelOriginHint::default();
    assert!(!hint.was_set());
    hint.set(None);
    hint.set(Some("user".to_string()));

    assert!(hint.was_set());
    assert_eq!(hint.get(), None);
}

#[test]
fn installed_originless_token_does_not_block_a_later_registry_origin() {
    let hint = TurnCancelOriginHint::default();
    hint.configure_local_token(None);

    assert!(!hint.was_set());

    hint.set(Some("user".to_string()));
    assert_eq!(hint.get().as_deref(), Some("user"));
}

#[test]
fn observed_registry_origin_wins_over_configured_token_origin() {
    let hint = TurnCancelOriginHint::default();
    hint.configure_local_token(Some("shutdown".to_string()));
    assert_eq!(hint.get().as_deref(), Some("shutdown"));

    hint.set(Some("user".to_string()));
    assert_eq!(hint.get().as_deref(), Some("user"));
}

#[test]
fn terminal_success_has_no_cancellation_evidence() {
    let terminal = TurnTerminal::Committed {
        outcome: TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: "done".to_string(),
        }),
        session_revision: None,
    };
    let encoded = terminal_resolution(&terminal).expect("encode terminal");
    assert!(matches!(encoded, Resolution::Ok(_)));
}

#[test]
fn legacy_cancel_request_without_mode_decodes_as_immediate() {
    let decoded: TurnCancelRequest = serde_json::from_value(serde_json::json!({
        "address": { "session_id": "legacy-session", "turn_id": "legacy-turn" },
        "request_id": "legacy-request",
        "origin": "user",
        "reason": "stop button",
        "undelivered": "drop"
    }))
    .expect("decode a pre-mode cancel request");
    assert_eq!(decoded.mode, TurnCancelMode::Immediate);
    assert_eq!(decoded.undelivered, TurnCancelDisposition::Drop);
    let encoded = serde_json::to_value(&decoded).expect("encode defaulted request");
    assert!(
        encoded.get("mode").is_none(),
        "the Immediate default stays sparse on the durable row: {encoded}"
    );
}

#[test]
fn legacy_cancellation_evidence_without_mode_decodes_as_immediate() {
    let decoded: TurnCancellationEvidence = serde_json::from_value(serde_json::json!({
        "request_id": "legacy-request",
        "origin": "user"
    }))
    .expect("decode pre-mode evidence");
    assert_eq!(decoded.mode, TurnCancelMode::Immediate);
    assert_eq!(decoded.honoured_after_step, None);
    let encoded = serde_json::to_value(&decoded).expect("encode evidence");
    assert!(encoded.get("mode").is_none());
    assert!(encoded.get("honoured_after_step").is_none());
}

#[test]
fn after_step_request_and_evidence_round_trip_the_mode() {
    let request = request(address("mode"), "request-1").mode(TurnCancelMode::AfterStep);
    let encoded = serde_json::to_value(&request).expect("encode request");
    assert_eq!(encoded["mode"], serde_json::json!("after_step"));
    let decoded: TurnCancelRequest =
        serde_json::from_value(encoded).expect("decode after-step request");
    assert_eq!(decoded, request);
    let evidence = TurnCancellationEvidence {
        honoured_after_step: Some(3),
        ..decoded.evidence()
    };
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    let encoded = serde_json::to_value(&evidence).expect("encode evidence");
    assert_eq!(encoded["mode"], serde_json::json!("after_step"));
    assert_eq!(encoded["honoured_after_step"], serde_json::json!(3));
    let decoded: TurnCancellationEvidence =
        serde_json::from_value(encoded).expect("decode evidence");
    assert_eq!(decoded, evidence);
}

#[test]
fn cancel_mode_ordering_only_lets_immediate_escalate_after_step() {
    assert!(TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::AfterStep));
    assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::Immediate));
    assert!(!TurnCancelMode::Immediate.is_stronger_than(TurnCancelMode::Immediate));
    assert!(!TurnCancelMode::AfterStep.is_stronger_than(TurnCancelMode::AfterStep));
    assert!(TurnCancelMode::default().is_immediate());
}

#[test]
fn peek_identities_are_replay_deterministic_and_name_their_escalation() {
    let after_step = TurnCancelPeekIdentity::AfterStep {
        protocol_iteration: 4,
    };
    assert_eq!(after_step.causal_identity(), "turn_cancel.after_step.4");
    assert_eq!(
        after_step.escalation_causal_identity(),
        "turn_cancel.escalation.after_step.4"
    );
    assert_eq!(after_step.honours_after_step(), Some(Some(4)));
    assert_eq!(
        TurnCancelPeekIdentity::StartGate.honours_after_step(),
        Some(None)
    );
    assert_eq!(
        TurnCancelPeekIdentity::PostAbortGate.honours_after_step(),
        Some(None)
    );
    assert_eq!(
        TurnCancelPeekIdentity::AfterLlm {
            protocol_iteration: 0
        }
        .honours_after_step(),
        None
    );
    assert_eq!(
        TurnCancelPeekIdentity::AfterLlm {
            protocol_iteration: 0
        }
        .escalation_causal_identity(),
        "turn_cancel.escalation.after_llm.0"
    );
}

#[tokio::test]
async fn after_step_request_is_deferred_until_immediate_escalates_it() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("escalate");
    let driver = bound_driver(host.clone(), &address);
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let scoped = scoped_turn_controller(host.as_ref(), &address);

    let stop = driver
        .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
        .await
        .expect("after-step request");
    let stop_evidence = match stop.outcome {
        TurnCancelOutcome::Requested(evidence) => evidence,
        other => panic!("expected requested, got {other:?}"),
    };
    assert_eq!(stop_evidence.mode, TurnCancelMode::AfterStep);

    // Mid-model-call observation (AfterLlm never honours after-step): the
    // request is remembered as deferred, never as effective evidence.
    let observed = active
        .observe_pending_cancel(
            &scoped,
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 0,
            },
        )
        .await
        .expect("peek after llm");
    assert_eq!(observed, None);
    assert_eq!(active.evidence(), None);
    assert_eq!(active.deferred_evidence(), Some(stop_evidence.clone()));

    let again = driver
        .request_cancel(request(address.clone(), "stop-2").mode(TurnCancelMode::AfterStep))
        .await
        .expect("second after-step request");
    assert!(matches!(
        again.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "stop-1"
    ));

    let abort = driver
        .request_cancel(request(address.clone(), "abort-1"))
        .await
        .expect("escalation");
    let abort_evidence = match abort.outcome {
        TurnCancelOutcome::Escalated(evidence) => evidence,
        other => panic!("expected escalated, got {other:?}"),
    };
    assert_eq!(abort_evidence.request_id, "abort-1");
    assert_eq!(abort_evidence.mode, TurnCancelMode::Immediate);

    let repeat = driver
        .request_cancel(request(address.clone(), "abort-2"))
        .await
        .expect("repeated escalation");
    assert!(matches!(
        repeat.outcome,
        TurnCancelOutcome::AlreadyRequested(ref evidence) if evidence.request_id == "abort-1"
    ));

    let observed = active
        .observe_pending_cancel(
            &scoped,
            TurnCancelPeekIdentity::AfterLlm {
                protocol_iteration: 1,
            },
        )
        .await
        .expect("peek after escalation");
    assert_eq!(observed, Some(abort_evidence.clone()));
    assert_eq!(active.evidence(), Some(abort_evidence.clone()));

    let settled = active
        .settle_before_commit(host.as_ref(), true, None)
        .await
        .expect("settle");
    assert_eq!(settled, Some(abort_evidence));
}

#[tokio::test]
async fn after_step_request_is_honoured_at_the_step_boundary_with_its_iteration() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("boundary");
    let driver = bound_driver(host.clone(), &address);
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control");
    let scoped = scoped_turn_controller(host.as_ref(), &address);
    driver
        .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
        .await
        .expect("after-step request");
    assert_eq!(
        active
            .observe_pending_cancel(
                &scoped,
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 2,
                },
            )
            .await
            .expect("peek after llm"),
        None
    );
    let honoured = active
        .observe_pending_cancel(
            &scoped,
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 2,
            },
        )
        .await
        .expect("peek at boundary")
        .expect("after-step lands at the boundary");
    assert_eq!(honoured.request_id, "stop-1");
    assert_eq!(honoured.mode, TurnCancelMode::AfterStep);
    assert_eq!(honoured.honoured_after_step, Some(2));
    assert_eq!(active.evidence(), Some(honoured.clone()));
    let settled = active
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle");
    assert_eq!(settled, Some(honoured));
    // Once the gate holds after-step evidence, a later Immediate request
    // still escalates the record; the owner is what decides whether it
    // is already past its boundary.
    let late = driver
        .request_cancel(request(address.clone(), "abort-late"))
        .await
        .expect("late escalation");
    assert!(matches!(late.outcome, TurnCancelOutcome::Escalated(_)));
}

#[tokio::test]
async fn local_after_step_stop_resolves_the_own_gate_and_lands_at_commit() {
    let host = Arc::new(NativeEffectHost::default());
    let address = address("local-after-step");
    let hint = TurnCancelOriginHint::default();
    let active = ActiveTurnControl::new(host.as_ref(), address.clone())
        .await
        .expect("active control")
        .with_local_cancel_origin(hint.clone());
    let scoped = scoped_turn_controller(host.as_ref(), &address);
    hint.request_after_step(Some("shutdown".to_string()));
    assert!(hint.after_step_requested());
    active
        .resolve_local_after_step(host.as_ref())
        .await
        .expect("resolve own gate");
    let honoured = active
        .observe_pending_cancel(
            &scoped,
            TurnCancelPeekIdentity::AfterStep {
                protocol_iteration: 0,
            },
        )
        .await
        .expect("peek at boundary")
        .expect("local after-step lands at the boundary");
    assert_eq!(honoured.mode, TurnCancelMode::AfterStep);
    assert_eq!(honoured.origin.as_deref(), Some("shutdown"));
    assert_eq!(honoured.honoured_after_step, Some(0));
    assert_eq!(honoured.request_id, format!("internal:{}", address.turn_id));

    // Without a boundary the flag still settles the final commit as an
    // after-step stop.
    let commit_only = ActiveTurnControl::new(host.as_ref(), self::address("local-commit"))
        .await
        .expect("active control")
        .with_local_cancel_origin({
            let hint = TurnCancelOriginHint::default();
            hint.request_after_step(None);
            hint
        });
    let settled = commit_only
        .settle_before_commit(host.as_ref(), false, None)
        .await
        .expect("settle")
        .expect("after-step flag settles as cancelled");
    assert_eq!(settled.mode, TurnCancelMode::AfterStep);
    assert_eq!(settled.honoured_after_step, None);
}
