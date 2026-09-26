//! L-D1 through L-D4: the recorded session close and its retained tombstone.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::engine::{NoScopeClose, ScopeCloseSink};
use lash_core::store::{
    AdmissionId, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    DriveEpochSeal, RootStartNonce, RootTerminalCause, RootTerminalKind, RootTerminalWrite,
    TurnCommitId, TurnParkWrite,
};
use lash_core::{
    NoSessionWork, SessionAdministration, SessionDeleteContext, SessionDeleteExecution, SessionId,
    SessionStoreFactory, StoreError, StoreSet, TurnId,
};

struct CloseSink {
    factory: Arc<dyn SessionStoreFactory>,
    calls: Mutex<Vec<(ControlIntentId, Vec<TurnId>)>>,
    /// How many of the next closes fail.
    failures: AtomicUsize,
}

impl CloseSink {
    fn new(factory: Arc<dyn SessionStoreFactory>, failures: usize) -> Arc<Self> {
        Arc::new(Self {
            factory,
            calls: Mutex::new(Vec::new()),
            failures: AtomicUsize::new(failures),
        })
    }

    fn calls(&self) -> Vec<(ControlIntentId, Vec<TurnId>)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl ScopeCloseSink for CloseSink {
    async fn close_root_scope(&self, _: &lash_core::store::RootTerminal) -> Result<(), StoreError> {
        Ok(())
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError> {
        for root in roots {
            let terminal = self.factory.root_terminal(session, root).await?;
            assert!(matches!(
                terminal,
                Some(terminal) if terminal.cause == RootTerminalCause::SessionDeleted { intent }
            ));
        }
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((intent, roots.to_vec()));
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(StoreError::Backend("injected scope close failure".into()));
        }
        Ok(())
    }
}

fn administration(
    host: Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn StoreSet>,
    scopes: Arc<dyn ScopeCloseSink>,
) -> SessionAdministration {
    SessionAdministration::new(
        stores.session_store_factory(),
        host,
        None,
        None,
        stores.process_env_store(),
        lash_core::ProcessEngineRegistry::new(),
        lash_core::session_close::SessionCloseServices {
            work: Arc::new(NoSessionWork::new()),
            scopes,
            clock: stores.clock(),
        },
    )
}

async fn session(
    stores: &Arc<dyn StoreSet>,
    prefix: &str,
    law: &str,
) -> (SessionId, Arc<dyn crate::RuntimePersistence>) {
    let id = SessionId::from(format!("{prefix}-{law}"));
    let store = super::law_session_store(stores.as_ref(), &id).await;
    (id, store)
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn close(
    admin: &SessionAdministration,
    id: &SessionId,
    runner: Option<&Arc<dyn crate::ConformanceTurnRunner>>,
) -> ControlIntent {
    let Some(runner) = runner else {
        return lash_core::session_close::close_session(
            &admin.delete_context(id.as_str()).expect("delete context"),
        )
        .await
        .expect("session close")
        .expect("session exists")
        .intent;
    };
    struct HandlerExecution<'a> {
        admin: SessionAdministration,
        scoped: lash_core::ScopedEffectController<'a>,
    }
    impl SessionDeleteExecution for HandlerExecution<'_> {
        fn administration(&self) -> &SessionAdministration {
            &self.admin
        }

        fn scoped<'a>(
            &'a self,
            _: lash_core::AdmittedScope,
        ) -> Result<lash_core::ScopedEffectController<'a>, lash_core::RuntimeError> {
            Ok(self.scoped.clone())
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let admin = admin.clone();
    let id = id.clone();
    runner
        .run_turn(
            lash_core::AdmittedScope::session_delete(&id),
            Arc::new(move |scope| {
                let execution = HandlerExecution {
                    admin: admin.clone(),
                    scoped: scope,
                };
                let id = id.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let outcome = lash_core::session_close::close_session(
                        &SessionDeleteContext::from_execution(&execution, id.as_str())
                            .expect("handler delete context"),
                    )
                    .await;
                    tx.send(
                        outcome
                            .expect("session close")
                            .expect("session exists")
                            .intent,
                    )
                    .expect("law receiver");
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    rx.recv().await.expect("handler completed the close")
}

/// L-D1: the transaction ends active and parked roots before the scope owner
/// runs, and the deleted session answers both roots from its tombstone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_closes_active_and_parked_roots_as_session_deleted(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, store) = session(&stores, prefix, "close-roots").await;
    let active = TurnId::from("active-root");
    let parked = TurnId::from("parked-root");
    store
        .bind_root_inputs(&id, &active, &[])
        .await
        .expect("record active root");
    store
        .record_turn_park(&TurnParkWrite {
            session_id: id.clone(),
            turn_id: parked.clone(),
            reason: lash_core::store::ParkReason::ReplayDivergence {
                message: "parked before session close".into(),
            },
            at_ms: 1,
            engine: None,
        })
        .await
        .expect("record parked root");
    let factory = stores.session_store_factory();
    let sink = CloseSink::new(Arc::clone(&factory), 0);
    let admin = administration(host, &stores, sink.clone());
    let intent = close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        intent.kind,
        ControlIntentKind::CloseSession {
            roots: vec![active.clone(), parked.clone()]
        }
    );
    assert_eq!(
        sink.calls(),
        vec![(intent.id, vec![active.clone(), parked.clone()])]
    );
    for root in [&active, &parked] {
        let terminal = store
            .root_terminal(&id, root)
            .await
            .expect("read live root")
            .expect("close transaction wrote terminal evidence");
        assert_eq!(
            terminal.cause,
            RootTerminalCause::SessionDeleted { intent: intent.id }
        );
    }
    assert!(
        store
            .load_turn_park(&id)
            .await
            .expect("park read")
            .is_none()
    );
    factory.delete_session(&id).await.expect("delete session");
    for root in [active, parked] {
        let terminal = factory
            .root_terminal(&id, &root)
            .await
            .expect("read deleted root")
            .expect("deletion tombstone answers root");
        assert_eq!(terminal.kind, RootTerminalKind::Cancelled);
        assert_eq!(
            terminal.cause,
            RootTerminalCause::SessionDeleted { intent: intent.id }
        );
    }
}

/// L-D2: a pending turn-cancel closure pins the session, so deletion refuses
/// before the close transaction or scope owner runs.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_refused_deletion_closes_nothing(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    _runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, store) = session(&stores, prefix, "refused-close").await;
    let lease = store
        .try_claim_session_execution_lease(
            &id,
            &crate::LeaseOwnerIdentity::opaque("close-law", "close-law:incarnation"),
            "close-law:executor",
            60_000,
        )
        .await
        .expect("claim closure lease")
        .acquired()
        .expect("closure lease is free");
    let address = crate::TurnAddress::new(&id, TurnId::from("pinned-turn"));
    let scope = address.execution_scope();
    let binding = crate::turn_control_binding_id_for_scope("s7c-close-law", &scope)
        .expect("bind closure scope");
    store
        .validate_turn_cancellation_binding(&id, &lease.fence(), &binding, &scope)
        .await
        .expect("validate closure binding");
    let key = |suffix: &str, wait| crate::AwaitEventKey {
        scope: scope.clone(),
        wait,
        key_id: format!("pinned-turn:{suffix}"),
        signature: format!("close-law:{suffix}"),
    };
    let authorization = crate::TurnCancelClosureAuthorization::new(
        address,
        binding,
        scope.clone(),
        key("cancel", crate::AwaitEventWaitIdentity::TurnCancelGate),
        key(
            "escalation",
            crate::AwaitEventWaitIdentity::TurnCancelEscalation,
        ),
        key("terminal", crate::AwaitEventWaitIdentity::TurnTerminal),
        crate::TurnCancelClosureProposal::CompletionSealed,
        crate::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )
    .expect("construct closure authorization");
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await
        .expect("pin the session");
    let factory = stores.session_store_factory();
    let sink = CloseSink::new(Arc::clone(&factory), 0);
    let admin = administration(host, &stores, sink.clone());
    let context = admin.delete_context(id.as_str()).expect("delete context");
    assert!(matches!(
        lash_core::session_close::close_session(&context).await,
        Err(lash_core::session_close::SessionCloseError::Store(
            StoreError::TurnCancelClosureLifecyclePinned {
                pending_count: 1,
                ..
            }
        ))
    ));
    assert!(
        factory
            .list_open_control_intents(None, std::num::NonZeroUsize::new(10).expect("positive"))
            .await
            .expect("intent list")
            .is_empty()
    );
    assert!(sink.calls().is_empty());
    store
        .bind_root_inputs(&id, &TurnId::from("still-open"), &[])
        .await
        .expect("refused close leaves the session writable");
}

