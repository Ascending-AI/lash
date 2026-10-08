//! L6b's laws (FIG-5176): a turn's end and a session's close end their
//! scopes' `Until` processes through the batched cascade, across a crash at
//! each commit; a turn stop never hangs on a child; and a session's close is
//! durable deferred work of its own actor.
//!
//! Each law runs the production functions over a SQLite memory store set
//! whose durable store is layered with [`CutStore`]: a commit under a chosen
//! label is refused before it applies, as a crash before that commit would
//! leave the store, and the law then reclaims the actor on a fresh node
//! boot, as a successor does.
//!
//! The process half of these laws (registering a process, its activation,
//! its terminal) is L6's (FIG-5175) API, through the helpers at the bottom
//! of this file.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_core_execution::runtime::actor::process::{self, ProcessActivation};
use lash_core_execution::runtime::process::steps::{ProcessSteps, StepAdmission, StepRefusal};
use lash_durable::domain::{
    ExecKey, OwnerKey, ParkEventRow, ParkEventSeq, ProcessActorRow, RunRecordRow, ScopeKey,
    SessionCloseRow, SessionCloseStep, SnapshotRow, TurnEnd, TurnRow, WaitId, WaitRow,
};
use lash_durable::{
    ActorCommit, ActorKey, ActorSnapshot, ActorState, ActorTx, Claimed, CommitLabel, DurableError,
    DurableInstant, DurableReads, DurableSettings, DurableStore, FormatSet, HeartbeatOutcome,
    MailCommit, MailTx, NodeId, NodeLease, NodeSpec, Reaped, StoreFailure, StoreFailureKind,
};
use tokio_util::sync::CancellationToken;

use super::session_close::{
    SessionCloseExit, SessionCloseRequested, begin_session_close, request_session_close,
    run_session_close,
};
use super::turn_scope::{await_turn_children, continue_scope_ends, end_turn_scope};
use crate::store::{ObligationKey, ObligationKind, ObligationState, SessionLookup};
use crate::{
    ActorContext, AdmittedScope, Backend, ProcessId, ScopeId, SessionId, StoreSet, TurnId,
};

/// The format set the laws' session actors are written in.
const SESSION_FORMATS: &str = "law-session";

/// Work a law runs just before the first commit under its label applies:
/// another actor's commit that lands in between.
type BeforeCommit =
    Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

/// A durable store that refuses one chosen commit before it applies: a
/// crash just before that commit.
struct CutStore {
    inner: Arc<dyn DurableStore>,
    /// The label to cut and how many of its commits to let through first.
    cut: Mutex<Option<(CommitLabel, usize)>>,
    fired: AtomicBool,
    before: Mutex<Option<(CommitLabel, BeforeCommit)>>,
}

impl CutStore {
    fn over(inner: Arc<dyn DurableStore>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            cut: Mutex::new(None),
            fired: AtomicBool::new(false),
            before: Mutex::new(None),
        })
    }

    /// Run `work` just before the next commit under `label` applies.
    fn before(&self, label: CommitLabel, work: BeforeCommit) {
        *self.before.lock().expect("the before lock") = Some((label, work));
    }

    /// Cut the commit under `label` after `skip` of them went through.
    fn arm(&self, label: CommitLabel, skip: usize) {
        *self.cut.lock().expect("the cut lock") = Some((label, skip));
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl DurableReads for CutStore {
    async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
        self.inner.turn(session).await
    }

    async fn turn_namespaces(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
        self.inner.turn_namespaces(session, run).await
    }

    async fn turn_end(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<Option<TurnEnd>, DurableError> {
        self.inner.turn_end(session, run).await
    }

    async fn run_records(&self, owner: &OwnerKey) -> Result<Vec<RunRecordRow>, DurableError> {
        self.inner.run_records(owner).await
    }

    async fn snapshot(&self, exec: &ExecKey) -> Result<Option<SnapshotRow>, DurableError> {
        self.inner.snapshot(exec).await
    }

    async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
        self.inner.pending_waits(owner).await
    }

    async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
        self.inner.wait(id).await
    }

    async fn process(&self, process: &ProcessId) -> Result<Option<ProcessActorRow>, DurableError> {
        self.inner.process(process).await
    }

    async fn live_until_descendants(
        &self,
        scope: &ScopeKey,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.live_until_descendants(scope, limit).await
    }

    async fn until_children(
        &self,
        scope: &ScopeKey,
        after: Option<&ProcessId>,
        limit: usize,
    ) -> Result<Vec<ProcessId>, DurableError> {
        self.inner.until_children(scope, after, limit).await
    }

    async fn session_close(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionCloseRow>, DurableError> {
        self.inner.session_close(session).await
    }

    async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
        self.inner.ending_scopes(session).await
    }

    async fn session_mailbox(
        &self,
        session: &SessionId,
    ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
        self.inner.session_mailbox(session).await
    }

    async fn park_events(
        &self,
        after: Option<ParkEventSeq>,
        limit: usize,
    ) -> Result<Vec<ParkEventRow>, DurableError> {
        self.inner.park_events(after, limit).await
    }

    async fn prompt_snapshot(
        &self,
        call: &lash_durable::domain::PromptCallKey,
    ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
        self.inner.prompt_snapshot(call).await
    }

    async fn prompt_texts(
        &self,
        hashes: &[String],
    ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
        self.inner.prompt_texts(hashes).await
    }
}

#[async_trait::async_trait]
impl DurableStore for CutStore {
    async fn now(&self) -> Result<DurableInstant, DurableError> {
        self.inner.now().await
    }

