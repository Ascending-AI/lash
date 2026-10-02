//! FIG-4765: a root's recorded executor decides who runs it.
//!
//! A child session's turn is driven by its acceptor, inline in the parent's
//! execution, and its acceptance holds the row's ingress claim for the
//! relay's claim TTL (FIG-4728). The claim is a lease: an acceptor that is
//! alive but slower than it loses the claim to a relay pass, whose ask has
//! the session's drive run the row's root as an engine run of its own. The
//! seal of the root's admission records that run as its executor, and from
//! then on the record, not the lease, excludes every other execution an
//! engine holds: the acceptor seals nothing over the run's fence and admits
//! nothing beside it, and once the root has ended it answers the outcome
//! that run committed (FIG-4814).
//!
//! The laws drive both executions through the tier's turn runner: the
//! acceptor as a child session's turn under its turn scope, and the relay's
//! ask as the engine's admission and run of the root.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::engine::{AdmitVerdict, RootOutcome};
use lash_core::store::{ObligationKind, RootExecutor, RootTerminalKind};
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use super::drive_admission::{DriveParts, driver_scope, on_tier};
use crate::admit;

/// How long a law waits for a step the tier owes it.
const STEP: std::time::Duration = std::time::Duration::from_secs(60);

#[expect(
    clippy::panic,
    reason = "conformance-law fixture: the tier reaches the step within its budget"
)]
async fn within<T>(what: &str, step: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, step)
        .await
        .unwrap_or_else(|_| panic!("{what} within {STEP:?}"))
}

/// Holds the acceptor at the start of its drive admission.
///
/// The session's first read of its drive epoch is the acceptor's admission
/// reading the epoch its seal will raise: it is held until the law resumes
/// it, so the acceptor has accepted its input and admitted nothing, and
/// what it then observes is the session as the root's executor left it.
/// Once resumed, every further read of the unfinished root is a re-decision
/// of that admission (a refused attempt's retry on Restate) and is held
/// until the law settles, so the retries spend none of the invocation's
/// attempt budget while the root's executor is still running it.
struct AdmissionGate {
    inner: Arc<dyn crate::RuntimeStore>,
    reads: AtomicUsize,
    decisions: AtomicUsize,
    held: tokio::sync::Notify,
    resumed: tokio::sync::watch::Sender<bool>,
    redecided: AtomicBool,
    redecided_wake: tokio::sync::Notify,
    settled: tokio::sync::watch::Sender<bool>,
}

