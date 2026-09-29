//! L-D1 through L-D4: the recorded session close and its retained tombstone.

use lash_core::testing::RuntimeStoreTestDriveExt as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::engine::{NoScopeClose, ScopeCloseSink};
use lash_core::store::{
    AdmissionId, ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState,
    DriveEpochSeal, ObligationKind, ObligationState, RootStartNonce, RootTerminalCause,
    RootTerminalKind, RootTerminalWrite, TurnCommitId, TurnParkWrite,
};
use lash_core::{
    DeploymentStore, NoSessionWork, SessionAdministration, SessionDeleteContext,
    SessionDeleteExecution, SessionId, StoreError, StoreSet, TurnId,
};

pub(super) struct CloseSink {
    factory: Arc<dyn DeploymentStore>,
    calls: Mutex<Vec<(ControlIntentId, Vec<TurnId>)>>,
    /// How many of the next closes fail.
    failures: AtomicUsize,
}

impl CloseSink {
    pub(super) fn new(factory: Arc<dyn DeploymentStore>, failures: usize) -> Arc<Self> {
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

pub(super) fn administration(
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
            scopes: scopes.clone(),
            // The `ScopeClose` kind's relay over the law's stores (ADR 0109
            // §3): the close's engine half gives each closed root's armed
            // obligation its immediate delivery here.
            scope_close_obligations: Arc::new(lash_core::runtime::drive::ScopeCloseRelay::new(
                stores.obligation_ledger(crate::store::ObligationKind::ScopeClose),
                stores.session_store_factory(),
                scopes,
            )),
            intents: stores.obligation_ledger(ObligationKind::ControlIntent),
            clock: stores.clock(),
            deletes: lash_core::session_delete::SessionDeleteStores::of_store_set(Arc::clone(
                stores,
            )),
        },
    )
}

/// The `ControlIntent` relay a deployment's reconcile tick runs, over the
/// store set's ledger and `scopes`, on `clock`.
pub(super) fn intent_relay(
    stores: &Arc<dyn StoreSet>,
    scopes: Arc<dyn ScopeCloseSink>,
    clock: Arc<dyn lash_core::Clock>,
) -> lash_core::drive::ControlIntentRelay {
    lash_core::drive::ControlIntentRelay::new(
        stores.obligation_ledger(ObligationKind::ControlIntent),
        stores.session_store_factory(),
        Arc::new(NoSessionWork::new()),
        Arc::clone(&scopes),
        Arc::new(lash_core::runtime::drive::ScopeCloseRelay::new(
            stores.obligation_ledger(ObligationKind::ScopeClose),
            stores.session_store_factory(),
            scopes,
        )),
        clock,
    )
}

pub(super) async fn session(
    stores: &Arc<dyn StoreSet>,
    prefix: &str,
    law: &str,
) -> (SessionId, Arc<dyn crate::RuntimeStore>) {
    let id = SessionId::from(format!("{prefix}-{law}"));
    let store = super::law_session_store(stores.as_ref(), &id).await;
    (id, store)
}

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

/// The deletion's close as one attempt of the engine's `SessionDelete`
/// handler, reporting the intent it answered through `tx`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn close_attempt(
    admin: &SessionAdministration,
    id: &SessionId,
    tx: tokio::sync::mpsc::UnboundedSender<ControlIntent>,
) -> crate::ConformanceTurnAttempt {
    let admin = admin.clone();
    let id = id.clone();
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
            let _ = tx.send(
                outcome
                    .expect("session close")
                    .expect("session exists")
                    .intent,
            );
            crate::ConformanceTurnEnd::Settled
        })
    })
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn close(
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            lash_core::AdmittedScope::session_delete(id),
            close_attempt(admin, id, tx),
        )
        .await;
    rx.recv().await.expect("handler completed the close")
}