    async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
        self.inner.register_node(spec).await
    }

    async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
        self.inner.heartbeat(node).await
    }

    async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
        self.inner.reap(reaper).await
    }

    async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
        self.inner.release_node(node).await
    }

    async fn claim(&self, node: &NodeLease, limit: usize) -> Result<Vec<Claimed>, DurableError> {
        self.inner.claim(node, limit).await
    }

    async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
        self.inner.mark_draining(node).await
    }

    async fn live_decodes(&self) -> Result<Vec<Vec<lash_durable::FormatSet>>, DurableError> {
        self.inner.live_decodes().await
    }

    async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
        self.inner.owned(node).await
    }

    async fn begin(
        &self,
        actor: &ActorKey,
        epoch: lash_durable::Epoch,
    ) -> Result<ActorTx, DurableError> {
        self.inner.begin(actor, epoch).await
    }

    async fn commit(&self, tx: ActorTx, label: CommitLabel) -> Result<ActorCommit, DurableError> {
        let before = {
            let mut before = self.before.lock().expect("the before lock");
            match before.take() {
                Some((before_label, work)) if before_label == label => Some(work),
                other => {
                    *before = other;
                    None
                }
            }
        };
        if let Some(work) = before {
            work().await;
        }
        let fire = {
            let mut cut = self.cut.lock().expect("the cut lock");
            match cut.as_mut() {
                Some((cut_label, 0)) if *cut_label == label => {
                    *cut = None;
                    true
                }
                Some((cut_label, skip)) if *cut_label == label => {
                    *skip -= 1;
                    false
                }
                _ => false,
            }
        };
        if fire {
            self.fired.store(true, Ordering::SeqCst);
            return Err(DurableError::Store(StoreFailure {
                kind: StoreFailureKind::Unavailable,
                message: format!("the law cut the process before `{label}` committed"),
            }));
        }
        self.inner.commit(tx, label).await
    }

    async fn commit_mail(
        &self,
        tx: MailTx,
        label: CommitLabel,
    ) -> Result<MailCommit, DurableError> {
        self.inner.commit_mail(tx, label).await
    }

    async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
        self.inner.actor(actor).await
    }
}

/// One session actor over a SQLite memory store set, owned by one node boot
/// at a time.
struct World {
    cut: Arc<CutStore>,
    backend: Backend,
    session: SessionId,
    actor: ActorKey,
    lease: Mutex<Option<NodeLease>>,
    boots: AtomicUsize,
}

impl World {
    async fn new(
        session: &str,
        settings: DurableSettings,
        mut engines: Vec<Arc<dyn crate::ProcessEngine>>,
    ) -> Self {
        engines.push(Arc::new(lash_core_execution::testing::HeldProcessEngine));
        let stores: Arc<dyn StoreSet> = crate::testing::sqlite_memory_store_set().await;
        let cut = CutStore::over(stores.durable_store());
        let layered = {
            let cut = Arc::clone(&cut);
            crate::testing::runtime_helpers::LayeredStores::over(stores)
                .map_durable_store(move |_| cut)
                .into_store_set()
        };
        let backend = Backend::assemble(lash_core_execution::BackendParts {
            formats: Vec::new(),
            stores: layered,
            settings,
            engines,
            providers: Arc::new(lash_core_execution::NoProjectionProviders),
        })
        .expect("the law's backend assembles");
        let session = SessionId::parse(session).expect("a session id");
        let actor = ActorKey::session(session.as_str()).expect("a session actor key");
        let mut create = MailTx::new();
        create.create_actor(actor.clone(), FormatSet::new(SESSION_FORMATS));
        backend
            .durable()
            .commit_mail(create, CommitLabel::new("law.create"))
            .await
            .expect("create the session actor");
        Self {
            cut,
            backend,
            session,
            actor,
            lease: Mutex::new(None),
            boots: AtomicUsize::new(0),
        }
    }

    /// Claim the session on a fresh node boot. The previous boot, if any,
    /// releases first, which bumps the epoch: what a successor finds after a
    /// crash.
    async fn claim(&self) -> ActorContext {
        let durable = self.backend.durable();
        let previous = self.lease.lock().expect("the lease lock").take();
        if let Some(previous) = previous {
            durable
                .release_node(&previous)
                .await
                .expect("release the previous boot");
        }
        let boot = self.boots.fetch_add(1, Ordering::SeqCst);
        let lease = durable
            .register_node(&NodeSpec {
                node: NodeId::new(format!("law-node-{boot}")),
                decodes: vec![FormatSet::new(SESSION_FORMATS)],
                ttl_millis: 600_000,
            })
            .await
            .expect("register the law's node");
        let claimed = durable
            .claim(&lease, 16)
            .await
            .expect("claim the session")
            .into_iter()
            .find(|claimed| claimed.actor == self.actor)
            .expect("the session actor is claimable");
        *self.lease.lock().expect("the lease lock") = Some(lease);
        ActorContext::new(
            self.backend.clone(),
            self.actor.clone(),
            claimed.epoch,
            AdmittedScope::runtime_operation("l6b-law"),
            CancellationToken::new(),
            Arc::new(lash_durable::NoProbe),
        )
    }

    async fn process(&self, process: &ProcessId) -> ProcessActorRow {
        self.backend
            .durable()
            .process(process)
            .await
            .expect("read the process")
            .expect("the process has its row")
    }
}

fn turn(run: &str) -> TurnId {
    TurnId::parse(run).expect("a turn id")
}