impl AdmissionGate {
    fn over(inner: &Arc<dyn crate::RuntimeStore>) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::clone(inner),
            reads: AtomicUsize::new(0),
            decisions: AtomicUsize::new(0),
            held: tokio::sync::Notify::new(),
            resumed: tokio::sync::watch::channel(false).0,
            redecided: AtomicBool::new(false),
            redecided_wake: tokio::sync::Notify::new(),
            settled: tokio::sync::watch::channel(false).0,
        })
    }

    /// Let the held acceptor read the unfinished root and decide.
    fn resume(&self) {
        self.resumed.send_replace(true);
    }

    /// Let every held re-decision read.
    fn settle(&self) {
        self.settled.send_replace(true);
    }

    /// Resolves once a resumed acceptor's admission is decided again.
    async fn redecided(&self) {
        loop {
            let wake = self.redecided_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.redecided.load(Ordering::SeqCst) {
                return;
            }
            wake.await;
        }
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for AdmissionGate {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn drive_epoch(
        &self,
        session_id: &SessionId,
    ) -> Result<crate::store::StoredDriveEpoch, crate::StoreError> {
        if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
            self.held.notify_one();
            let _ = self.resumed.subscribe().wait_for(|resumed| *resumed).await;
        }
        self.inner.drive_epoch(session_id).await
    }

    async fn unfinished_root(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<crate::store::UnfinishedRoot>, crate::StoreError> {
        // The resumed acceptor's own decision reads through; what follows
        // it is a retry.
        if *self.resumed.borrow()
            && !*self.settled.borrow()
            && self.decisions.fetch_add(1, Ordering::SeqCst) > 0
        {
            self.redecided.store(true, Ordering::SeqCst);
            self.redecided_wake.notify_waiters();
            let _ = self.settled.subscribe().wait_for(|settled| *settled).await;
        }
        self.inner.unfinished_root(session_id).await
    }
}

/// One law's session: its acceptor's turn, the gate that holds it, and the
/// model its root asks.
struct Fixture {
    parts: DriveParts,
    gate: Arc<AdmissionGate>,
    turn_id: TurnId,
    /// Notified once the root's first model call arrived.
    asked: Arc<tokio::sync::Notify>,
    /// Notify it to answer the root's first model call, when the law holds it.
    answer: Arc<tokio::sync::Notify>,
    calls: Arc<AtomicUsize>,
}

/// How the acceptor's turn answered: the outcome of the root that drove its
/// input.
type Accepted = Result<crate::TurnOutcome, crate::RuntimeError>;

impl Fixture {
    async fn new(
        prefix: &str,
        law: &str,
        host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        hold_model: bool,
    ) -> Self {
        let mut parts = DriveParts::new(prefix, law, host, stores, 1).await;
        let gate = AdmissionGate::over(&parts.store);
        parts.store = Arc::clone(&gate) as Arc<dyn crate::RuntimeStore>;
        let asked = Arc::new(tokio::sync::Notify::new());
        let answer = Arc::new(tokio::sync::Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let model = crate::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let asked = Arc::clone(&asked);
                let answer = Arc::clone(&answer);
                let calls = Arc::clone(&calls);
                move |_request| {
                    let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
                    let asked = Arc::clone(&asked);
                    let answer = Arc::clone(&answer);
                    async move {
                        if first {
                            asked.notify_one();
                            if hold_model {
                                answer.notified().await;
                            }
                        }
                        Ok(crate::LlmResponse {
                            parts: vec![crate::LlmOutputPart::Text {
                                text: ANSWER.to_string(),
                                response_meta: None,
                            }],
                            ..crate::LlmResponse::default()
                        })
                    }
                }
            })
            .build();
        parts.host.providers.models =
            crate::testing::standard_test_llm_profiles(model.into_handle());
        let turn_id = TurnId::fixture(format!("{law}-turn"));
        Self {
            parts,
            gate,
            turn_id,
            asked,
            answer,
            calls,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// The session's drive epoch, read past the gate.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own epoch"
    )]
    async fn epoch(&self) -> crate::store::StoredDriveEpoch {
        self.gate
            .inner
            .drive_epoch(&self.parts.session_id)
            .await
            .expect("read the session's drive epoch")
    }

    /// The session's unfinished root, read past the gate.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own roots"
    )]
    async fn unfinished(&self) -> Option<crate::store::UnfinishedRoot> {
        self.gate
            .inner
            .unfinished_root(&self.parts.session_id)
            .await
            .expect("read the unfinished root")
    }

    /// The scope the acceptor's execution runs its child session's turn
    /// under.
    fn acceptor_scope(&self) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::turn(
            &self.parts.session_id,
            self.turn_id.clone(),
        ))
    }

    /// One attempt of the acceptor: it accepts its input and drives the row
    /// itself, and reports how the turn answered when the attempt returns.
    fn acceptor(
        &self,
        answers: tokio::sync::mpsc::UnboundedSender<Accepted>,
    ) -> crate::ConformanceTurnAttempt {
        Self::acceptor_of(&self.parts, &self.turn_id, answers)
    }

    /// [`Self::acceptor`] over `parts`, the acceptor's own view of the
    /// session.
    fn acceptor_of(
        parts: &DriveParts,
        turn_id: &TurnId,
        answers: tokio::sync::mpsc::UnboundedSender<Accepted>,
    ) -> crate::ConformanceTurnAttempt {
        let parts = parts.clone();
        let turn_id = turn_id.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let turn_id = turn_id.clone();
            let answers = answers.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                let mut input = crate::TurnInput::text("the accepted words");
                input.trace_turn_id = Some(turn_id);
                let turn = runtime
                    .drive_child_session_turn(
                        input,
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = answers.send(turn.map(|turn| turn.outcome));
                end
            })
        })
    }

    /// The acceptance's row, open and held by nothing but its lapsed claim.
    #[expect(
        clippy::expect_used,
        clippy::panic,
        reason = "conformance-law fixture: the acceptance wrote its one row"
    )]
    async fn accepted_input(&self) -> crate::InputId {
        let open = self
            .parts
            .store
            .list_pending_turn_inputs(&self.parts.session_id)
            .await
            .expect("read pending inputs");
        let [accepted] = open.as_slice() else {
            panic!("the acceptance left its one row open: {open:?}");
        };
        accepted.input.input_id.clone()
    }

    /// A relay pass after the acceptor's claim lapsed: it retakes the claim
    /// of `input`'s ingress obligation, which is what asks the session's
    /// drive for the row.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the ledger answers its relay pass"
    )]
    async fn relay_retakes_the_lapsed_claim(&self, input: &crate::InputId) {
        let backend = self.parts.host.backend();
        let ingress = backend.obligation_ledger(ObligationKind::Ingress);
        let ttl_ms = lash_core::drive::relay::RelayPolicy::default().claim_ttl_ms;
        let obligation = lash_core::store::ObligationKey::Ingress {
            session_id: self.parts.session_id.clone(),
            item_id: input.as_str().to_string(),
        }
        .id();
        let lapsed = ingress
            .claim_due(
                backend.clock().timestamp_ms().saturating_add(ttl_ms),
                ttl_ms,
                std::num::NonZeroUsize::new(64).expect("non-zero"),
            )
            .await
            .expect("a relay pass reads the ledger");
        assert!(
            lapsed.iter().any(|claimed| claimed.id == obligation),
            "the acceptor's lapsed claim is retaken: {lapsed:?}"
        );
    }

    /// The relay's ask: the session's drive admits the accepted row's root
    /// and runs it as the engine's own run of that root.
    #[expect(
        clippy::panic,
        reason = "conformance-law fixture: the drive admits the accepted row"
    )]
    fn spawn_session_drive(
        &self,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
    ) -> tokio::task::JoinHandle<Result<RootOutcome, lash_core::engine::DriveAbort>> {
        let parts = self.parts.clone();
        let runner = Arc::clone(runner);
        let request = parts.request("relay-ask");
        let root = self.turn_id.clone();
        tokio::spawn(async move {
            on_tier(&runner, &parts, move |mut runtime, scope| {
                let request = request.clone();
                let root = root.clone();
                Box::pin(async move {
                    let admitted = match lash_core::drive::admit_drive(
                        &mut runtime,
                        &scope,
                        &request,
                        0,
                        None,
                    )
                    .await?
                    {
                        AdmitVerdict::Admit(admitted) => admitted,
                        other => {
                            panic!("the session's drive admits the accepted row: {other:?}")
                        }
                    };
                    assert_eq!(admitted.root(), &root);
                    lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted).await
                })
            })
            .await
        })
    }

    /// The root ended answered, by one model call, with nothing parked and
    /// nothing left pending.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store answers its own reads"
    )]
    async fn assert_driven_once_to_its_end(&self) {
        assert_eq!(self.calls(), 1, "one execution asked the model");
        let store = &self.parts.store;
        let session = &self.parts.session_id;
        assert_eq!(
            store
                .root_terminal(session, &self.turn_id)
                .await
                .expect("read the root's terminal")
                .map(|terminal| terminal.kind()),
            Some(RootTerminalKind::Answered),
            "the root ended answered"
        );
        assert!(
            store
                .load_turn_park(session)
                .await
                .expect("read the park")
                .is_none(),
            "nothing parked"
        );
        assert!(
            store
                .list_pending_turn_inputs(session)
                .await
                .expect("read pending inputs")
                .is_empty(),
            "the accepted row was answered"
        );
    }
}

