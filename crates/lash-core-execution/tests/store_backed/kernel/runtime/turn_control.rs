mod tests {
    use crate::runtime::turn_control::*;
    use crate::support::prelude::*;
    use crate::{
        AdmittedScope, EffectHost, ExecutionScope, ScopedEffectController, TurnFinish, TurnStop,
    };
    use crate::{
        AwaitEventWaitIdentity, Resolution, ResolveOutcome, RuntimeError, TurnOutcome,
        active_turn_internal_evidence, cancel_requested_gate_resolution, turn_cancel_gate_key,
        turn_escalation_key,
    };
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    struct MissingBaseAfterSettleResolver {
        owner: Arc<dyn EffectHost>,
        base_key: crate::AwaitEventKey,
        resolve_calls: AtomicUsize,
        base_peeks: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::AwaitEventResolver for MissingBaseAfterSettleResolver {
        /// A test double that mints keys under no durable authority.
        fn await_event_authority_binding_id(&self) -> Option<String> {
            None
        }

        async fn resolve_await_event(
            &self,
            key: &crate::AwaitEventKey,
            resolution: crate::Resolution,
        ) -> Result<crate::ResolveOutcome, crate::RuntimeError> {
            self.resolve_calls.fetch_add(1, Ordering::SeqCst);
            self.owner.resolve_await_event(key, resolution).await
        }

        async fn peek_await_event(
            &self,
            key: &crate::AwaitEventKey,
        ) -> Result<Option<crate::Resolution>, crate::RuntimeError> {
            if key == &self.base_key {
                // The base, then its escalation: a base lash originated
                // itself stays adoptable until its escalation is closed.
                assert_eq!(
                    self.resolve_calls.load(Ordering::SeqCst),
                    2,
                    "strict base read must follow effective settlement"
                );
                self.base_peeks.fetch_add(1, Ordering::SeqCst);
                return Err(crate::RuntimeError::new(
                    crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
                    "injected base loss after effective settlement",
                ));
            }
            self.owner.peek_await_event(key).await
        }
    }

    struct CatalogProbeFactory {
        store: Arc<dyn crate::RuntimePersistence>,
        opens: AtomicUsize,
        fail_open: bool,
    }

    impl CatalogProbeFactory {
        async fn new(fail_open: bool) -> Self {
            let backend = crate::support::memory_backend().await;
            Self {
                store: fixture_store(&backend, "catalog-probe").await,
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
        ) -> Result<Option<Arc<dyn crate::RuntimePersistence>>, crate::StoreError> {
            self.opens.fetch_add(1, Ordering::SeqCst);
            if self.fail_open {
                return Err(crate::StoreError::Backend(
                    "catalog unavailable".to_string(),
                ));
            }
            Ok(Some(self.store.clone()))
        }

        async fn session_was_deleted(
            &self,
            _session_id: &crate::SessionId,
        ) -> Result<bool, String> {
            Ok(false)
        }

        async fn delete_session(
            &self,
            _session_id: &crate::SessionId,
        ) -> crate::store::MaintenanceResult<crate::SessionBlobReclaimReport> {
            Ok(crate::SessionBlobReclaimReport::default())
        }

        // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
        async fn count_unsettled_turns(
            &self,
        ) -> Result<crate::store::UnsettledTurnCounts, crate::StoreError> {
            Err(crate::StoreError::UnsupportedStoreOperation {
                operation: "SessionStoreFactory::count_unsettled_turns",
            })
        }

        async fn list_turn_parks(
            &self,
            _query: &crate::store::TurnParkQuery,
        ) -> Result<Vec<crate::store::TurnPark>, crate::StoreError> {
            Err(crate::StoreError::UnsupportedStoreOperation {
                operation: "SessionStoreFactory::list_turn_parks",
            })
        }

        async fn turn_park_feed(
            &self,
            _after: crate::store::ParkFeedCursor,
            _limit: std::num::NonZeroUsize,
        ) -> Result<crate::store::ParkFeedPage<crate::store::TurnParkTarget>, crate::StoreError>
        {
            Err(crate::StoreError::UnsupportedStoreOperation {
                operation: "SessionStoreFactory::turn_park_feed",
            })
        }

        async fn compact_turn_park_feed(
            &self,
            _through: crate::store::ParkFeedCursor,
        ) -> Result<(), crate::StoreError> {
            Err(crate::StoreError::UnsupportedStoreOperation {
                operation: "SessionStoreFactory::compact_turn_park_feed",
            })
        }
    }

    fn address(label: &str) -> TurnAddress {
        TurnAddress::new(
            format!("turn-control-{label}-{}", uuid::Uuid::new_v4()),
            "turn-a",
        )
    }

    fn request(address: TurnAddress, request_id: &str) -> TurnCancelRequest {
        TurnCancelRequest::new(address, request_id, Some("user".to_string()))
            .with_reason("stop button")
    }

    /// A fresh session store for `session_id` in `backend`, whose effect host
    /// owns the session's turn control.
    async fn fixture_store(
        backend: &lash_sqlite_store::SqliteBackend,
        session_id: impl Into<crate::SessionId>,
    ) -> Arc<dyn crate::RuntimePersistence> {
        backend
            .session_store_factory()
            .create_store(&crate::testing::store_fixtures::session_store_request(
                &session_id.into(),
                "model",
                crate::SessionRelation::Root,
            ))
            .await
            .expect("create the fixture session store")
    }

    async fn bound_driver(address: &TurnAddress) -> (Arc<dyn EffectHost>, TurnWorkDriver) {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver = TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store);
        (host, driver)
    }

    fn scoped_turn_controller<'a>(
        host: &'a dyn EffectHost,
        address: &TurnAddress,
    ) -> ScopedEffectController<'a> {
        host.scoped(
            AdmittedScope::unpinned(address.execution_scope()).expect("a turn admits unpinned"),
        )
        .expect("scope turn controller")
    }

    #[tokio::test]
    async fn process_scoped_turn_control_peek_uses_admitted_effect_scope() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = TurnAddress::new(
            "session:subagent:scope-probe",
            "process:subagent:scope-probe",
        );
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active process-backed turn control");
        let process_scope = ExecutionScope::process("process:subagent:scope-probe");
        let scoped = host
            .scoped(AdmittedScope::process(crate::ProcessRef::new(
                "process:subagent:scope-probe",
                crate::ProcessIncarnation::from_registration_sequence(1),
            )))
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

    #[tokio::test]
    async fn first_gate_winner_owns_policy_and_conflict_cannot_escalate() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-conflict");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let accepted = request(address.clone(), "accepted-defer")
            .undelivered(TurnCancelDisposition::Defer)
            .mode(TurnCancelMode::AfterStep);
        assert!(matches!(
            driver
                .request_cancel(accepted.clone())
                .await
                .unwrap()
                .outcome,
            TurnCancelOutcome::Requested(_)
        ));

        let conflict = driver
            .request_cancel(
                request(address.clone(), "conflicting-drop")
                    .undelivered(TurnCancelDisposition::Drop)
                    .mode(TurnCancelMode::Immediate),
            )
            .await
            .unwrap();
        assert!(matches!(
            conflict.outcome,
            TurnCancelOutcome::PolicyConflict {
                requested: TurnCancelDisposition::Drop,
                ref accepted,
            } if accepted.request_id == "accepted-defer"
                && accepted.undelivered == TurnCancelDisposition::Defer
        ));
        let escalation = turn_escalation_key(host.as_ref(), &address).await.unwrap();
        assert_eq!(host.peek_await_event(&escalation).await.unwrap(), None);
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .unwrap()
                .unwrap()
                .request,
            accepted,
        );
    }

    #[tokio::test]
    async fn same_policy_escalation_preserves_original_policy_acceptor() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-escalation-acceptor");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver = TurnWorkDriver::for_session(host, address.session_id.clone(), store.clone());
        let accepted = request(address.clone(), "accepted-after-step")
            .undelivered(TurnCancelDisposition::Drop)
            .mode(TurnCancelMode::AfterStep);
        driver.request_cancel(accepted.clone()).await.unwrap();
        let escalated = driver
            .request_cancel(
                request(address.clone(), "timing-escalation")
                    .undelivered(TurnCancelDisposition::Drop)
                    .mode(TurnCancelMode::Immediate),
            )
            .await
            .unwrap();
        assert!(matches!(escalated.outcome, TurnCancelOutcome::Escalated(_)));
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .unwrap()
                .unwrap()
                .request,
            accepted,
            "the durable policy projection retains the base-gate acceptor",
        );
    }

    #[tokio::test]
    async fn accepted_drop_policy_refuses_a_later_defer_repeat() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-conflict-reverse");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let accepted = request(address.clone(), "accepted-drop")
            .undelivered(TurnCancelDisposition::Drop)
            .mode(TurnCancelMode::Immediate);
        assert!(matches!(
            driver
                .request_cancel(accepted.clone())
                .await
                .unwrap()
                .outcome,
            TurnCancelOutcome::Requested(_)
        ));

        // The weaker timing mode is irrelevant: the disposition decides.
        let conflict = driver
            .request_cancel(
                request(address.clone(), "conflicting-defer")
                    .undelivered(TurnCancelDisposition::Defer)
                    .mode(TurnCancelMode::AfterStep),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                conflict.outcome,
                TurnCancelOutcome::PolicyConflict {
                    requested: TurnCancelDisposition::Defer,
                    ref accepted,
                } if accepted.request_id == "accepted-drop"
                    && accepted.undelivered == TurnCancelDisposition::Drop
            ),
            "expected a typed conflict naming both policies, got {:?}",
            conflict.outcome
        );
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .unwrap()
                .unwrap()
                .request,
            accepted,
            "the conflicting repeat must not touch the durable policy projection",
        );
    }

    #[tokio::test]
    async fn concurrent_opposing_requests_converge_on_one_accepted_policy() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-conflict-concurrent");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());

        let mut callers = Vec::new();
        for index in 0..8 {
            let driver = driver.clone();
            let address = address.clone();
            let disposition = if index % 2 == 0 {
                TurnCancelDisposition::Defer
            } else {
                TurnCancelDisposition::Drop
            };
            callers.push(crate::task::spawn(async move {
                driver
                    .request_cancel(
                        request(address, &format!("racer-{index}")).undelivered(disposition),
                    )
                    .await
                    .expect("concurrent cancellation request")
            }));
        }
        let mut winners = Vec::new();
        let mut conflicts = Vec::new();
        let mut idempotent = Vec::new();
        for caller in callers {
            match caller.await.expect("join concurrent caller").outcome {
                TurnCancelOutcome::Requested(evidence) => winners.push(evidence),
                TurnCancelOutcome::AlreadyRequested(evidence) => idempotent.push(evidence),
                TurnCancelOutcome::PolicyConflict {
                    requested,
                    accepted,
                } => conflicts.push((requested, accepted)),
                other => panic!("unexpected concurrent outcome {other:?}"),
            }
        }
        assert_eq!(winners.len(), 1, "exactly one caller may accept the policy");
        let winner = winners.remove(0);
        assert_eq!(
            idempotent.len() + conflicts.len(),
            7,
            "every other caller must receive a typed repeat outcome"
        );
        for evidence in &idempotent {
            assert_eq!(
                evidence.undelivered, winner.undelivered,
                "an idempotent repeat agreed with the accepted policy"
            );
            assert_eq!(evidence.request_id, winner.request_id);
        }
        for (requested, accepted) in &conflicts {
            assert_ne!(
                *requested, winner.undelivered,
                "only a differing disposition may report a conflict"
            );
            assert_eq!(
                *accepted, winner,
                "every conflict names the one accepted request"
            );
        }
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .unwrap()
                .unwrap()
                .request
                .request_id,
            winner.request_id,
            "the durable projection converges on the gate winner",
        );
    }

    #[tokio::test]
    async fn sealed_turn_refuses_a_conflicting_repeat_without_durable_effect() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-conflict-post-terminal");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("bind cancellation store");
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        assert_eq!(
            active
                .settle_before_commit(host.as_ref(), None, None)
                .await
                .expect("seal the completing turn"),
            None,
        );

        for disposition in [TurnCancelDisposition::Defer, TurnCancelDisposition::Drop] {
            let late = driver
                .request_cancel(
                    request(address.clone(), "late-after-seal").undelivered(disposition),
                )
                .await
                .expect("late cancellation request");
            assert!(
                matches!(late.outcome, TurnCancelOutcome::CompletionWonRace),
                "a request after the gate sealed is a typed no-op, got {:?}",
                late.outcome
            );
            assert!(
                late.record.is_none(),
                "a typed no-op must not return a cancellation record"
            );
            assert_eq!(
                store
                    .turn_cancel_request(&address)
                    .await
                    .expect("read durable cancellation row"),
                None,
                "a request the sealed gate refused must leave no durable row",
            );
        }
    }

    #[tokio::test]
    async fn escalation_cannot_substitute_the_accepted_undelivered_policy() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("policy-escalation-substitution");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let accepted = request(address.clone(), "accepted-drop-after-step")
            .undelivered(TurnCancelDisposition::Drop)
            .mode(TurnCancelMode::AfterStep);
        assert!(matches!(
            driver.request_cancel(accepted).await.unwrap().outcome,
            TurnCancelOutcome::Requested(_)
        ));

        // The escalation promise is durable and shared. Write a well-formed
        // escalation that carries the opposite disposition straight onto it, the
        // way a peer binary or a replayed row could, and require every reader to
        // keep honouring the accepted policy.
        let escalation = turn_escalation_key(host.as_ref(), &address).await.unwrap();
        let substituted = TurnCancellationEvidence {
            request_id: "substituting-escalation".to_string(),
            origin: Some("peer".to_string()),
            reason: None,
            undelivered: TurnCancelDisposition::Defer,
            mode: TurnCancelMode::Immediate,
            honoured_after_step: None,
        };
        assert!(matches!(
            host.resolve_await_event(
                &escalation,
                cancel_requested_gate_resolution(substituted).unwrap(),
            )
            .await
            .unwrap(),
            crate::ResolveOutcome::Accepted
        ));

        let repeated = driver
            .request_cancel(
                request(address.clone(), "matching-repeat")
                    .undelivered(TurnCancelDisposition::Drop),
            )
            .await
            .unwrap();
        match repeated.outcome {
            TurnCancelOutcome::AlreadyRequested(evidence) => {
                assert_eq!(evidence.request_id, "substituting-escalation");
                assert_eq!(evidence.mode, TurnCancelMode::Immediate);
                assert_eq!(
                    evidence.undelivered,
                    TurnCancelDisposition::Drop,
                    "escalation may change the honoured timing, never the accepted policy"
                );
            }
            other => panic!("expected the escalation to be reported, got {other:?}"),
        }

        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let observed = active
            .observe_pending_cancel(
                &scoped_turn_controller(host.as_ref(), &address),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 0,
                },
            )
            .await
            .expect("peek the escalated gate pair")
            .expect("the escalation makes the turn stop here");
        assert_eq!(
            observed.undelivered,
            TurnCancelDisposition::Drop,
            "the owner honours the accepted policy, not the escalation's copy"
        );
        let settled = active
            .settle_before_commit(host.as_ref(), None, None)
            .await
            .expect("settle the cancelled turn")
            .expect("the gate holds a cancellation");
        assert_eq!(
            settled.undelivered,
            TurnCancelDisposition::Drop,
            "the committed evidence carries the accepted policy"
        );
    }

    #[tokio::test]
    async fn escalation_promise_persists_no_undelivered_policy() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("escalation-payload-shape");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        driver
            .request_cancel(
                request(address.clone(), "accepted-after-step")
                    .undelivered(TurnCancelDisposition::Drop)
                    .mode(TurnCancelMode::AfterStep),
            )
            .await
            .unwrap();
        let escalated = driver
            .request_cancel(
                request(address.clone(), "timing-escalation")
                    .undelivered(TurnCancelDisposition::Drop)
                    .mode(TurnCancelMode::Immediate),
            )
            .await
            .unwrap();
        assert!(matches!(escalated.outcome, TurnCancelOutcome::Escalated(_)));

        // The durable payload is the escalation promise's whole contract: it names
        // the request that won escalation and nothing about the accepted
        // undelivered-input disposition, which lives only on the base gate.
        let key = turn_escalation_key(host.as_ref(), &address).await.unwrap();
        let Some(crate::Resolution::Ok(payload)) = host.peek_await_event(&key).await.unwrap()
        else {
            panic!("the escalation promise resolved with the winning request");
        };
        assert_eq!(payload["state"], "cancel_requested");
        let cancellation = payload["cancellation"].as_object().unwrap();
        assert_eq!(cancellation["request_id"], "timing-escalation");
        assert!(
            !cancellation.contains_key("undelivered"),
            "the escalation payload must not carry a second undelivered copy: {payload}"
        );
        assert!(
            !cancellation.contains_key("honoured_after_step"),
            "the escalation payload carries no honoured-step field: {payload}"
        );
    }

    #[tokio::test]
    async fn orphan_recovery_uses_only_the_existing_gate_terminal() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let cancel_address = address("orphan-cancel-winner");
        let evidence = request(cancel_address.clone(), "durable-intent")
            .undelivered(crate::TurnCancelDisposition::Drop)
            .evidence();
        let cancel_key = turn_cancel_gate_key(host.as_ref(), &cancel_address)
            .await
            .unwrap();
        assert!(matches!(
            host.resolve_await_event(
                &cancel_key,
                cancel_requested_gate_resolution(evidence.clone()).unwrap(),
            )
            .await
            .unwrap(),
            ResolveOutcome::Accepted
        ));
        let decision =
            ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &cancel_address)
                .await
                .expect("observe durable cancellation")
                .expect("gate remains addressable");
        assert_eq!(
            decision,
            crate::TurnCancelRepairDecision::CancellationWon(evidence.clone())
        );
        assert_eq!(
            ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &cancel_address)
                .await
                .expect("peek settled cancellation gate"),
            Some(crate::TurnCancelRepairDecision::CancellationWon(evidence))
        );

        let complete_address = address("orphan-completion-winner");
        let active = ActiveTurnControl::new(host.as_ref(), complete_address.clone())
            .await
            .expect("create completion gate");
        assert_eq!(
            active
                .settle_before_commit(host.as_ref(), None, None)
                .await
                .expect("seal completion"),
            None
        );
        assert_eq!(
            ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &complete_address)
                .await
                .expect("observe completion winner"),
            Some(crate::TurnCancelRepairDecision::CancellationDidNotWin)
        );

        let revoked_address = address("orphan-revoked");
        host.revoke_await_events_for_session(&revoked_address.session_id)
            .await
            .expect("revoke orphan scope");
        assert_eq!(
            ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &revoked_address)
                .await
                .expect("revoked is an explicit no-authority result"),
            None
        );
    }

    #[tokio::test]
    async fn incoming_request_proposes_its_own_evidence_to_an_empty_gate() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("incoming-gate-candidate");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("bind cancellation store");
        let earlier =
            request(address.clone(), "earlier-row").undelivered(crate::TurnCancelDisposition::Drop);
        store
            .record_turn_cancel_request(earlier.clone())
            .await
            .expect("persist earlier intent without resolving the gate");
        let incoming = request(address.clone(), "incoming-gate")
            .undelivered(crate::TurnCancelDisposition::Defer)
            .mode(TurnCancelMode::AfterStep);
        let outcome = TurnWorkDriver::for_session(host, address.session_id.clone(), store.clone())
            .request_cancel(incoming.clone())
            .await
            .expect("resolve empty gate with incoming request");
        assert!(matches!(
            outcome.outcome,
            TurnCancelOutcome::Requested(ref evidence)
                if evidence.request_id == incoming.request_id
                    && evidence.undelivered == crate::TurnCancelDisposition::Defer
        ));
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .expect("read intent projection")
                .expect("earlier intent remains")
                .request,
            incoming,
            "the durable projection reconciles to the gate winner"
        );
    }

    #[tokio::test]
    async fn durable_commit_makes_later_cancel_a_noop_before_terminal_publication() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("durably-ended");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("bind cancellation store");
        store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &address.session_id,
                crate::TurnInputIngress::active_turn(
                    &address.turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("drop after cancellation"),
            ))
            .await
            .expect("enqueue interrupted input");
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active turn control");
        let first = request(address.clone(), "before-commit")
            .mode(TurnCancelMode::AfterStep)
            .undelivered(crate::TurnCancelDisposition::Drop);
        let receipt = driver
            .request_cancel(first.clone())
            .await
            .expect("request before commit");
        assert!(matches!(receipt.outcome, TurnCancelOutcome::Requested(_)));
        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque("turn-control-test", "turn-control-test:1"),
                "turn-control-test-executor",
                60_000,
            )
            .await
            .expect("claim final-commit lane")
            .acquired()
            .expect("test lane is free");
        let observed = store
            .turn_cancel_request_intent(&address)
            .await
            .expect("snapshot cancellation intent before authorization");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind turn cancellation authority");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                observed.clone(),
                None,
                None,
            )
            .expect("materialize closure authorization");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("authorize closure");
        let settlement = active
            .settle_authorized(host.as_ref(), &authorization, None)
            .await
            .expect("settle cancellation gate");
        let winner = settlement
            .effective_cancellation()
            .cloned()
            .expect("request won the gate");

        let mut state = crate::RuntimeSessionState {
            session_id: address.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_operation(crate::OperationId::turn(
                &address.session_id,
                &address.turn_id,
                "final",
            ))
            .expect("stamp exact turn final operation");
        commit.interrupted_turn_input_turn_id = Some(address.turn_id.clone());
        commit.interrupted_turn_input_cancellation = Some(winner);
        commit.interrupted_turn_cancel_intent = Some(observed);
        commit.turn_cancel_closure_settlement = Some(settlement);
        commit.session_execution_lease_fence = Some(lease.fence());
        let committed = store
            .commit_runtime_state(commit)
            .await
            .expect("durable final commit");
        assert_eq!(committed.turn_cancel_input_outcome.len(), 1);
        let durable = store
            .turn_cancel_request(&address)
            .await
            .expect("read cancellation record before payload reclamation")
            .expect("first request remains durable");
        assert_eq!(durable.request, first);
        assert_eq!(
            durable
                .outcome
                .expect("committed cancellation outcome")
                .len(),
            1
        );
        crate::StoreMaintenance::vacuum(store.as_ref())
            .await
            .expect("vacuum cancelled input tombstone");
        let after_vacuum = request(address.clone(), "direct-after-vacuum");
        let no_op = store
            .record_turn_cancel_request(after_vacuum.clone())
            .await
            .expect("committed request path does not decode reclaimed outcome payloads");
        assert_eq!(no_op.request, after_vacuum);
        assert!(no_op.outcome.is_none());

        // Deliberately do not publish TurnTerminal: the receipt is the completion
        // authority during this crash-sized window.
        let late = driver
            .request_cancel(request(address.clone(), "after-commit"))
            .await
            .expect("late cancellation returns a typed no-op");
        assert!(matches!(late.outcome, TurnCancelOutcome::CompletionWonRace));
        assert!(
            late.record.is_none(),
            "a terminal no-op does not decode retained cancellation payload references"
        );
    }

    struct TurnAttachProbe {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl TurnAttach for TurnAttachProbe {
        async fn await_terminal(
            &self,
            _address: &TurnAddress,
        ) -> Result<TurnTerminal, RuntimeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            panic!("a foreign exact-session address must be refused before terminal attachment")
        }
    }

    #[tokio::test]
    async fn exact_driver_rejects_foreign_terminal_attach_before_touching_the_host() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
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
        let store = fixture_store(&backend, foreign.session_id.clone()).await;
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
            host.clone(),
            foreign.session_id.clone(),
            fixture_store(&backend, foreign.session_id.clone()).await,
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
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("wrong-exact-session");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let store = fixture_store(&backend, address.session_id.clone()).await;
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

    #[tokio::test]
    async fn cancel_before_start_duplicate_and_terminal_attach() {
        let address = address("before-start");
        let (host, driver) = bound_driver(&address).await;

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
            .settle_before_commit(host.as_ref(), None, None)
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
        let address = address("assembled");
        let (host, driver) = bound_driver(&address).await;
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let assembled = TurnCancellationEvidence::internal("provider-cancelled:3");

        let settled = active
            .settle_before_commit(host.as_ref(), None, Some(assembled.clone()))
            .await
            .expect("settle")
            .expect("a locally cancelled turn settles cancelled");
        assert_eq!(settled, assembled);
        assert_ne!(settled, active_turn_internal_evidence(&active));

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
        let address = address("race");
        let (host, driver) = bound_driver(&address).await;
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let (seal, cancel) = tokio::join!(
            active.settle_before_commit(host.as_ref(), None, None),
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
    async fn authorized_completion_adopts_a_legitimate_different_cancel_winner() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("authorized-different-winner");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("admit session");
        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "authorized-different-winner",
                    "authorized-different-winner:incarnation",
                ),
                "authorized-different-winner:executor",
                60_000,
            )
            .await
            .expect("claim closure lane")
            .acquired()
            .expect("closure lane is free");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind authority");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                TurnCancelIntentSnapshot::Absent,
                None,
                None,
            )
            .expect("assemble completion authorization");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("authorize before promise resolution");

        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let cancel = request(address.clone(), "different-winner");
        let accepted = driver
            .request_cancel(cancel.clone())
            .await
            .expect("cancel wins after completion was proposed");
        assert!(matches!(accepted.outcome, TurnCancelOutcome::Requested(_)));

        let settlement = active
            .settle_authorized(host.as_ref(), &authorization, None)
            .await
            .expect("adopt the promise's actual winner");
        let settled = settlement
            .effective_cancellation()
            .cloned()
            .expect("cancel is the actual winner");
        assert_eq!(settled.request_id, cancel.request_id);
        assert_eq!(
            store
                .repair_orphaned_active_turn_inputs(
                    &address.session_id,
                    &lease.fence(),
                    &address.turn_id,
                    &store
                        .turn_cancel_request_intent(&address)
                        .await
                        .expect("refresh intent after the differing winner"),
                    Some(&settlement),
                )
                .await
                .expect("consume exact authorization under current fence"),
            crate::TurnCancelRepairResult::Applied(crate::TurnCancelInputOutcome::default())
        );
    }

    #[tokio::test]
    async fn repair_projects_authenticated_gate_winner_over_provisional_memory_row() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("authenticated-winner-repair");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("admit session");
        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "authenticated-winner-repair",
                    "authenticated-winner-repair:incarnation",
                ),
                "authenticated-winner-repair:executor",
                60_000,
            )
            .await
            .expect("claim closure lane")
            .acquired()
            .expect("closure lane is free");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind authority");
        let provisional = request(address.clone(), "provisional-row-a");
        store
            .record_turn_cancel_request(provisional)
            .await
            .expect("record provisional request row");
        let observed = store
            .turn_cancel_request_intent(&address)
            .await
            .expect("snapshot provisional row");
        let actual_winner = TurnCancellationEvidence {
            request_id: "actual-gate-winner-b".to_string(),
            origin: Some("remote-gate".to_string()),
            reason: Some("won before projection".to_string()),
            undelivered: crate::TurnCancelDisposition::Drop,
            mode: TurnCancelMode::Immediate,
            honoured_after_step: None,
        };
        // Resolve the real promise through the public cancellation path while the
        // catalog under repair still holds its distinct provisional row A. This
        // models a gate owner and a recovering catalog observing the same turn;
        // the gate owner's catalog is a second backend's, so its row B never
        // lands in the catalog under repair.
        let gate_backend = crate::support::memory_backend().await;
        let gate_store = fixture_store(&gate_backend, address.session_id.clone()).await;
        gate_store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("admit gate-owner session");
        TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), gate_store)
            .request_cancel(
                TurnCancelRequest::new(
                    address.clone(),
                    actual_winner.request_id.clone(),
                    actual_winner.origin.clone(),
                )
                .with_reason(actual_winner.reason.clone().expect("actual reason"))
                .undelivered(actual_winner.undelivered)
                .mode(actual_winner.mode),
            )
            .await
            .expect("resolve actual gate winner through public request path");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("open actual gate");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                observed.clone(),
                None,
                Some(actual_winner.clone()),
            )
            .expect("materialize exact closure");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("authorize exact closure");
        let settlement = active
            .settle_authorized(host.as_ref(), &authorization, None)
            .await
            .expect("settle against the actual promise owner");
        assert_eq!(settlement.base_cancellation(), Some(&actual_winner));
        store
            .repair_orphaned_active_turn_inputs(
                &address.session_id,
                &lease.fence(),
                &address.turn_id,
                &observed,
                Some(&settlement),
            )
            .await
            .expect("repair consumes authenticated closure")
            .into_applied()
            .expect("repair applies");

        let durable = store
            .turn_cancel_request(&address)
            .await
            .expect("read repaired row")
            .expect("authenticated winner is retained");
        assert_eq!(durable.request.request_id, actual_winner.request_id);
        assert_eq!(durable.request.origin, actual_winner.origin);
        assert_eq!(durable.request.reason, actual_winner.reason);
        assert!(
            store
                .pending_turn_cancel_closure_pins()
                .await
                .expect("read pins")
                .is_empty()
        );
        let replayed = ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &address)
            .await
            .expect("replay the actual promise winner")
            .expect("gate remains observable");
        assert!(matches!(
            replayed,
            crate::TurnCancelRepairDecision::CancellationWon(ref evidence)
                if evidence == &actual_winner
        ));
    }

    #[tokio::test]
    async fn wrong_owner_and_revoked_authorized_closure_retain_the_store_pin() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("authorized-owner-refusal");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("admit session");
        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "authorized-owner-refusal",
                    "authorized-owner-refusal:incarnation",
                ),
                "authorized-owner-refusal:executor",
                60_000,
            )
            .await
            .expect("claim closure lane")
            .acquired()
            .expect("closure lane is free");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind authority");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                TurnCancelIntentSnapshot::Absent,
                None,
                None,
            )
            .expect("assemble completion authorization");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("persist authorization before settlement");

        let wrong_owner = crate::TurnCancellationAuthority::new(
            "another-durable-owner",
            crate::support::memory_backend().await.effect_host(),
        );
        let wrong = wrong_owner
            .settle_authorized_closure(&authorization)
            .await
            .expect_err("another promise owner cannot settle this authorization");
        assert_eq!(
            wrong.code,
            crate::RuntimeErrorCode::InvalidTurnCancelRequest
        );
        assert_eq!(
            store
                .pending_turn_cancel_closure_pins()
                .await
                .expect("wrong owner leaves pin"),
            vec![authorization.clone()]
        );

        host.revoke_await_events_for_session(&address.session_id)
            .await
            .expect("revoke the exact promise owner");
        let exact_owner = crate::TurnCancellationAuthority::new(binding_id, host);
        let revoked = exact_owner
            .settle_authorized_closure(&authorization)
            .await
            .expect_err("revoked promise evidence cannot authenticate settlement");
        assert_eq!(
            revoked.code,
            crate::RuntimeErrorCode::TurnControlUnknownOrRevoked
        );
        assert_eq!(
            store
                .pending_turn_cancel_closure_pins()
                .await
                .expect("revoked evidence leaves pin"),
            vec![authorization]
        );
    }

    #[tokio::test]
    async fn authorized_settlement_refuses_base_loss_after_effective_resolution_and_retains_pin() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("base-loss-after-effective");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("admit session");
        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque(
                    "base-loss-after-effective",
                    "base-loss-after-effective:incarnation",
                ),
                "base-loss-after-effective:executor",
                60_000,
            )
            .await
            .expect("claim closure lane")
            .acquired()
            .expect("closure lane is free");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("open exact promise keys");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind authority");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                TurnCancelIntentSnapshot::Absent,
                Some(&active.internal_evidence(None)),
                None,
            )
            .expect("materialize cancellation closure");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("persist closure pin");
        let resolver = MissingBaseAfterSettleResolver {
            owner: host,
            base_key: authorization.cancel_key().clone(),
            resolve_calls: AtomicUsize::new(0),
            base_peeks: AtomicUsize::new(0),
        };
        let error = active
            .settle_authorized(&resolver, &authorization, None)
            .await
            .expect_err("missing authenticated base terminal must fail closed");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::TurnControlUnknownOrRevoked
        );
        assert_eq!(
            resolver.resolve_calls.load(Ordering::SeqCst),
            2,
            "the effective cancellation (base and escalation) was settled before the strict \
             base read failed"
        );
        assert_eq!(resolver.base_peeks.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .pending_turn_cancel_closure_pins()
                .await
                .expect("read retained pin"),
            vec![authorization]
        );
    }

    #[tokio::test]
    async fn recovered_owner_observes_pending_cancel_after_control_recreation() {
        let address = address("replay");
        let (host, driver) = bound_driver(&address).await;
        let requested = driver
            .request_cancel(request(address.clone(), "request-before-replay"))
            .await
            .expect("request cancellation");
        let expected = match requested.outcome {
            TurnCancelOutcome::Requested(evidence) => evidence,
            other => panic!("expected requested, got {other:?}"),
        };

        let scoped = host
            .scoped(
                AdmittedScope::unpinned(address.execution_scope()).expect("a turn admits unpinned"),
            )
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
            .settle_before_commit(host.as_ref(), None, None)
            .await
            .expect("settle recovered turn")
            .expect("pending cancellation survives owner loss");
        assert_eq!(settled, expected);
    }

    #[tokio::test]
    async fn turn_control_is_exact_scope_and_excluded_from_wait_cancel_sweep() {
        let address_a = address("scope");
        let (host, driver) = bound_driver(&address_a).await;
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
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("revoke");
        let store = fixture_store(&backend, address.session_id.clone()).await;
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
    async fn store_delegated_native_authority_resolves_catalog_before_local_registry() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("revoked-failing-catalog");
        host.revoke_await_events_for_session(&address.session_id)
            .await
            .expect("revoke session");
        let catalog = Arc::new(CatalogProbeFactory::new(true).await);
        let driver = TurnWorkDriver::for_catalog(host, catalog.clone());

        let error = driver
            .request_cancel(request(address, "request-after-delete"))
            .await
            .expect_err("the catalog is needed to recover Native promise authority");

        assert_eq!(error.code, crate::RuntimeErrorCode::RuntimeStore);
        assert_eq!(catalog.opens.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn catalog_driver_resolves_the_target_store_once_per_request() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("catalog-resolve-once");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        let catalog = Arc::new(CatalogProbeFactory::new(false).await);
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
        let address = address("terminal-timeout");
        let (host, driver) = bound_driver(&address).await;
        let error = driver
            .await_terminal_with_timeout(&address, Duration::from_millis(1))
            .await
            .expect_err("unpublished terminal must time out");
        assert_eq!(error.code.as_str(), "turn_terminal_await_timeout");

        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control after timed-out attach");
        active
            .settle_before_commit(host.as_ref(), None, None)
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

    #[tokio::test]
    async fn after_step_request_is_deferred_until_immediate_escalates_it() {
        let address = address("escalate");
        let (host, driver) = bound_driver(&address).await;
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

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
        // request is deferred, never honoured evidence.
        let observed = active
            .observe_pending_cancel(
                &scoped_turn_controller(host.as_ref(), &address),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 0,
                },
            )
            .await
            .expect("peek after llm");
        assert_eq!(observed, None);

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
                &scoped_turn_controller(host.as_ref(), &address),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 1,
                },
            )
            .await
            .expect("peek after escalation");
        assert_eq!(observed, Some(abort_evidence.clone()));

        let settled = active
            .settle_before_commit(host.as_ref(), observed.as_ref(), None)
            .await
            .expect("settle");
        assert_eq!(settled, Some(abort_evidence));
        let _ = stop_evidence;
    }

    #[tokio::test]
    async fn weaker_repeat_and_recovery_preserve_the_accepted_escalation() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("effective-escalation");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());

        let base = request(address.clone(), "after-step-a").mode(TurnCancelMode::AfterStep);
        assert!(matches!(
            driver
                .request_cancel(base.clone())
                .await
                .expect("accept base request")
                .outcome,
            TurnCancelOutcome::Requested(_)
        ));
        let escalation = request(address.clone(), "immediate-b");
        let escalated = driver
            .request_cancel(escalation.clone())
            .await
            .expect("accept escalation");
        assert!(matches!(
            escalated.outcome,
            TurnCancelOutcome::Escalated(ref evidence)
                if evidence.request_id == escalation.request_id
        ));

        let weaker = request(address.clone(), "after-step-c").mode(TurnCancelMode::AfterStep);
        let repeated = driver
            .request_cancel(weaker)
            .await
            .expect("repeat weaker request after escalation");
        assert!(matches!(
            repeated.outcome,
            TurnCancelOutcome::AlreadyRequested(ref evidence)
                if evidence.request_id == escalation.request_id
                    && evidence.mode == TurnCancelMode::Immediate
        ));
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .expect("read projected winner")
                .expect("winner remains durable")
                .request,
            base
        );

        let recovered = ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &address)
            .await
            .expect("observe after owner recovery")
            .expect("gate pair remains addressable");
        assert!(matches!(
            recovered,
            crate::TurnCancelRepairDecision::CancellationWon(ref evidence)
                if evidence.request_id == escalation.request_id
                    && evidence.mode == TurnCancelMode::Immediate
        ));
        assert!(matches!(
            ActiveTurnControl::peek_orphan_repair_decision(host.as_ref(), &address)
                .await
                .expect("peek effective cancellation after recovery"),
            Some(crate::TurnCancelRepairDecision::CancellationWon(ref evidence))
                if evidence.request_id == escalation.request_id
                    && evidence.mode == TurnCancelMode::Immediate
        ));
    }

    #[tokio::test]
    async fn final_settlement_refreshes_cached_base_to_an_already_projected_escalation() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("settlement-refreshes-cached-base");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let base = request(address.clone(), "after-step-base").mode(TurnCancelMode::AfterStep);
        driver
            .request_cancel(base.clone())
            .await
            .expect("accept base request");
        let honoured_base = active
            .observe_pending_cancel(
                &scoped_turn_controller(host.as_ref(), &address),
                TurnCancelPeekIdentity::AfterStep {
                    protocol_iteration: 3,
                },
            )
            .await
            .expect("observe base at step boundary")
            .expect("after-step base is honoured");
        assert_eq!(honoured_base.request_id, base.request_id);
        assert_eq!(honoured_base.honoured_after_step, Some(3));

        let escalation = request(address.clone(), "immediate-escalation");
        assert!(matches!(
            driver
                .request_cancel(escalation.clone())
                .await
                .expect("accept and project escalation")
                .outcome,
            TurnCancelOutcome::Escalated(ref evidence)
                if evidence.request_id == escalation.request_id
        ));
        assert_eq!(
            store
                .turn_cancel_request(&address)
                .await
                .expect("read projected escalation")
                .expect("escalation row")
                .request,
            base
        );

        let settled = active
            .settle_before_commit(host.as_ref(), None, None)
            .await
            .expect("settle from actual gate pair")
            .expect("cancellation won");
        assert_eq!(settled.request_id, "immediate-escalation");
        assert_eq!(settled.mode, TurnCancelMode::Immediate);
        assert_eq!(settled.honoured_after_step, None);
    }

    #[tokio::test]
    async fn final_settlement_observes_same_header_escalation_accepted_after_snapshot() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("same-header-delayed-escalation");
        let store = fixture_store(&backend, address.session_id.clone()).await;
        store
            .admit_and_bind_session(&crate::SessionBinding::root(&address.session_id))
            .await
            .expect("bind cancellation store");
        let driver =
            TurnWorkDriver::for_session(host.clone(), address.session_id.clone(), store.clone());
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");

        let base = request(address.clone(), "after-step-base")
            .undelivered(crate::TurnCancelDisposition::Drop)
            .mode(TurnCancelMode::AfterStep);
        driver
            .request_cancel(base)
            .await
            .expect("accept base request");
        let immediate = request(address.clone(), "same-immediate-header")
            .undelivered(crate::TurnCancelDisposition::Drop);
        store
            .record_turn_cancel_request(immediate.clone())
            .await
            .expect("persist unaccepted immediate header");
        let before_gate_acceptance = store
            .turn_cancel_request_intent(&address)
            .await
            .expect("snapshot immediate header before gate acceptance");
        assert_eq!(
            active
                .observe_pending_cancel(
                    &scoped_turn_controller(host.as_ref(), &address),
                    TurnCancelPeekIdentity::AfterLlm {
                        protocol_iteration: 0,
                    },
                )
                .await
                .expect("observe base before escalation"),
            None
        );

        assert!(matches!(
            driver
                .request_cancel(immediate.clone())
                .await
                .expect("accept delayed same-header escalation")
                .outcome,
            TurnCancelOutcome::Escalated(ref evidence)
                if evidence.request_id == immediate.request_id
        ));
        let after_gate_acceptance = store
            .turn_cancel_request_intent(&address)
            .await
            .expect("snapshot after same-header gate acceptance");
        assert!(matches!(
            (&before_gate_acceptance, &after_gate_acceptance),
            (
                TurnCancelIntentSnapshot::Present { revision: before, .. },
                TurnCancelIntentSnapshot::Present { request, revision: after },
            ) if request.request_id == "after-step-base" && after > before
        ));

        let lease = store
            .try_claim_session_execution_lease(
                &address.session_id,
                &crate::LeaseOwnerIdentity::opaque("same-header-test", "same-header-test:1"),
                "same-header-test-executor",
                60_000,
            )
            .await
            .expect("claim final-commit lane")
            .acquired()
            .expect("test lane is free");
        let binding_id = host.turn_control_binding_id();
        store
            .validate_turn_cancellation_binding(
                &address.session_id,
                &lease.fence(),
                &binding_id,
                &address.execution_scope(),
            )
            .await
            .expect("bind turn cancellation authority");
        let authorization = active
            .closure_authorization(
                &binding_id,
                address.execution_scope(),
                &lease.fence(),
                after_gate_acceptance.clone(),
                None,
                None,
            )
            .expect("materialize closure authorization");
        store
            .authorize_turn_cancel_closure(&lease.fence(), &authorization)
            .await
            .expect("authorize closure");
        let settlement = active
            .settle_authorized(host.as_ref(), &authorization, None)
            .await
            .expect("close and observe escalation before final commit");
        let settled = settlement
            .effective_cancellation()
            .cloned()
            .expect("accepted escalation wins");
        assert_eq!(settled.request_id, immediate.request_id);
        assert_eq!(settled.mode, TurnCancelMode::Immediate);

        let pending = store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &address.session_id,
                crate::TurnInputIngress::active_turn(
                    &address.turn_id,
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text("same-header winner decides this input"),
            ))
            .await
            .expect("enqueue interrupted input");
        let mut state = crate::RuntimeSessionState {
            session_id: address.session_id.clone(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        state.ensure_agent_frame_initialized();
        let (mut commit, _) = crate::RuntimeCommit::persisted_state_for_test(&state, &[])
            .with_operation(crate::OperationId::turn(
                &address.session_id,
                &address.turn_id,
                "final",
            ))
            .expect("stamp exact final operation");
        commit.interrupted_turn_input_turn_id = Some(address.turn_id.clone());
        commit.interrupted_turn_input_cancellation = Some(settled);
        commit.interrupted_turn_cancel_intent = Some(before_gate_acceptance);
        commit.turn_cancel_closure_settlement = Some(settlement);
        commit.session_execution_lease_fence = Some(lease.fence());
        assert!(matches!(
            store.commit_runtime_state(commit.clone()).await,
            Err(crate::StoreError::TurnCancelIntentChanged { .. })
        ));
        commit.interrupted_turn_cancel_intent = Some(after_gate_acceptance);
        let receipt = store
            .commit_runtime_state(commit)
            .await
            .expect("refreshed same-header snapshot commits the gate winner");
        assert_eq!(receipt.turn_cancel_input_outcome.len(), 1);
        assert_eq!(
            receipt.turn_cancel_input_outcome.affected_inputs[0].input_id,
            pending.input_id
        );
        assert_eq!(
            receipt.turn_cancel_input_outcome.affected_inputs[0].disposition,
            crate::TurnCancelDisposition::Drop,
            "the accepted escalation's disposition, not the stale base, is applied"
        );
    }

    #[tokio::test]
    async fn after_step_request_is_honoured_at_the_step_boundary_with_its_iteration() {
        let address = address("boundary");
        let (host, driver) = bound_driver(&address).await;
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        driver
            .request_cancel(request(address.clone(), "stop-1").mode(TurnCancelMode::AfterStep))
            .await
            .expect("after-step request");
        assert_eq!(
            active
                .observe_pending_cancel(
                    &scoped_turn_controller(host.as_ref(), &address),
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
                &scoped_turn_controller(host.as_ref(), &address),
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
        let settled = active
            .settle_before_commit(host.as_ref(), Some(&honoured), None)
            .await
            .expect("settle");
        assert_eq!(settled, Some(honoured));
        // Final settlement closes escalation admission. An Immediate request that
        // arrives afterward observes the already-honoured base request and cannot
        // retroactively replace the turn-ending decision.
        let late = driver
            .request_cancel(request(address.clone(), "abort-late"))
            .await
            .expect("late escalation");
        assert!(matches!(
            late.outcome,
            TurnCancelOutcome::AlreadyRequested(ref evidence)
                if evidence.request_id == "stop-1"
                    && evidence.mode == TurnCancelMode::AfterStep
        ));
    }

    /// F7 (FIG-3672 P9): a host-local stop takes the gate mid-turn with
    /// lash's internal evidence, which chose no undelivered-input policy. A
    /// host request routed afterwards adopts it: its identity and its `Drop`
    /// policy are what the turn settles, not the internal default.
    #[tokio::test]
    async fn a_routed_request_after_a_local_stop_keeps_its_drop_policy() {
        let address = address("local-then-routed");
        let (host, driver) = bound_driver(&address).await;
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        active
            .request_local_stop(
                host.await_event_resolver(),
                TurnCancelMode::Immediate,
                Some("user".to_string()),
            )
            .await
            .expect("the local stop takes the gate");
        let honoured = active
            .observe_pending_cancel(
                &scoped_turn_controller(host.as_ref(), &address),
                TurnCancelPeekIdentity::AfterLlm {
                    protocol_iteration: 0,
                },
            )
            .await
            .expect("peek after the model call")
            .expect("the local stop is honoured");
        assert!(honoured.is_internal());

        let routed = driver
            .request_cancel(
                request(address.clone(), "host-drop").undelivered(TurnCancelDisposition::Drop),
            )
            .await
            .expect("route the host request");
        assert!(matches!(
            routed.outcome,
            TurnCancelOutcome::Requested(ref evidence)
                if evidence.request_id == "host-drop"
                    && evidence.undelivered == TurnCancelDisposition::Drop
        ));
        assert_eq!(
            routed
                .record
                .as_ref()
                .map(|record| record.request.undelivered),
            Some(TurnCancelDisposition::Drop),
            "the durable request row names the adopting request's policy"
        );

        let settled = active
            .settle_before_commit(host.as_ref(), Some(&honoured), None)
            .await
            .expect("settle")
            .expect("the turn settles cancelled");
        assert_eq!(settled.request_id, "host-drop");
        assert_eq!(settled.undelivered, TurnCancelDisposition::Drop);

        // A host request can never pose as lash's own evidence.
        let forged = driver
            .request_cancel(request(address.clone(), "internal:forged"))
            .await
            .expect_err("the internal namespace is reserved");
        assert_eq!(
            forged.code,
            crate::RuntimeErrorCode::InvalidTurnCancelRequest
        );
    }

    #[tokio::test]
    async fn a_local_after_step_stop_is_a_durable_request_that_lands_at_the_boundary() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("local-after-step");
        let active = ActiveTurnControl::new(host.as_ref(), address.clone())
            .await
            .expect("active control");
        active
            .request_local_stop(
                host.await_event_resolver(),
                TurnCancelMode::AfterStep,
                Some("shutdown".to_string()),
            )
            .await
            .expect("resolve own gate");
        let honoured = active
            .observe_pending_cancel(
                &scoped_turn_controller(host.as_ref(), &address),
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

        // Without a boundary the durable request still settles the final
        // commit as an after-step stop.
        let commit_only = ActiveTurnControl::new(host.as_ref(), self::address("local-commit"))
            .await
            .expect("active control");
        commit_only
            .request_local_stop(host.await_event_resolver(), TurnCancelMode::AfterStep, None)
            .await
            .expect("resolve own gate");
        let settled = commit_only
            .settle_before_commit(host.as_ref(), None, None)
            .await
            .expect("settle")
            .expect("the local stop settles as cancelled");
        assert_eq!(settled.mode, TurnCancelMode::AfterStep);
        assert_eq!(settled.honoured_after_step, None);
    }

    #[tokio::test]
    async fn a_forwarded_local_stop_escalates_through_the_gate_pair() {
        let backend = crate::support::memory_backend().await;
        let host: Arc<dyn EffectHost> = backend.effect_host();
        let address = address("forwarded-local-stop");
        let active = Arc::new(
            ActiveTurnControl::new(host.as_ref(), address.clone())
                .await
                .expect("active control"),
        );
        let stop = crate::runtime::LocalTurnStop::new();
        let _forwarding = stop
            .forward_to(Arc::clone(&active), Arc::clone(&host))
            .await;
        stop.request(TurnCancelMode::AfterStep, Some("shutdown".to_string()));
        stop.request(TurnCancelMode::Immediate, Some("user".to_string()));
        // The watch a step body runs sees the forwarded stop through the gate
        // pair alone: the base gate's after-step request, then its escalation.
        let watched = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            active.watch_immediate(host.await_event_resolver()),
        )
        .await
        .expect("the forwarded stop reaches the gate pair")
        .expect("watch the gate pair")
        .expect("an escalated stop");
        assert_eq!(watched.mode, TurnCancelMode::Immediate);
        assert_eq!(watched.origin.as_deref(), Some("shutdown"));
        assert_eq!(watched.request_id, format!("internal:{}", address.turn_id));
    }
}