/// Run the deletion's close until `crash` fires, then kill it where it
/// stands: inside the engine's handler on a tier with a runner, in the
/// calling task otherwise. The tier's recovery is the next [`close`].
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn close_until_crash(
    admin: &SessionAdministration,
    id: &SessionId,
    runner: Option<&Arc<dyn crate::ConformanceTurnRunner>>,
    crash: &crate::ConformanceCrash,
) {
    let Some(runner) = runner else {
        let context = admin.delete_context(id.as_str()).expect("delete context");
        tokio::select! {
            biased;
            () = crash.fired() => {}
            closed = lash_core::session_close::close_session(&context) => {
                panic!("the close ended ({:?}) before its crash fired", closed.map(|closed| closed.map(|closed| closed.intent.id)))
            }
        }
        return;
    };
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn_until_crash(
            lash_core::AdmittedScope::session_delete(id),
            close_attempt(admin, id, tx),
            crash.clone(),
        )
        .await;
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
            after_redrive: None,
            build_generation: None,
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

/// Pin session `id` with a pending turn-cancel closure: a turn that
/// authorized its closure and has not yet committed past it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn pin_a_turn_cancel_closure(store: &dyn crate::RuntimeStore, id: &SessionId) {
    let lease = store
        .seal_drive_epoch_for_test(
            id,
            &crate::LeaseOwnerIdentity::opaque("close-law", "close-law:incarnation"),
            "close-law:executor",
            60_000,
        )
        .await
        .expect("claim closure lease")
        .acquired()
        .expect("closure lease is free");
    let address = crate::TurnAddress::new(id, TurnId::from("pinned-turn"));
    let scope = address.execution_scope();
    let binding = crate::turn_control_binding_id_for_scope("s7c-close-law", &scope)
        .expect("bind closure scope");
    store
        .validate_turn_cancellation_binding(id, &lease, &binding, &scope)
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
        &lease,
    )
    .expect("construct closure authorization");
    store
        .authorize_turn_cancel_closure(&lease, &authorization)
        .await
        .expect("pin the session");
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
    pin_a_turn_cancel_closure(store.as_ref(), &id).await;
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
            .list_control_intents(None, std::num::NonZeroUsize::new(10).expect("positive"))
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
/// its obligation due again, and its relay still finishes it after the
/// session is deleted; the intent then answers the deleted session's roots
/// as its tombstone.
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
    let intents = stores.obligation_ledger(ObligationKind::ControlIntent);
    let retained = async |intent: Option<ControlIntent>, calls: usize| {
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
        assert_eq!(
            intents
                .state(intent.obligation.as_ref().expect("armed by the close"))
                .await
                .expect("obligation state"),
            Some(ObligationState::Due),
            "its obligation is handed back for its next attempt"
        );
        assert_eq!(sink.calls().len(), calls, "one engine half per attempt");
    };
    let first = close(&admin, &id, runner.as_ref()).await;
    retained(factory.load_intent(first.id).await.expect("load intent"), 1).await;
    let again = close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        again.id, first.id,
        "a retried deletion answers the same close"
    );
    assert_eq!(again.kind, first.kind);
    retained(factory.load_intent(first.id).await.expect("load intent"), 2).await;
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
    let relay = intent_relay(&stores, sink.clone(), stores.clock());
    let apply = || relay.deliver_intent(&first);
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

/// The registry's scope owner, refusing its first session close: what a
/// deletion whose close intent's engine half failed leaves behind.
struct FailingFirstRegistryClose {
    registry: crate::RegistryScopeClose,
    fail_next: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ScopeCloseSink for FailingFirstRegistryClose {
    async fn close_root_scope(
        &self,
        terminal: &lash_core::store::RootTerminal,
    ) -> Result<(), StoreError> {
        self.registry.close_root_scope(terminal).await
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(StoreError::Backend(
                "the law's scope owner refuses its first session close".into(),
            ));
        }
        self.registry
            .close_session_scope(session, intent, roots)
            .await
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the registry answers its own read"
)]
async fn close_row(
    registry: &dyn crate::ProcessRegistry,
    scope: &lash_core::ScopeId,
) -> Option<crate::ParentEndPlan> {
    registry
        .get_parent_end_plan(scope)
        .await
        .expect("read the session's close row")
}