/// A slow acceptor past its claim TTL is never driven twice (FIG-4765).
///
/// The acceptor accepts its input and stalls before its admission. Its claim
/// lapses, a relay pass retakes it, and the session's drive admits the root
/// and runs it as the engine's own run: the root's admission records that
/// executor. The acceptor then wakes while the root is still running. It
/// seals nothing and admits nothing: its admission is refused retryably,
/// naming the wait, and the recorded executor drives the root to its end
/// under the one fence that was ever sealed. The acceptor's retry then finds
/// its input answered, runs nothing, and answers the root's outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_recorded_under_one_executor_is_never_admitted_by_another(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "one-executor", &host, &stores, true).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let accepting = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = f.acceptor_scope();
        let attempt = f.acceptor(answers.clone());
        async move { runner.run_turn(scope, attempt).await }
    });
    within("the acceptor reaches its admission", f.gate.held.notified()).await;
    let input = f.accepted_input().await;
    f.relay_retakes_the_lapsed_claim(&input).await;

    let driving = f.spawn_session_drive(&runner);
    within("the root's run asks its model", f.asked.notified()).await;
    let sealed = f.epoch().await;
    assert_eq!(
        f.unfinished()
            .await
            .map(|unfinished| (unfinished.root, unfinished.executor)),
        Some((f.turn_id.clone(), RootExecutor::Root)),
        "the root is recorded under the engine's own run"
    );

    // The acceptor wakes while the root runs under its recorded executor.
    f.gate.resume();
    let refused = within("the acceptor decides its admission", async {
        tokio::select! {
            answer = answered.recv() => Some(answer.expect("the acceptor's attempt reports")),
            () = f.gate.redecided() => None,
        }
    })
    .await;
    assert_eq!(
        f.calls(),
        1,
        "the acceptor never runs a root recorded under another executor"
    );
    assert_eq!(
        f.epoch().await,
        sealed,
        "the acceptor seals nothing over the recorded executor's fence"
    );
    // In process the refused attempt returns; on Restate it ends inside its
    // admission step and the engine retries the open invocation.
    if let Some(refused) = &refused {
        let refusal = refused
            .as_ref()
            .expect_err("the acceptor waits for the root's recorded executor");
        assert_eq!(
            refusal.code,
            crate::RuntimeErrorCode::SessionRootPending,
            "{refusal:?}"
        );
        assert!(refusal.is_retryable(), "{refusal:?}");
    }

    // The recorded executor drives the root to its end.
    f.answer.notify_one();
    let outcome = within("the root's run ends", driving)
        .await
        .expect("the session's drive ran")
        .expect("the root's one executor commits it");
    assert!(
        matches!(
            &outcome,
            RootOutcome::Committed {
                outcome: crate::TurnOutcome::Finished(_),
                ..
            }
        ),
        "{outcome:?}"
    );

    // The acceptor's retry finds its input answered and drives nothing.
    f.gate.settle();
    if refused.is_some() {
        within("the acceptor's first attempt ends", accepting)
            .await
            .expect("the acceptor ran");
        runner
            .run_turn(f.acceptor_scope(), f.acceptor(answers))
            .await;
    } else {
        within("the acceptor's retry ends", accepting)
            .await
            .expect("the acceptor ran");
    }
    let retried = within("the acceptor's retry answers", answered.recv())
        .await
        .expect("the acceptor's retry reports");
    let adopted = retried.expect("the acceptor answers what its root's executor committed");
    assert!(
        matches!(&adopted, crate::TurnOutcome::Finished(_)),
        "{adopted:?}"
    );
    f.assert_driven_once_to_its_end().await;
    assert_eq!(
        f.epoch().await,
        sealed,
        "one seal: nothing superseded the root's executor"
    );
}