/// L-D3: a retried deletion replays its recorded close and answers the same
/// intent; an engine half that failed is retained on the intent, retryable,
/// and still finishes after the session is deleted; the intent then answers
/// the deleted session's roots as its tombstone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_close_intent_is_idempotent_retained_on_failure_and_survives_deletion(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, _) = session(&stores, prefix, "close-retry").await;
    let factory = stores.session_store_factory();
    let sink = CloseSink::new(Arc::clone(&factory), 2);
    let admin = administration(host, &stores, sink.clone());
    let retained = |intent: Option<ControlIntent>, attempts: u32| {
        let intent = intent.expect("the close intent is kept");
        assert!(
            matches!(
                intent.state,
                ControlIntentState::Failed {
                    retryable: true,
                    ..
                }
            ),
            "a failed engine half is retained, retryable: {intent:?}"
        );
        assert_eq!(intent.attempts, attempts);
    };
    let first = close(&admin, &id, runner.as_ref()).await;
    retained(factory.load_intent(first.id).await.expect("load intent"), 1);
    let again = close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        again.id, first.id,
        "a retried deletion answers the same close"
    );
    assert_eq!(again.kind, first.kind);
    retained(factory.load_intent(first.id).await.expect("load intent"), 2);
    assert_eq!(
        factory
            .begin_session_close(&id, 999)
            .await
            .expect("begin again")
            .map(|intent| intent.id),
        Some(first.id),
        "the store half is idempotent per session"
    );
    factory
        .delete_session(&id)
        .await
        .expect("delete after close");
    let (work, clock) = (NoSessionWork::new(), stores.clock());
    let apply = || {
        lash_core::drive::apply_control_intent(
            factory.as_ref(),
            &lash_core::engine::NoEngineControl,
            &work,
            sink.as_ref(),
            &first,
            clock.as_ref(),
        )
    };
    assert!(matches!(
        apply()
            .await
            .expect("finish the engine half after deletion"),
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(sink.calls().len(), 3);
    assert!(matches!(
        apply().await.expect("apply an acknowledged close"),
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(sink.calls().len(), 3, "an acknowledged close runs nothing");
    let terminal = factory
        .root_terminal(&id, &TurnId::from("any-root"))
        .await
        .expect("tombstone read")
        .expect("the deleted session answers from its tombstone");
    assert_eq!(terminal.kind, RootTerminalKind::Cancelled);
    assert_eq!(
        terminal.cause,
        RootTerminalCause::SessionDeleted { intent: first.id }
    );
}

/// L-D4: a head commit carrying the old admission fence cannot land after
/// close raises the epoch, even if the head revision itself is unchanged.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_commit_racing_a_close_is_refused_stale_fence(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, store) = session(&stores, prefix, "close-fence").await;
    let fence = match store
        .seal_drive_epoch(
            &id,
            &AdmissionId::new("root#0"),
            0,
            &RootStartNonce::new("root"),
        )
        .await
        .expect("seal root")
    {
        DriveEpochSeal::Sealed(fence) => fence,
        other => panic!("root must seal: {other:?}"),
    };
    let factory = stores.session_store_factory();
    let admin = administration(host, &stores, Arc::new(NoScopeClose));
    let close = close(&admin, &id, runner.as_ref()).await;
    let mut state = lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
        lash_core::TurnBudget::Unbounded,
    ));
    state.session_id = id.clone();
    state.ensure_agent_frame_initialized();
    let root = TurnId::from("racing-root");
    let operation = crate::OperationId::turn(id.as_str(), root.as_str(), "final");
    let mut graph = state.pending_graph_commit();
    graph
        .derive_node_ids(&id, &operation)
        .expect("derive node ids");
    let mut commit = crate::RuntimeCommit::persisted_state_with_graph_commit_and_operation(
        &state,
        graph,
        &[],
        operation,
    )
    .expect("build racing commit");
    commit.drive_fence = Some(Box::new(fence));
    commit.root_terminal = Some(Box::new(RootTerminalWrite {
        root: root.clone(),
        turn: root.clone(),
        commit: TurnCommitId::new(root.clone(), 0),
        stop: None,
    }));
    assert!(matches!(
        store.commit_runtime_state(commit).await,
        Err(StoreError::StaleDriveFence { .. })
    ));
    assert!(
        store
            .root_terminal(&id, &root)
            .await
            .expect("terminal read")
            .is_none()
    );
    assert!(
        factory
            .load_intent(close.id)
            .await
            .expect("close intent")
            .is_some()
    );
}