/// L-D5 (FIG-3607 D11): a session's deletion writes exactly one close row
/// for its `Session` scope, and only through its `CloseSession` intent. The
/// deletion of the session's process state writes none, so while the
/// intent's engine half is retained the scope stays open; the intent's
/// re-application writes the row, and a second application leaves that one
/// row as it was.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_delete_writes_exactly_one_close_row_via_its_intent(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, _) = session(&stores, prefix, "close-row").await;
    let factory = stores.session_store_factory();
    let registry = stores.process_registry();
    let session_scope = lash_core::ScopeId::session(id.clone());
    let sink = Arc::new(FailingFirstRegistryClose {
        registry: crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock()),
        fail_next: std::sync::atomic::AtomicBool::new(true),
    });
    let admin = administration(host, &stores, sink.clone());
    let intent = close(&admin, &id, runner.as_ref()).await;
    assert!(
        close_row(registry.as_ref(), &session_scope).await.is_none(),
        "the close intent's engine half failed, so its scope is still open"
    );
    registry
        .delete_session_process_state(&id)
        .await
        .expect("delete the session's process state");
    factory.delete_session(&id).await.expect("delete session");
    assert!(
        close_row(registry.as_ref(), &session_scope).await.is_none(),
        "the deletion itself writes no close row: the intent is its one owner"
    );

    let relay = intent_relay(&stores, sink.clone(), stores.clock());
    let apply = || relay.deliver_intent(&intent);
    assert!(matches!(
        apply().await.expect("re-apply the retained close"),
        ControlIntentState::Acknowledged { .. }
    ));
    let row = close_row(registry.as_ref(), &session_scope)
        .await
        .expect("the intent's re-application writes the session's close row");
    assert_eq!(row.parent, session_scope);
    assert!(matches!(
        apply().await.expect("apply an acknowledged close"),
        ControlIntentState::Acknowledged { .. }
    ));
    assert_eq!(
        close_row(registry.as_ref(), &session_scope).await,
        Some(row),
        "one close row, kept as the intent first wrote it"
    );
}

/// The registry's scope owner, crashing the execution on its first session
/// close: it fires `crash` and never answers, so the close's engine half
/// writes nothing, the way a process that dies between the `CloseSession`
/// intent's commit and its acknowledgement leaves it.
struct CrashingRegistryClose {
    registry: crate::RegistryScopeClose,
    crash: crate::ConformanceCrash,
    armed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ScopeCloseSink for CrashingRegistryClose {
    async fn close_root_scope(
        &self,
        terminal: &lash_core::store::RootTerminal,
    ) -> Result<(), StoreError> {
        self.registry.close_root_scope(terminal).await
    }

    async fn close_session_scope(
        &self,
        session: &SessionId,
        intent: ControlIntentId,
        roots: &[TurnId],
    ) -> Result<(), StoreError> {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.crash.fire();
            std::future::pending::<()>().await;
        }
        self.registry
            .close_session_scope(session, intent, roots)
            .await
    }
}