/// Cut at every session-close label in turn (and nowhere): the close resumes
/// at the step the cut interrupted. No session state is deleted while its
/// `Until` process is non-terminal, the session's waits are revoked and its
/// process terminal before the tombstone, and the session's `ArtifactCleanup`
/// is armed once and still owed at the end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_cut_at_any_step_resumes_there_and_deletes_nothing_while_a_process_lives() {
    let cuts = std::iter::once(None).chain(SessionCloseStep::ALL.map(|step| Some(step.label())));
    for cut in cuts {
        let world = World::new("closing", DurableSettings::default(), Vec::new()).await;
        world
            .backend
            .session_store_factory()
            .admit_session(
                &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
            )
            .await
            .expect("materialize the session");
        let child = start_process(
            &world.backend,
            ScopeId::Session(world.session.clone()),
            lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
        )
        .await;
        let mut cx = world.claim().await;

        assert_eq!(
            request_session_close(&world.backend, &world.session)
                .await
                .expect("request the close"),
            SessionCloseRequested::Requested
        );
        // What the session's mail drain commits for a close request (L3s).
        let mut tx = cx.begin().await.expect("begin");
        begin_session_close(&mut tx, &world.session);
        tx.ack_seen();
        cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
            .await
            .expect("drain the close request");
        if let Some(label) = cut {
            world.cut.arm(label, 0);
        }

        let mut child_ended = false;
        let mut closed = false;
        for _ in 0..16 {
            match run_session_close(&cx, &world.session).await {
                Ok(Some(SessionCloseExit::Closed)) => {
                    closed = true;
                    break;
                }
                Ok(Some(SessionCloseExit::Waiting)) => {
                    assert!(!child_ended, "{cut:?}: the close waits on an ended process");
                    assert_state_kept(&world, &child).await;
                    end_process(&world.backend, &child).await;
                    child_ended = true;
                }
                Ok(None) => panic!("{cut:?}: the closing session lost its close row"),
                Err(error) => {
                    assert!(
                        world.cut.fired(),
                        "{cut:?}: the close failed uncut: {error}"
                    );
                    if !child_ended {
                        assert_state_kept(&world, &child).await;
                    }
                }
            }
            cx = world.claim().await;
        }
        assert!(closed, "{cut:?}: the close did not finish");
        assert!(
            child_ended,
            "{cut:?}: the close never waited on its process"
        );
        assert_eq!(
            cut.is_some(),
            world.cut.fired(),
            "{cut:?}: the cut did not fire"
        );

        let durable = world.backend.durable();
        let row = durable
            .session_close(&world.session)
            .await
            .expect("read the close")
            .expect("the tombstone");
        assert!(row.is_tombstone(), "{cut:?}: the close ended at {row:?}");
        assert_eq!(
            durable
                .actor(&world.actor)
                .await
                .expect("read the actor")
                .map(|actor| actor.state),
            Some(ActorState::Terminal),
            "{cut:?}: the closed session's actor did not end"
        );
        assert!(
            durable
                .pending_waits(&world.actor)
                .await
                .expect("read the waits")
                .is_empty(),
            "{cut:?}: the closed session kept pending waits"
        );
        assert!(
            world.process(&child).await.terminal,
            "{cut:?}: the session closed over a live process"
        );
        assert!(
            matches!(
                world
                    .backend
                    .session_store_factory()
                    .lookup_session(&world.session)
                    .await
                    .expect("look the session up"),
                SessionLookup::Deleted
            ),
            "{cut:?}: the session's storage is not deleted"
        );
        let cleanup = ObligationKey::ArtifactCleanup {
            referrer: lash_core_execution::ArtifactReferrer::Session(world.session.clone()),
        };
        let standing = world
            .backend
            .obligation_ledger(ObligationKind::ArtifactCleanup)
            .standing(&cleanup.id())
            .await
            .expect("read the session's cleanup");
        assert!(
            standing.is_some_and(|standing| standing.state == ObligationState::Due),
            "{cut:?}: the session's ArtifactCleanup is not owed: {standing:?}"
        );
    }
}

/// The close's process ends between the close's read of the session's live
/// processes and the commit of the wait it pins on it: the process's
/// terminal resolved no wait, so the close resolves its own from the
/// registry, and the session, released as waiting, is woken and closes
/// (FIG-5176).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_whose_process_ends_before_its_wait_commits_still_closes() {
    let world = World::new("racing-close", DurableSettings::default(), Vec::new()).await;
    world
        .backend
        .session_store_factory()
        .admit_session(
            &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
        )
        .await
        .expect("materialize the session");
    let child = start_process(
        &world.backend,
        ScopeId::Session(world.session.clone()),
        lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
    )
    .await;
    let cx = world.claim().await;
    request_session_close(&world.backend, &world.session)
        .await
        .expect("request the close");
    let mut tx = cx.begin().await.expect("begin");
    begin_session_close(&mut tx, &world.session);
    tx.ack_seen();
    cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
        .await
        .expect("drain the close request");
    {
        let backend = world.backend.clone();
        let child = child.clone();
        world.cut.before(
            CommitLabel::WAIT_MINT,
            Box::new(move || Box::pin(async move { end_process(&backend, &child).await })),
        );
    }

    assert_eq!(
        run_session_close(&cx, &world.session)
            .await
            .expect("the close runs"),
        Some(SessionCloseExit::Waiting),
        "the close did not wait on its process"
    );
    assert!(
        world.process(&child).await.terminal,
        "the law's process did not end before the wait committed"
    );
    // What the session's activation commits on `Waiting`.
    let mut tx = cx.begin().await.expect("begin");
    tx.give_up(lash_durable::Release::Waiting {
        next_due: cx.next_due(),
    });
    let released = cx
        .commit(tx, CommitLabel::SESSION_RELEASE)
        .await
        .expect("release the session");
    assert_eq!(
        released.state,
        ActorState::Ready,
        "the session waits on a process that already ended, and nothing wakes it"
    );

    let cx = world.claim().await;
    assert_eq!(
        run_session_close(&cx, &world.session)
            .await
            .expect("the woken close runs"),
        Some(SessionCloseExit::Closed)
    );
    assert!(
        world
            .backend
            .durable()
            .session_close(&world.session)
            .await
            .expect("read the close")
            .is_some_and(|row| row.is_tombstone()),
        "the woken close did not reach its tombstone"
    );
}