/// A lost acceptor's root is still driven to its end exactly once
/// (FIG-4765).
///
/// The acceptor dies between its acceptance and its admission, having
/// recorded nothing, so no executor owns the root yet. The relay retakes its
/// lapsed claim and the session's drive runs the root as the engine's own
/// run, to its end. The acceptor's execution, recovered afterwards, finds
/// its input answered, drives nothing, and answers the root's outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_lost_acceptors_root_is_driven_once_by_the_sessions_drive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "lost-acceptor", &host, &stores, false).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let crash = crate::ConformanceCrash::new();
    let dying = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = f.acceptor_scope();
        let attempt = f.acceptor(answers.clone());
        let crash = crash.clone();
        async move { runner.run_turn_until_crash(scope, attempt, crash).await }
    });
    within("the acceptor reaches its admission", f.gate.held.notified()).await;
    crash.fire();
    within("the acceptor dies", dying)
        .await
        .expect("the acceptor ran until its crash");
    let input = f.accepted_input().await;
    assert_eq!(
        f.unfinished().await,
        None,
        "the lost acceptor recorded no root"
    );
    f.relay_retakes_the_lapsed_claim(&input).await;

    let outcome = within(
        "the session's drive runs the root",
        f.spawn_session_drive(&runner),
    )
    .await
    .expect("the session's drive ran")
    .expect("the session's drive commits the lost acceptor's root");
    assert!(
        matches!(
            &outcome,
            RootOutcome::Committed {
                outcome: crate::TurnOutcome::Finished(_),
                ..
            }
        ),
        "{outcome:?}"
    );
    f.assert_driven_once_to_its_end().await;
    let sealed = f.epoch().await;

    // The acceptor's execution is recovered: its root already ended under
    // the session's drive.
    f.gate.resume();
    f.gate.settle();
    within(
        "the acceptor's recovery ends",
        runner.run_turn(f.acceptor_scope(), f.acceptor(answers)),
    )
    .await;
    let recovered = within("the acceptor's recovery answers", answered.recv())
        .await
        .expect("the acceptor's recovery reports");
    let adopted = recovered.expect("the acceptor answers what the session's drive committed");
    assert!(
        matches!(&adopted, crate::TurnOutcome::Finished(_)),
        "{adopted:?}"
    );
    f.assert_driven_once_to_its_end().await;
    assert_eq!(f.epoch().await, sealed, "the recovery seals nothing");
}