/// L-D6 (FIG-3607 item 7): a crash between a session's `CloseSession` intent
/// and its acknowledgement leaves the intent open and durable, and recovery
/// finishes the close. The intent then outlives the session and the
/// deployment's evidence retention as the tombstone the deleted session's
/// roots are answered from (ADR 0108 §5).
///
/// The crash lands inside the close's engine half: the store half already
/// ended the session's roots and stopped it admitting, and the crashed
/// delivery's claim on the intent's obligation is left to lapse. The tier's
/// recovery — the engine's redelivery of its `SessionDelete` handler, or a
/// retried deletion in process — answers the same intent, and the
/// obligation's relay retakes the lapsed claim (ADR 0109 §1.8: by
/// `claimed_at + claim_ttl + T`), closes the session's scope and
/// acknowledges it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_close_interrupted_before_its_acknowledgement_is_finished_and_its_tombstone_kept(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, store) = session(&stores, prefix, "close-crash").await;
    let active = TurnId::from("close-crash-root");
    store
        .bind_root_inputs(&id, &active, &[])
        .await
        .expect("record the session's active root");
    let factory = stores.session_store_factory();
    let registry = stores.process_registry();
    let session_scope = lash_core::ScopeId::session(id.clone());
    let crash = crate::ConformanceCrash::new();
    let sink = Arc::new(CrashingRegistryClose {
        registry: crate::RegistryScopeClose::new(Arc::clone(&registry), stores.clock()),
        crash: crash.clone(),
        armed: std::sync::atomic::AtomicBool::new(true),
    });
    let admin = administration(host, &stores, sink);

    close_until_crash(&admin, &id, runner.as_ref(), &crash).await;
    assert!(crash.has_fired(), "the close died inside its engine half");
    let open = factory
        .list_control_intents(None, std::num::NonZeroUsize::new(64).expect("positive"))
        .await
        .expect("list the intents")
        .into_iter()
        .find(|intent| intent.session_id == id)
        .expect("the interrupted close is kept");
    let intents = stores.obligation_ledger(ObligationKind::ControlIntent);
    let obligation = open.obligation.clone().expect("armed by the close");
    assert_eq!(
        intents.state(&obligation).await.expect("obligation state"),
        Some(ObligationState::Claimed),
        "the crashed delivery's claim holds the obligation until it lapses"
    );
    assert_eq!(
        open.kind,
        ControlIntentKind::CloseSession {
            roots: vec![active.clone()]
        }
    );
    assert_eq!(open.state, ControlIntentState::Pending);
    assert_eq!(
        store
            .root_terminal(&id, &active)
            .await
            .expect("read the closed root")
            .map(|terminal| terminal.cause),
        Some(RootTerminalCause::SessionDeleted { intent: open.id }),
        "the store half ended the root before the crash"
    );
    assert!(
        close_row(registry.as_ref(), &session_scope).await.is_none(),
        "the crash took the scope close"
    );

    // The tier's recovery answers the same close, and the relay retakes
    // its lapsed claim and finishes it.
    let recovered = close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        recovered.id, open.id,
        "recovery answers the interrupted close"
    );
    let policy = lash_core::drive::relay::RelayPolicy::default();
    let later: Arc<dyn lash_core::Clock> =
        super::root_control::ShiftedClock::new(stores.clock(), policy.claim_ttl_ms + 10_000);
    let relay = intent_relay(
        &stores,
        Arc::new(crate::RegistryScopeClose::new(
            Arc::clone(&registry),
            stores.clock(),
        )),
        Arc::clone(&later),
    );
    let pass = lash_core::drive::relay::relay_due(
        &relay,
        later.as_ref(),
        std::num::NonZeroUsize::new(64).expect("positive"),
    )
    .await
    .expect("relay pass");
    assert!(
        pass.delivered >= 1,
        "the relay retook the lapsed claim: {pass:?}"
    );
    assert_eq!(
        intents.state(&obligation).await.expect("obligation state"),
        Some(ObligationState::Delivered)
    );
    assert!(
        matches!(
            factory
                .load_intent(open.id)
                .await
                .expect("load the close")
                .expect("the close is kept")
                .state,
            ControlIntentState::Acknowledged { .. }
        ),
        "recovery acknowledged the close"
    );
    assert!(
        close_row(registry.as_ref(), &session_scope).await.is_some(),
        "recovery closed the session's scope"
    );

    // The rest of the deletion, then the deployment's evidence retention.
    registry
        .delete_session_process_state(&id)
        .await
        .expect("delete the session's process state");
    factory.delete_session(&id).await.expect("delete session");
    factory
        .reclaim_retained_evidence(lash_core::store::RetentionBound {
            committed_before_epoch_ms: u64::MAX,
        })
        .await
        .expect("reclaim every retained evidence");
    assert!(
        factory
            .load_intent(open.id)
            .await
            .expect("load the tombstone")
            .is_some(),
        "retention keeps the deletion tombstone"
    );
    for root in [active, TurnId::from("close-crash-other-root")] {
        let terminal = factory
            .root_terminal(&id, &root)
            .await
            .expect("read a deleted session's root")
            .expect("the tombstone answers every root of the deleted session");
        assert_eq!(terminal.kind, RootTerminalKind::Cancelled);
        assert_eq!(
            terminal.cause,
            RootTerminalCause::SessionDeleted { intent: open.id }
        );
    }
}

/// L-D10 (FIG-3873 S3): a turn that pinned its cancel closure after the
/// deletion asked its refusals, and before its close committed, has its
/// final commit cut short by the close. The deletion retried after that
/// close replays its recorded step and asks no refusal: it answers the
/// recorded close, and the pin is the physical delete's to retire (L-D11).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_deletion_retried_after_its_close_is_not_refused_by_a_pin_the_close_superseded(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn StoreSet>,
    runner: Option<Arc<dyn crate::ConformanceTurnRunner>>,
) {
    let (id, store) = session(&stores, prefix, "close-superseded-pin").await;
    let factory = stores.session_store_factory();
    pin_a_turn_cancel_closure(store.as_ref(), &id).await;
    // The close committed with the pin in place.
    let committed = factory
        .begin_session_close(&id, stores.clock().timestamp_ms())
        .await
        .expect("commit the close's store half")
        .expect("the session exists");
    let admin = administration(host, &stores, CloseSink::new(Arc::clone(&factory), 0));
    let retried = close(&admin, &id, runner.as_ref()).await;
    assert_eq!(
        retried.id, committed.id,
        "the retried deletion answers the recorded close"
    );
    assert!(
        matches!(
            factory
                .load_intent(committed.id)
                .await
                .expect("load the close")
                .expect("the close is kept")
                .state,
            ControlIntentState::Acknowledged { .. }
        ),
        "the retried deletion delivered the close's engine half"
    );
    assert_eq!(
        factory
            .pending_turn_cancel_closure_pins(&id)
            .await
            .expect("read the pins")
            .len(),
        1,
        "the superseded pin is left to the physical delete"
    );
}