/// A9 (FIG-5222): once a turn's scope has ended, or a session's close has
/// begun, no process registers under it: the ending transaction records the
/// scope's closure, and a start that commits after it is refused, whether
/// it lives `Until` the scope or only started there, as is a start in a turn
/// of the closed session that never ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_after_its_turn_ended_or_its_session_began_closing_is_refused() {
    let world = World::new("closed-scopes", DurableSettings::default(), Vec::new()).await;
    let cx = world.claim().await;
    let ended = turn("ended-turn");
    let mut tx = cx.begin().await.expect("begin");
    end_turn_scope(&cx, &mut tx, &world.session, &ended)
        .await
        .expect("end the turn's scope");
    cx.commit(tx, CommitLabel::TURN_COMMIT)
        .await
        .expect("commit the turn");
    assert_parent_ended(
        try_start_process(
            &world.backend,
            ScopeId::turn(world.session.clone(), ended.clone()),
            lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
        )
        .await,
        "a start under the ended turn",
    );
    assert_parent_ended(
        try_start_detached(
            &world.backend,
            ScopeId::turn(world.session.clone(), ended.clone()),
        )
        .await,
        "a detached start by the ended turn",
    );
    start_process(
        &world.backend,
        ScopeId::turn(world.session.clone(), turn("open-turn")),
        lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
    )
    .await;

    request_session_close(&world.backend, &world.session)
        .await
        .expect("request the close");
    let mut tx = cx.begin().await.expect("begin");
    begin_session_close(&mut tx, &world.session);
    tx.ack_seen();
    cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
        .await
        .expect("drain the close request");
    for (scope, what) in [
        (
            ScopeId::Session(world.session.clone()),
            "a start under the closing session",
        ),
        (
            ScopeId::turn(world.session.clone(), turn("never-ran")),
            "a start in a turn of the closing session that never ran",
        ),
    ] {
        assert_parent_ended(
            try_start_process(
                &world.backend,
                scope,
                lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
            )
            .await,
            what,
        );
    }
    assert_parent_ended(
        try_start_detached(&world.backend, ScopeId::Session(world.session.clone())).await,
        "a detached start by the closing session",
    );
}

fn assert_parent_ended(started: Result<ProcessId, crate::PluginError>, what: &str) {
    assert!(
        matches!(started, Err(crate::PluginError::ParentEnded { .. })),
        "{what} was not refused as parent-ended: {started:?}"
    );
}

/// A8 (FIG-5222): process A lives `Until` the session and B `Until` A. A's
/// terminal committed before its cascade marked B, so B is live below a
/// terminal intermediate: the close waits on B before its `triggers` step,
/// and deletes nothing until B ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_grandchild_below_a_terminal_intermediate_blocks_the_session_close() {
    let world = World::new("grandchild-close", DurableSettings::default(), Vec::new()).await;
    world
        .backend
        .session_store_factory()
        .admit_session(
            &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
        )
        .await
        .expect("materialize the session");
    let held = lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND;
    let intermediate = start_process(
        &world.backend,
        ScopeId::Session(world.session.clone()),
        held,
    )
    .await;
    let grandchild =
        start_process(&world.backend, ScopeId::process(intermediate.clone()), held).await;
    end_process(&world.backend, &intermediate).await;
    let cx = world.claim().await;
    assert_closes_only_after(&world, &cx, &grandchild).await;
}

/// A8 (FIG-5222): a turn's `Until` child that the turn's end marked for
/// cancel is still in its grace when the session closes: the close waits on
/// it, though it lives `Until` the turn and not the session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_child_still_in_its_grace_blocks_the_session_close() {
    let world = World::new("grace-close", DurableSettings::default(), Vec::new()).await;
    world
        .backend
        .session_store_factory()
        .admit_session(
            &lash_core_store::testing::store_fixtures::root_session_request(&world.session),
        )
        .await
        .expect("materialize the session");
    let run = turn("graced-turn");
    let child = start_process(
        &world.backend,
        ScopeId::turn(world.session.clone(), run.clone()),
        lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
    )
    .await;
    let cx = world.claim().await;
    let mut tx = cx.begin().await.expect("begin");
    end_turn_scope(&cx, &mut tx, &world.session, &run)
        .await
        .expect("end the turn's scope");
    cx.commit(tx, CommitLabel::TURN_COMMIT)
        .await
        .expect("commit the turn");
    assert!(
        world.process(&child).await.cancel_requested_at.is_some(),
        "the turn's end did not mark its child"
    );
    assert_closes_only_after(&world, &cx, &child).await;
}

/// Close the session on `cx` while `live` lives: the close stops before its
/// `triggers` step with the session's storage kept, and finishes once
/// `live` has ended. A later claim of the session's node would take `live`
/// too once its cancel is pending, so the close runs on the caller's claim.
async fn assert_closes_only_after(world: &World, cx: &ActorContext, live: &ProcessId) {
    request_session_close(&world.backend, &world.session)
        .await
        .expect("request the close");
    let mut tx = cx.begin().await.expect("begin");
    begin_session_close(&mut tx, &world.session);
    tx.ack_seen();
    cx.commit(tx, CommitLabel::SESSION_CLOSE_BEGIN)
        .await
        .expect("drain the close request");
    assert_eq!(
        run_session_close(cx, &world.session)
            .await
            .expect("the close runs"),
        Some(SessionCloseExit::Waiting),
        "the session closed over its live process {live}"
    );
    assert_state_kept(world, live).await;
    assert_eq!(
        world
            .backend
            .durable()
            .session_close(&world.session)
            .await
            .expect("read the close")
            .and_then(|row| row.done),
        Some(SessionCloseStep::EndScope),
        "the close went past its end_scope step while {live} lives"
    );

    end_process(&world.backend, live).await;
    let cx = world.claim().await;
    assert_eq!(
        run_session_close(&cx, &world.session)
            .await
            .expect("the woken close runs"),
        Some(SessionCloseExit::Closed),
        "the close did not finish once {live} ended"
    );
}