/// The store's half of the rule (FIG-4765): `admit_root` reads a recorded
/// admission back to the executor that recorded it and to a drive no engine
/// holds, and refuses another engine-held executor typed, under whatever
/// fence it asks. The refused request takes nothing and changes nothing.
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn admit_root_refuses_another_engine_held_executor(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    use crate::testing::store_fixtures::{admit_root_request_for_test, seal_drive_fence_for_test};
    let process = |label: &str| RootExecutor::Acceptor {
        scope: crate::ExecutionScope::process(crate::ProcessId::fixture(label)),
    };
    let session_drive = RootExecutor::Inline {
        scope: crate::ExecutionScope::session_operation("root", "drive-admission:a-drain"),
    };
    for (law, recorded) in [
        ("recorded-root", RootExecutor::Root),
        ("recorded-process", process("the acceptor")),
    ] {
        let parts = DriveParts::new(prefix, law, &host, &stores, 1).await;
        let input = parts.enqueue("the accepted words", Some("held-root")).await;
        let root = TurnId::from("held-root");
        let head = crate::store::AdmittedHead::Input(input);
        let request = |fence: &crate::store::DriveFence, executor: &RootExecutor| {
            let mut request = admit_root_request_for_test(fence, &root, head.clone());
            request.executor = executor.clone();
            request
        };
        let first = seal_drive_fence_for_test(&parts.store, &parts.session_id, "first").await;
        let admission = parts
            .store
            .admit_root(&request(&first, &recorded))
            .await
            .expect("the first admission records the root")
            .expect("the first admission reaches its head");
        assert_eq!(admission.executor, recorded, "{law}");

        // Another engine-held executor, under the session's current fence.
        let later = seal_drive_fence_for_test(&parts.store, &parts.session_id, "later").await;
        for other in [
            RootExecutor::Root,
            process("the acceptor"),
            process("another process"),
        ] {
            let answer = parts.store.admit_root(&request(&later, &other)).await;
            if other == recorded {
                let read_back = answer
                    .expect("the recorded executor reads its admission back")
                    .expect("the admission is recorded");
                assert_eq!(read_back.executor, recorded, "{law}");
                continue;
            }
            match answer {
                Err(crate::StoreError::RootHeldByAnotherExecutor {
                    session_id,
                    root: held,
                    recorded: by,
                    admitting,
                }) => {
                    assert_eq!(session_id, parts.session_id, "{law}");
                    assert_eq!(held, root, "{law}");
                    assert_eq!(*by, recorded, "{law}");
                    assert_eq!(*admitting, other, "{law}");
                }
                answer => panic!("{law}: {other:?} is refused typed: {answer:?}"),
            }
        }
        // A drive no engine holds resumes the root under the drive fence, and
        // reads the same record.
        let resumed = parts
            .store
            .admit_root(&request(&later, &session_drive))
            .await
            .expect("an in-process drive resumes the root")
            .expect("the admission is recorded");
        assert_eq!(
            resumed.executor, recorded,
            "{law}: the record never changes"
        );
        assert_eq!(
            parts
                .store
                .unfinished_root(&parts.session_id)
                .await
                .expect("read the unfinished root")
                .map(|unfinished| (unfinished.root, unfinished.executor)),
            Some((root.clone(), recorded.clone())),
            "{law}"
        );
    }
}