/// A8 (FIG-5222): a turn's child ended before its cascade marked the child's
/// own `Until` child: the turn's stop still awaits that live grandchild.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_grandchild_below_a_terminal_intermediate_blocks_a_turn_stop() {
    let world = World::new("grandchild-stop", DurableSettings::default(), Vec::new()).await;
    let held = lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND;
    let run = turn("stopping-turn");
    let intermediate = start_process(
        &world.backend,
        ScopeId::turn(world.session.clone(), run.clone()),
        held,
    )
    .await;
    let grandchild =
        start_process(&world.backend, ScopeId::process(intermediate.clone()), held).await;
    end_process(&world.backend, &intermediate).await;
    let cx = world.claim().await;
    let mut tx = cx.begin().await.expect("begin");
    end_turn_scope(&cx, &mut tx, &world.session, &run)
        .await
        .expect("end the turn's scope");
    cx.commit(tx, CommitLabel::TURN_CANCEL)
        .await
        .expect("cancel the turn");
    let stop = await_turn_children(&cx, &world.session, &run, Duration::from_millis(300), 16)
        .await
        .expect("the turn's stop");
    assert_eq!(
        stop.may_still_be_running,
        vec![grandchild],
        "the turn's stop did not await its live grandchild: {stop:?}"
    );
}

/// While a process of the session lives, nothing of the session is deleted.
async fn assert_state_kept(world: &World, child: &ProcessId) {
    assert!(
        !world.process(child).await.terminal,
        "the law's process ended on its own"
    );
    assert!(
        matches!(
            world
                .backend
                .session_store_factory()
                .lookup_session(&world.session)
                .await
                .expect("look the session up"),
            SessionLookup::Live(_)
        ),
        "the session's storage was deleted while its process lived"
    );
}

/// With more `Until(turn)` children than one batch, the turn's end marks the
/// first batch in its own commit and the session marks the rest, one
/// `cascade.batch` commit each; a cut at any of those commits is resumed by
/// the next claim, and every child ends up cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_cascade_larger_than_a_batch_ends_every_child_across_a_cut_at_each_batch() {
    let settings = DurableSettings {
        cascade_batch: 2,
        ..DurableSettings::default()
    };
    // Five children, two per batch: the turn's commit marks two, then two
    // `cascade.batch` commits mark two and one.
    for cut_batch in 0..2 {
        let world = World::new("cascading", settings, Vec::new()).await;
        let run = turn("cascading-turn");
        let scope = ScopeKey::Turn(world.session.clone(), run.clone());
        let mut children = Vec::new();
        for _ in 0..5 {
            children.push(
                start_process(
                    &world.backend,
                    ScopeId::turn(world.session.clone(), run.clone()),
                    lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
                )
                .await,
            );
        }
        let mut cx = world.claim().await;

        // The turn's commit (L3's `turn.commit`) ends its scope.
        let mut tx = cx.begin().await.expect("begin");
        end_turn_scope(&cx, &mut tx, &world.session, &run)
            .await
            .expect("end the turn's scope");
        cx.commit(tx, CommitLabel::TURN_COMMIT)
            .await
            .expect("commit the turn");
        assert_eq!(
            world
                .backend
                .durable()
                .ending_scopes(&world.session)
                .await
                .expect("read the ending scopes"),
            vec![scope.clone()],
            "the unfinished cascade is not recorded"
        );

        world.cut.arm(CommitLabel::CASCADE_BATCH, cut_batch);
        assert!(
            continue_scope_ends(&cx, &world.session).await.is_err(),
            "batch {cut_batch}: the cut cascade answered done"
        );
        cx = world.claim().await;
        continue_scope_ends(&cx, &world.session)
            .await
            .expect("the successor finishes the cascade");

        for child in &children {
            assert!(
                world.process(child).await.cancel_requested_at.is_some(),
                "batch {cut_batch}: {child} was never marked for cancel"
            );
        }
        assert!(
            world
                .backend
                .durable()
                .ending_scopes(&world.session)
                .await
                .expect("read the ending scopes")
                .is_empty(),
            "batch {cut_batch}: the finished cascade is still recorded"
        );
    }
}

/// C1 (FIG-5160, G1): child A of turn T is parked, and A has an `Until(A)`
/// child B. Ending T ends A as `Cancelled` without running A's engine, B
/// receives `ParentEnded` and ends, and T's stop completes with A and B ended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ending_a_turn_ends_its_parked_child_engine_free_and_the_grandchild_by_parent_end() {
    turn_end_ends_its_child_and_grandchild(ChildAt::Parked).await;
}

/// C1 with A waiting (on a durable sleep) instead of parked: A receives its
/// cancel once and ends, and so does B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ending_a_turn_ends_its_waiting_child_and_the_grandchild_by_parent_end() {
    turn_end_ends_its_child_and_grandchild(ChildAt::Waiting).await;
}

/// Where the law's child A is when its turn ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChildAt {
    /// Parked: its engine refused its start.
    Parked,
    /// Waiting on a durable sleep.
    Waiting,
}

async fn turn_end_ends_its_child_and_grandchild(at: ChildAt) {
    let engine = Arc::new(LawEngine::new(match at {
        ChildAt::Parked => LawEngineMode::RefuseStart,
        ChildAt::Waiting => LawEngineMode::SleepUntilCancelled,
    }));
    let world = World::new("turn-end", DurableSettings::default(), vec![engine.clone()]).await;
    let run = turn("ending-turn");
    let child = start_process(
        &world.backend,
        ScopeId::turn(world.session.clone(), run.clone()),
        LAW_ENGINE_KIND,
    )
    .await;
    let grandchild = start_process(
        &world.backend,
        ScopeId::process(child.clone()),
        lash_core_execution::testing::HELD_PROCESS_ENGINE_KIND,
    )
    .await;
    let cx = world.claim().await;

    let processes = ProcessNode::serve(&world.backend);
    wait_until("the child settles at its start", || async {
        engine.advances() >= 1 && !owned(&world, &child).await
    })
    .await;
    let settled = engine.advances();

    // The turn's cancel (L3's `turn.cancel`) ends its scope.
    let mut tx = cx.begin().await.expect("begin");
    end_turn_scope(&cx, &mut tx, &world.session, &run)
        .await
        .expect("end the turn's scope");
    cx.commit(tx, CommitLabel::TURN_CANCEL)
        .await
        .expect("cancel the turn");
    continue_scope_ends(&cx, &world.session)
        .await
        .expect("finish the turn's cascade");
    let stop = await_turn_children(&cx, &world.session, &run, Duration::from_secs(5), 16)
        .await
        .expect("the turn's stop waits for its children");
    assert!(
        stop.may_still_be_running.is_empty(),
        "{at:?}: the turn's stop left {:?} running",
        stop.may_still_be_running
    );
    // The stop awaits the turn's live `Until` descendants: the child and,
    // through it, the grandchild, which ends by its parent's end.
    let mut ended: Vec<ProcessId> = stop
        .ended
        .iter()
        .map(|(process, _)| process.clone())
        .collect();
    ended.sort();
    let mut expected = vec![child.clone(), grandchild.clone()];
    expected.sort();
    assert_eq!(
        ended, expected,
        "{at:?}: the turn's stop did not see its child and grandchild end"
    );
    assert!(
        world.process(&grandchild).await.terminal,
        "{at:?}: the grandchild lives"
    );
    processes.stop().await;

    assert!(
        world.process(&child).await.terminal,
        "{at:?}: the child lives"
    );
    match at {
        ChildAt::Parked => assert_eq!(
            engine.advances(),
            settled,
            "the parked child ran engine code after its turn ended"
        ),
        ChildAt::Waiting => assert!(
            engine.advances() <= settled + 1,
            "the waiting child received its cancel more than once"
        ),
    }
}

/// G1b: a turn stop that awaits a parked child completes within its grace
/// and sees the child end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_stop_awaiting_a_parked_child_completes_within_its_grace() {
    let stop = turn_stop_with_child(LawEngineMode::RefuseStart, Duration::from_secs(5)).await;
    assert!(stop.0.may_still_be_running.is_empty(), "{stop:?}");
    assert_eq!(
        stop.0.ended.len(),
        1,
        "the parked child did not end: {stop:?}"
    );
}

/// G1b: a turn stop whose child is running and ignores its cancel completes
/// at its grace and reports the child as possibly still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_stop_reports_a_running_child_that_ignores_its_token_and_completes() {
    let grace = Duration::from_millis(500);
    let (stop, elapsed) = turn_stop_with_child(LawEngineMode::RunForever, grace).await;
    assert!(stop.ended.is_empty(), "{stop:?}");
    assert_eq!(stop.may_still_be_running.len(), 1, "{stop:?}");
    assert!(
        elapsed >= grace,
        "the stop gave up before its grace: {elapsed:?}"
    );
}

/// Start one `Until(turn)` child in `mode`, end the turn, and time the
/// turn's stop over `grace`.
async fn turn_stop_with_child(
    mode: LawEngineMode,
    grace: Duration,
) -> (super::turn_scope::TurnChildrenStop, Duration) {
    let engine = Arc::new(LawEngine::new(mode));
    let world = World::new(
        "turn-stop",
        DurableSettings::default(),
        vec![engine.clone()],
    )
    .await;
    let run = turn("stopping-turn");
    start_process(
        &world.backend,
        ScopeId::turn(world.session.clone(), run.clone()),
        LAW_ENGINE_KIND,
    )
    .await;
    let cx = world.claim().await;
    let processes = ProcessNode::serve(&world.backend);
    wait_until("the child starts", || async { engine.advances() >= 1 }).await;

    let mut tx = cx.begin().await.expect("begin");
    end_turn_scope(&cx, &mut tx, &world.session, &run)
        .await
        .expect("end the turn's scope");
    cx.commit(tx, CommitLabel::TURN_CANCEL)
        .await
        .expect("cancel the turn");
    let started = Instant::now();
    let stop = await_turn_children(&cx, &world.session, &run, grace, 16)
        .await
        .expect("the turn's stop");
    let elapsed = started.elapsed();
    assert!(
        elapsed < grace + Duration::from_secs(2),
        "the turn's stop outlived its grace: {elapsed:?}"
    );
    processes.stop().await;
    (stop, elapsed)
}

/// Whether `process`'s actor is owned by a node now.
async fn owned(world: &World, process: &ProcessId) -> bool {
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    world
        .backend
        .durable()
        .actor(&actor)
        .await
        .expect("read the process actor")
        .is_some_and(|actor| actor.state == ActorState::Owned)
}