/// The text the fixture's model answers a root's one call with.
const ANSWER: &str = "answered by the root's one executor";

/// A refused acceptor adopts its root's recorded outcome (FIG-4814).
///
/// The acceptor accepts its input and stalls before its admission. Its claim
/// lapses, a relay pass retakes it, and the session's drive runs the root to
/// its end as the engine's own run. The acceptor then wakes: its input is
/// answered, so it drives nothing and seals nothing, and its turn answers
/// what the root's recorded executor committed. The process whose child turn
/// the relay took completes with that outcome.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_refused_acceptor_adopts_the_outcome_its_roots_executor_recorded(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "acceptor-adopts", &host, &stores, false).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let accepting = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = f.acceptor_scope();
        let attempt = f.acceptor(answers);
        async move { runner.run_turn(scope, attempt).await }
    });
    within("the acceptor reaches its admission", f.gate.held.notified()).await;
    let input = f.accepted_input().await;
    f.relay_retakes_the_lapsed_claim(&input).await;

    let outcome = within(
        "the session's drive runs the root",
        f.spawn_session_drive(&runner),
    )
    .await
    .expect("the session's drive ran")
    .expect("the session's drive commits the relay-taken root");
    let RootOutcome::Committed {
        outcome: committed, ..
    } = outcome
    else {
        panic!("the root's executor committed it: {outcome:?}");
    };
    let sealed = f.epoch().await;

    // The acceptor wakes: its root ended under its recorded executor.
    f.gate.resume();
    f.gate.settle();
    within("the acceptor's attempt ends", accepting)
        .await
        .expect("the acceptor ran");
    let adopted = within("the acceptor answers", answered.recv())
        .await
        .expect("the acceptor reports")
        .expect("the acceptor answers its root's recorded outcome");
    assert_eq!(adopted, committed, "the acceptor adopts the root's outcome");
    assert_eq!(
        adopted,
        crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage {
            text: ANSWER.to_string()
        })
    );
    f.assert_driven_once_to_its_end().await;
    assert_eq!(f.epoch().await, sealed, "the acceptor seals nothing");
}