async fn wait_until<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition().await {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The kind of [`LawEngine`].
const LAW_ENGINE_KIND: &str = "l6b-law";

/// How [`LawEngine`] answers its start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LawEngineMode {
    /// Refuse it: the process parks.
    RefuseStart,
    /// Sleep until cancelled, then end.
    SleepUntilCancelled,
    /// Run one step that never finishes, and ignore its cancel within a
    /// cancel grace longer than the turn stop's.
    RunForever,
}

/// An engine that counts every `advance` it answers.
struct LawEngine {
    mode: LawEngineMode,
    advances: AtomicUsize,
}

impl LawEngine {
    fn new(mode: LawEngineMode) -> Self {
        Self {
            mode,
            advances: AtomicUsize::new(0),
        }
    }

    fn advances(&self) -> usize {
        self.advances.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::ProcessEngine for LawEngine {
    fn kind(&self) -> &'static str {
        LAW_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> Result<Vec<crate::ArtifactName>, crate::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &crate::ResolvedArtifactCleanup,
    ) -> Result<(), crate::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &crate::ReferrerClaim,
        artifact_ref: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(format!(
            "the law's engine stores no artifact `{artifact_ref}`"
        )))
    }

    fn state_format(&self) -> crate::EngineStateFormat {
        crate::EngineStateFormat {
            kind: LAW_ENGINE_KIND.to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> Duration {
        match self.mode {
            // Longer than any turn stop's grace in these laws.
            LawEngineMode::RunForever => Duration::from_secs(60),
            LawEngineMode::RefuseStart | LawEngineMode::SleepUntilCancelled => {
                Duration::from_millis(100)
            }
        }
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<crate::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &crate::ProcessExecutionEnvSpec,
    ) -> Result<Option<serde_json::Value>, crate::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: crate::EngineState,
        event: crate::EngineEvent,
    ) -> Result<(crate::EngineState, crate::EngineAction), crate::ProcessInfraError> {
        self.advances.fetch_add(1, Ordering::SeqCst);
        match (self.mode, event) {
            (LawEngineMode::RefuseStart, _) => Err(crate::ProcessInfraError::new(
                crate::PluginError::Session("the law's engine refuses to start".into()),
            )),
            (LawEngineMode::SleepUntilCancelled, crate::EngineEvent::Started { .. }) => Ok((
                state,
                crate::EngineAction::Sleep {
                    until: DurableInstant(i64::MAX / 2),
                },
            )),
            (LawEngineMode::RunForever, crate::EngineEvent::Started { .. }) => Ok((
                state,
                crate::EngineAction::Steps(vec![crate::StepRequest::Tool {
                    language_execution: None,
                    step: crate::StepName("forever".into()),
                    tool: lash_sansio::ToolId::new(HANGING_TOOL),
                    input: serde_json::Value::Null,
                    site: None,
                }]),
            )),
            // It ignores its cancel: only lash's forced terminal, at its
            // grace, ends it.
            (LawEngineMode::RunForever, crate::EngineEvent::Cancelled { .. }) => {
                Ok((state, crate::EngineAction::Idle))
            }
            (_, crate::EngineEvent::Cancelled { origin, .. }) => Ok((
                state,
                crate::EngineAction::Terminal(process::cancelled(origin, false)),
            )),
            (_, event) => Err(crate::ProcessInfraError::new(crate::PluginError::Session(
                format!("the law's engine did not expect {event:?}"),
            ))),
        }
    }

    async fn resolve(
        &self,
        _reference: &crate::ProcessDefinitionRef,
    ) -> Result<crate::ProcessDefinitionResolution, crate::ProcessDefinitionRefusal> {
        Ok(crate::ProcessDefinitionResolution::new(
            crate::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

/// The step tool of [`LawEngineMode::RunForever`]: [`HangingSteps`] runs it
/// and ignores its cancel.
const HANGING_TOOL: &str = "l6b-law-hang";

/// A node serving the law's process actors with L6's process activation.
struct ProcessNode {
    stop: CancellationToken,
    served: tokio::task::JoinHandle<()>,
}

impl ProcessNode {
    fn serve(backend: &Backend) -> Self {
        let runner = lash_durable::runner::Runner::new(
            Arc::clone(backend.durable()),
            backend.clock(),
            lash_durable::runner::RunnerConfig::new(
                NodeId::new("law-process-node"),
                backend.formats().decodes(),
                backend.config(),
            ),
            Arc::new(ProcessActivation::new(
                backend.clone(),
                Arc::new(HangingSteps),
                Arc::new(lash_durable::NoProbe),
            )),
        )
        .with_hints(backend.hints().clone());
        let stop = CancellationToken::new();
        let until = stop.clone();
        let served = crate::task::spawn(async move {
            runner
                .run(until.cancelled_owned())
                .await
                .expect("the process node serves");
        });
        Self { stop, served }
    }

    async fn stop(self) {
        self.stop.cancel();
        self.served.await.expect("the process node stops");
    }
}

/// Every law step: a `Once` tool whose body never returns and ignores its
/// cancel token.
struct HangingSteps;

#[async_trait::async_trait]
impl ProcessSteps for HangingSteps {
    async fn admit(
        &self,
        _process: &crate::ProcessRecord,
        _step: &crate::StepRequest,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        let long = Duration::from_secs(60);
        Ok(StepAdmission {
            wait: None,
            policy: lash_sansio::ExecutionPolicy::Once,
            limit: lash_sansio::ExecutionLimit::starting_at(now_ms, long, long),
        })
    }

    /// Never asked: no step of these parks.
    fn resolved(
        &self,
        _process: &crate::ProcessRecord,
        _step: &crate::StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
        _parked: &lash_core_execution::runtime::actor::round::Material<
            lash_core_store::tool_run::CompletionSource,
        >,
        _resolution: lash_core_execution::runtime::actor::waits::Resolution,
    ) -> lash_core_execution::runtime::actor::round::SettledOutput {
        lash_core_execution::runtime::actor::round::SettledOutput::Interrupted
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<lash_core_execution::runtime::process::StepRuntime>,
        _process: &crate::ProcessRecord,
        _step: &crate::StepRequest,
        _execution: &lash_core_execution::runtime::actor::round::AdmittedExecution,
    ) -> lash_core_execution::runtime::actor::round::MemberBody {
        lash_core_execution::runtime::actor::round::member_body({
            Box::new(|_token| Box::pin(std::future::pending()))
        })
    }
}

/// Register a process with an engine of `kind` that lives `Until` `scope`,
/// as a start admitted in that scope does: its registry row and its actor,
/// ready.
async fn start_process(backend: &Backend, scope: ScopeId, kind: &str) -> ProcessId {
    try_start_process(backend, scope, kind)
        .await
        .expect("register the law's process")
}

/// [`start_process`], answering the registry's refusal.
async fn try_start_process(
    backend: &Backend,
    scope: ScopeId,
    kind: &str,
) -> Result<ProcessId, crate::PluginError> {
    lash_core_execution::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let mut registration = crate::ProcessRegistration::new(
        crate::ProcessInput::Engine {
            kind: kind.to_owned(),
            payload: serde_json::Value::Null,
        },
        crate::ProcessProvenance::host(),
        crate::LifetimeDecision::Until {
            scope: scope.clone(),
            grant: crate::ScopeGrant::Ancestor,
        },
    )
    .with_execution_env_ref(Some(
        lash_core_execution::testing::process_execution_env_fixture_ref(),
    ));
    registration.ancestry = crate::Ancestry::from_scopes([scope]);
    backend
        .process_registry()
        .register_process(registration)
        .await
        .map(|record| record.id)
}

/// Register a detached process with the held engine that `starter`
/// started: it owes `starter` nothing, but names it as its starter.
async fn try_start_detached(
    backend: &Backend,
    starter: ScopeId,
) -> Result<ProcessId, crate::PluginError> {
    lash_core_execution::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let mut registration = lash_core_execution::testing::held_engine_registration(
        serde_json::Value::Null,
        crate::ProcessProvenance::host(),
        crate::Lifetime::Detached,
    );
    registration.ancestry = crate::Ancestry::from_scopes([starter]);
    backend
        .process_registry()
        .register_process(registration)
        .await
        .map(|record| record.id)
}

/// End `process` as its own activation does: claim its actor and commit its
/// terminal transaction, cancelled by its parent's end.
async fn end_process(backend: &Backend, process: &ProcessId) {
    let durable = backend.durable();
    let lease = durable
        .register_node(&NodeSpec {
            node: NodeId::new(format!("law-terminal-{process}")),
            decodes: backend.formats().decodes(),
            ttl_millis: 600_000,
        })
        .await
        .expect("register the terminal's node");
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let claimed = durable
        .claim(&lease, 16)
        .await
        .expect("claim the process")
        .into_iter()
        .find(|claimed| claimed.actor == actor)
        .expect("the process actor is claimable");
    let cx = ActorContext::new(
        backend.clone(),
        actor,
        claimed.epoch,
        AdmittedScope::runtime_operation("l6b-law"),
        CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    );
    let mut tx = cx.begin().await.expect("begin");
    process::record_terminal(
        &mut tx,
        process,
        &process::cancelled(crate::CancelOrigin::ParentEnded, false),
    )
    .expect("record the terminal");
    cx.commit(tx, CommitLabel::PROCESS_TERMINAL)
        .await
        .expect("commit the terminal");
    durable
        .release_node(&lease)
        .await
        .expect("release the terminal's node");
}

/// ADR 0049: a session's actor is created by its first work, so a session
/// created but never sent anything has metadata and no actor. Its metadata
/// makes the id one session lifetime, so its close request is its first
/// work: the request creates the actor with the close as its mail. An id the
/// catalog never held stays a no-op.
#[tokio::test]
async fn a_created_session_with_no_work_is_closed_by_its_close_request() {
    let world = World::new("with-work", DurableSettings::default(), Vec::new()).await;
    let idle = SessionId::parse("created-idle").expect("a session id");
    world
        .backend
        .session_store_factory()
        .admit_session(&lash_core_store::testing::store_fixtures::root_session_request(&idle))
        .await
        .expect("materialize the session");
    let idle_actor = ActorKey::session(idle.as_str()).expect("a session actor key");
    assert!(
        world
            .backend
            .durable()
            .actor(&idle_actor)
            .await
            .expect("read the actor")
            .is_none(),
        "a created session has no actor before its first work"
    );

    assert_eq!(
        request_session_close(&world.backend, &idle)
            .await
            .expect("request the close"),
        SessionCloseRequested::Requested
    );
    assert!(
        world
            .backend
            .durable()
            .actor(&idle_actor)
            .await
            .expect("read the actor")
            .is_some(),
        "the close request created the session's actor"
    );
    assert_eq!(
        request_session_close(&world.backend, &idle)
            .await
            .expect("repeat the close"),
        SessionCloseRequested::Requested,
        "a repeated request appends to the actor the first one created"
    );

    let never = SessionId::parse("never-created").expect("a session id");
    assert_eq!(
        request_session_close(&world.backend, &never)
            .await
            .expect("request the close"),
        SessionCloseRequested::Absent
    );
    assert!(
        world
            .backend
            .durable()
            .actor(&ActorKey::session(never.as_str()).expect("a session actor key"))
            .await
            .expect("read the actor")
            .is_none(),
        "closing an id the catalog never held creates nothing"
    );
}