/// What one executor did to the session's drive, in the order the store
/// answered.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Drove {
    /// `by`'s seal raised the drive epoch to `epoch`.
    Sealed { by: &'static str, epoch: u64 },
    /// `by`'s admission recorded the root, or read its record back.
    Admitted { by: &'static str },
}

/// Records what one actor's seals and root admissions answered.
struct DriveLog {
    inner: Arc<dyn crate::RuntimeStore>,
    by: &'static str,
    log: Arc<std::sync::Mutex<Vec<Drove>>>,
}

impl DriveLog {
    fn record(&self, drove: Drove) {
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(drove);
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for DriveLog {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &crate::store::AdmissionId,
        observed_epoch: u64,
        root_start: &crate::store::RootStartNonce,
        hold: Option<&crate::store::RootHold>,
    ) -> Result<crate::store::DriveEpochSeal, crate::StoreError> {
        let before = self.inner.drive_epoch(session_id).await?.epoch;
        let seal = self
            .inner
            .seal_drive_epoch(session_id, admission, observed_epoch, root_start, hold)
            .await?;
        if let crate::store::DriveEpochSeal::Sealed(fence) = &seal
            && fence.epoch() > before
        {
            self.record(Drove::Sealed {
                by: self.by,
                epoch: fence.epoch(),
            });
        }
        Ok(seal)
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, crate::StoreError> {
        let admission = self.inner.admit_root(request).await?;
        if admission.is_some() {
            self.record(Drove::Admitted { by: self.by });
        }
        Ok(admission)
    }
}

/// No order of a root's owner and another admitter supersedes the owner's
/// fence (FIG-4814).
///
/// Two executions an engine holds race for one accepted row: its acceptor,
/// and the session's drive a relay pass asked for it. Each is held before it
/// reads the drive epoch its admission observes, before its seal, before
/// its root admission, and before the acceptor reads the root that took its
/// input, and the explorer runs every order of those calls. An engine
/// retries a refused admission and a waiting acceptor, so each is bounded
/// to two steps ahead of the other. Whatever the order, once a root's admission is recorded no
/// seal raises the drive epoch again: the executor the record names keeps
/// its fence to the root's end, the root is driven once, and it ends
/// answered.
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn no_order_of_a_roots_owner_and_another_admitter_supersedes_the_owners_fence(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    use crate::StoreOp;
    let mut explorer = crate::interleave::Explorer::new("owner-versus-admitter").holding(&[
        StoreOp::drive_epoch.into(),
        StoreOp::seal_drive_epoch.into(),
        StoreOp::admit_root.into(),
        StoreOp::root_of_input.into(),
    ]);
    let mut raced = 0;
    while let Some(mut schedule) = explorer.next_schedule() {
        let law = format!("explore-owner-{}", schedule.index());
        let f = Fixture::new(prefix, &law, &host, &stores, false).await;
        // The explorer holds the actors; the fixture's own gate holds none.
        f.gate.resume();
        f.gate.settle();
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut actor = |by: &'static str| {
            let logged: Arc<dyn crate::RuntimeStore> = Arc::new(DriveLog {
                inner: Arc::clone(&f.parts.store),
                by,
                log: Arc::clone(&log),
            });
            let mut parts = f.parts.clone();
            parts.store = schedule.actor(by, logged);
            parts
        };
        let (owner, acceptor) = (actor("session"), actor("acceptor"));
        schedule.yields_after("session", 2);
        schedule.yields_after("acceptor", 2);

        // The relay's ask: the session's drive, retried while its admission
        // is refused.
        let session_drive = async {
            let (reports, mut reported) = tokio::sync::mpsc::unbounded_channel();
            let parts = owner.clone();
            let request = owner.request("relay-ask");
            let attempt: crate::ConformanceTurnAttempt = Arc::new(move |scope| {
                let parts = parts.clone();
                let request = request.clone();
                let reports = reports.clone();
                Box::pin(async move {
                    let mut runtime = parts.runtime().await;
                    let drove = match lash_core::drive::admit_drive(
                        &mut runtime,
                        &scope,
                        &request,
                        0,
                        None,
                    )
                    .await
                    {
                        Ok(AdmitVerdict::Admit(admitted)) => {
                            lash_core::drive::run_admitted_root(&mut runtime, &scope, admitted)
                                .await
                                .map(Some)
                        }
                        Ok(_) => Ok(None),
                        Err(abort) => Err(abort),
                    }
                    .map_err(lash_core::engine::DriveAbort::into_error);
                    let end = crate::ConformanceTurnEnd::of(&drove);
                    let _ = reports.send(drove);
                    end
                })
            });
            loop {
                runner
                    .run_turn(driver_scope(&owner), Arc::clone(&attempt))
                    .await;
                match reported.recv().await.expect("the session's drive reports") {
                    Err(refusal) if refusal.code == crate::RuntimeErrorCode::SessionRootPending => {
                    }
                    drove => break drove.map(|_| ()),
                }
            }
        };
        // The acceptor's turn, retried while its root's executor runs it.
        let accepting = async {
            let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
            let attempt = Fixture::acceptor_of(&acceptor, &f.turn_id, answers);
            loop {
                runner
                    .run_turn(f.acceptor_scope(), Arc::clone(&attempt))
                    .await;
                match answered.recv().await.expect("the acceptor reports") {
                    Err(refusal) if refusal.code == crate::RuntimeErrorCode::SessionRootPending => {
                    }
                    answer => break answer.map(|_| ()),
                }
            }
        };
        schedule
            .run(vec![
                ("session", Box::pin(session_drive)),
                ("acceptor", Box::pin(accepting)),
            ])
            .await;

        let drove = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let rendered = schedule.rendered();
        let recorded = drove
            .iter()
            .position(|drove| matches!(drove, Drove::Admitted { .. }))
            .unwrap_or_else(|| panic!("no executor recorded the root: {drove:?} in `{rendered}`"));
        let Drove::Admitted { by: owner } = drove[recorded] else {
            unreachable!("the position names an admission")
        };
        let superseding: Vec<&Drove> = drove[recorded..]
            .iter()
            .filter(|drove| matches!(drove, Drove::Sealed { .. }))
            .collect();
        assert!(
            superseding.is_empty(),
            "a seal superseded the fence of `{owner}`, the root's recorded executor: {drove:?} \
             in `{rendered}`"
        );
        assert!(
            drove[recorded..]
                .iter()
                .all(|drove| *drove == Drove::Admitted { by: owner }),
            "only `{owner}` admits the root it recorded: {drove:?} in `{rendered}`"
        );
        if drove[..recorded]
            .iter()
            .filter(|drove| matches!(drove, Drove::Sealed { .. }))
            .count()
            > 1
            || schedule
                .trace()
                .iter()
                .filter(|call| {
                    call.op == StoreOp::seal_drive_epoch.into()
                        && call.phase == lash_core::testing::Phase::Before
                })
                .count()
                > 1
        {
            raced += 1;
        }
        f.assert_driven_once_to_its_end().await;
    }
    assert!(raced > 0, "no schedule had both executors seal");
}

/// A parent-turn acceptor's root is recorded as an acceptor's, and the store
/// closes it to a later drive (FIG-4814).
///
/// The acceptor drives its child session's turn under a turn scope. Its
/// root's admission records it as an acceptor, an execution an engine holds,
/// never as a drive no engine holds. While the root runs, the session's own
/// run of it is refused by the store: its seal raises nothing, its root
/// admission is refused typed under the session's current fence, and
/// nothing changes. The acceptor drives the root to its end.
#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_parent_turn_acceptors_root_is_closed_to_a_later_drive(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let f = Fixture::new(prefix, "acceptor-recorded", &host, &stores, true).await;
    f.gate.resume();
    f.gate.settle();
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let accepting = tokio::spawn({
        let runner = Arc::clone(&runner);
        let scope = f.acceptor_scope();
        let attempt = f.acceptor(answers);
        async move { runner.run_turn(scope, attempt).await }
    });
    within("the acceptor's root asks its model", f.asked.notified()).await;
    let acceptor = RootExecutor::Acceptor {
        scope: crate::ExecutionScope::turn(&f.parts.session_id, f.turn_id.clone()),
    };
    let unfinished = f.unfinished().await.expect("the acceptor's root runs");
    assert_eq!(
        (&unfinished.root, &unfinished.executor),
        (&f.turn_id, &acceptor),
        "the root is recorded under its acceptor"
    );

    // A later drive: the session's own run of the root.
    let sealed = f.epoch().await;
    let fence = lash_core::store::current_drive_fence(f.gate.inner.as_ref(), &f.parts.session_id)
        .await
        .expect("read the session's fence")
        .expect("the acceptor sealed its admission");
    let seal = f
        .gate
        .inner
        .seal_drive_epoch(
            &f.parts.session_id,
            &lash_core::store::AdmissionId::new("a-later-drive#0"),
            sealed.epoch,
            &lash_core::store::RootStartNonce::new("a-later-drive"),
            Some(&lash_core::store::RootHold {
                root: f.turn_id.clone(),
                executor: RootExecutor::Root,
            }),
        )
        .await
        .expect("the store answers the later drive's seal");
    assert_eq!(
        seal,
        lash_core::store::DriveEpochSeal::HeldByAnotherExecutor {
            root: f.turn_id.clone(),
            recorded: Box::new(acceptor.clone()),
        },
        "the store refuses the later drive's seal"
    );
    let request = crate::testing::store_fixtures::admit_root_request_for_test(
        &fence,
        &f.turn_id,
        unfinished.head.clone(),
    );
    assert_eq!(request.executor, RootExecutor::Root);
    match f.gate.inner.admit_root(&request).await {
        Err(crate::StoreError::RootHeldByAnotherExecutor {
            recorded,
            admitting,
            ..
        }) => {
            assert_eq!(*recorded, acceptor);
            assert_eq!(*admitting, RootExecutor::Root);
        }
        answer => panic!("the store refuses the later drive's admission: {answer:?}"),
    }
    assert_eq!(f.epoch().await, sealed, "the refused drive changed nothing");
    assert_eq!(f.unfinished().await, Some(unfinished));

    // The acceptor drives its root to its end.
    f.answer.notify_one();
    within("the acceptor's turn ends", accepting)
        .await
        .expect("the acceptor ran");
    let outcome = within("the acceptor answers", answered.recv())
        .await
        .expect("the acceptor reports")
        .expect("the acceptor commits its root");
    assert!(
        matches!(&outcome, crate::TurnOutcome::Finished(_)),
        "{outcome:?}"
    );
    f.assert_driven_once_to_its_end().await;
    assert_eq!(f.epoch().await, sealed, "one seal ran the root");
}
