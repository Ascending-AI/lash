//! The crash-point injection facility: the levers a case pulls to kill a
//! deployment at a named seam boundary.
//!
//! Two kinds of crash site exist:
//!
//! - **Engine sites** cut an invocation's journal. They are
//!   [`lash_restate_test::CrashRule`]s on the server double; the double drops
//!   the attempt before it stores the frame and replays the invocation. The
//!   world's crash listener records the [`Trip`] and takes the deployment
//!   down at once, so the replay waits for the restarted deployment.
//! - **Host sites** cut the host process between two writes. They are
//!   [`HostSite`] arms on decorators over the ports a core reaches the engine
//!   and the stores through: the session-work engine and its control half,
//!   the session store factory and the process-work port. An armed
//!   [`ArmEffect::Crash`] records the trip and never returns, as a dead
//!   process never does; the world then aborts every task the deployment ran.
//!
//! [`DriverProxy`] is the deployment slot the engine's handlers reach the
//! session driver through. While no deployment is up, a handler's call waits
//! in it, as an invoker's retries against a restarting deployment would,
//! without spending the handler's retry budget.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core::{SessionDriver, SessionId, SessionStoreFactory, SessionWorkEngine};
use tokio::sync::watch;

/// A crash that fired: where, and when on the server's virtual clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tripped {
    pub site: String,
    pub at_ms: u64,
}

/// The first crash of a case. Later fires are recorded as counted repeats.
pub struct Trip {
    first: Mutex<Option<Tripped>>,
    fires: std::sync::atomic::AtomicUsize,
    clock: Arc<dyn lash_core::Clock>,
    notify: tokio::sync::Notify,
}

impl std::fmt::Debug for Trip {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Trip")
            .field("first", &*self.first.lock_recover())
            .finish_non_exhaustive()
    }
}

impl Trip {
    #[must_use]
    pub fn new(clock: Arc<dyn lash_core::Clock>) -> Self {
        Self {
            first: Mutex::new(None),
            fires: std::sync::atomic::AtomicUsize::new(0),
            clock,
            notify: tokio::sync::Notify::new(),
        }
    }

    /// Record a crash at `site`.
    pub fn fire(&self, site: impl Into<String>) {
        self.fires.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut first = self.first.lock_recover();
        if first.is_none() {
            *first = Some(Tripped {
                site: site.into(),
                at_ms: self.clock.timestamp_ms(),
            });
        }
        drop(first);
        self.notify.notify_waiters();
    }

    #[must_use]
    pub fn tripped(&self) -> Option<Tripped> {
        self.first.lock_recover().clone()
    }

    #[must_use]
    pub fn fires(&self) -> usize {
        self.fires.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Wait until more than `seen` crashes have fired.
    pub async fn fired_beyond(&self, seen: usize) {
        loop {
            let notified = self.notify.notified();
            if self.fires() > seen {
                return;
            }
            notified.await;
        }
    }

    /// Wait until a crash fired or `within` of wall time passed.
    pub async fn wait(&self, within: std::time::Duration) -> Option<Tripped> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let notified = self.notify.notified();
            if let Some(tripped) = self.tripped() {
                return Some(tripped);
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.tripped();
            }
        }
    }
}

/// A host-side crash site: a point between two writes of a seam where the
/// host process can die.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HostSite {
    /// The host's drive ask after an acceptance committed: the ingress
    /// obligation's awaited `SessionWorkEngine::request_drive` (the host dies
    /// there) or a fire-and-forget `schedule_drive` (the ask never leaves).
    DriveAsk,
    /// `SessionControlEngine::release_root`, before the engine sees it.
    ReleaseRootBefore,
    /// `SessionControlEngine::release_root`, after the engine answered.
    ReleaseRootAfter,
    /// `ControlIntentStore::acknowledge_intent`, before it writes.
    AcknowledgeIntentBefore,
    /// `SessionStoreFactory::delete_session`, before the physical delete.
    DeleteStorageBefore,
    /// `ProcessWorkSubstrate::deliver_cancel`, before the engine sees it.
    DeliverCancelBefore,
    /// `ProcessWorkSubstrate::deliver_cancel`, after the engine answered.
    DeliverCancelAfter,
}

/// What an armed site does when a call reaches it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArmEffect {
    /// The host dies here: the trip fires and the call never returns. A
    /// swallowed drive ask returns at once instead, since the ask is
    /// fire-and-forget and the host's next step is the crash.
    Crash,
    /// The engine refuses the delivery for good.
    Refuse,
    /// The engine fails the delivery retryably.
    FailRetryable,
}

#[derive(Clone, Debug)]
struct Arm {
    site: HostSite,
    effect: ArmEffect,
    /// Fires left; `None` fires on every call.
    remaining: Option<u32>,
    /// Only calls whose detail contains this match.
    matching: Option<String>,
}

/// The armed host sites of one world, shared by every decorator.
#[derive(Debug)]
pub struct HostFaults {
    arms: Mutex<Vec<Arm>>,
    trip: Arc<Trip>,
}

impl HostFaults {
    #[must_use]
    pub fn new(trip: Arc<Trip>) -> Self {
        Self {
            arms: Mutex::new(Vec::new()),
            trip,
        }
    }

    /// Crash the host the next time a call reaches `site`.
    pub fn crash_once(&self, site: HostSite) {
        self.arm(site, ArmEffect::Crash, Some(1), None);
    }

    /// Crash the host the next time a call whose detail contains `matching`
    /// reaches `site`.
    pub fn crash_once_matching(&self, site: HostSite, matching: impl Into<String>) {
        self.arm(site, ArmEffect::Crash, Some(1), Some(matching.into()));
    }

    /// Apply `effect` to every call that reaches `site`.
    pub fn always(&self, site: HostSite, effect: ArmEffect) {
        self.arm(site, effect, None, None);
    }

    /// Apply `effect` to every call whose detail contains `matching`.
    pub fn always_matching(&self, site: HostSite, effect: ArmEffect, matching: impl Into<String>) {
        self.arm(site, effect, None, Some(matching.into()));
    }

    fn arm(
        &self,
        site: HostSite,
        effect: ArmEffect,
        remaining: Option<u32>,
        matching: Option<String>,
    ) {
        self.arms.lock_recover().push(Arm {
            site,
            effect,
            remaining,
            matching,
        });
    }

    /// Disarm every site.
    pub fn clear(&self) {
        self.arms.lock_recover().clear();
    }

    /// The crash sites armed but never reached: a cell whose armed crash
    /// nothing took never reached its crash point, whatever the trip says.
    #[must_use]
    pub fn unfired_crashes(&self) -> Vec<HostSite> {
        self.arms
            .lock_recover()
            .iter()
            .filter(|arm| {
                arm.effect == ArmEffect::Crash
                    && arm.remaining.is_some_and(|remaining| remaining > 0)
            })
            .map(|arm| arm.site)
            .collect()
    }

    /// The effect armed for a call at `site` with `detail`, consuming a
    /// one-shot arm. A crash fires the trip here.
    fn take(&self, site: HostSite, detail: &str) -> Option<ArmEffect> {
        let mut arms = self.arms.lock_recover();
        let index = arms.iter().position(|arm| {
            arm.site == site
                && arm.remaining != Some(0)
                && arm
                    .matching
                    .as_deref()
                    .is_none_or(|matching| detail.contains(matching))
        })?;
        let arm = &mut arms[index];
        if let Some(remaining) = &mut arm.remaining {
            *remaining -= 1;
        }
        let effect = arm.effect;
        drop(arms);
        if effect == ArmEffect::Crash {
            self.trip.fire(format!("host:{site:?}:{detail}"));
        }
        Some(effect)
    }
}

/// A dead host's call: it never returns.
async fn die<T>() -> T {
    std::future::pending::<T>().await
}

// ---------------------------------------------------------------------------
// The deployment slot
// ---------------------------------------------------------------------------

/// The session driver the engine's handlers reach: the live deployment's, or
/// a wait for the next one while none is up.
pub struct DriverProxy {
    current: Mutex<Option<Arc<dyn SessionDriver>>>,
    up: watch::Sender<u64>,
}

impl std::fmt::Debug for DriverProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DriverProxy")
            .field("up", &self.current.lock_recover().is_some())
            .finish()
    }
}

impl Default for DriverProxy {
    fn default() -> Self {
        Self {
            current: Mutex::new(None),
            up: watch::channel(0).0,
        }
    }
}

impl DriverProxy {
    /// Serve `driver` from now on.
    pub fn serve(&self, driver: Arc<dyn SessionDriver>) {
        *self.current.lock_recover() = Some(driver);
        self.up.send_modify(|generation| *generation += 1);
    }

    /// No deployment is up: handler calls wait.
    pub fn down(&self) {
        *self.current.lock_recover() = None;
        self.up.send_modify(|generation| *generation += 1);
    }

    /// The live deployment's driver, if one is up.
    #[must_use]
    pub fn current(&self) -> Option<Arc<dyn SessionDriver>> {
        self.current.lock_recover().clone()
    }

    async fn live(&self) -> Arc<dyn SessionDriver> {
        let mut changes = self.up.subscribe();
        loop {
            if let Some(driver) = self.current() {
                return driver;
            }
            if changes.changed().await.is_err() {
                return die().await;
            }
        }
    }
}

#[async_trait::async_trait]
impl SessionDriver for DriverProxy {
    fn hold_drive(&self, session: &lash_core::SessionId) -> lash_core::engine::DriveHold {
        self.current()
            .map_or_else(lash_core::engine::DriveHold::empty, |driver| {
                driver.hold_drive(session)
            })
    }

    async fn admit(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        request: &lash_core::engine::DriveRequest,
        ordinal: u32,
    ) -> Result<lash_core::engine::AdmitVerdict, lash_core::engine::DriveAbort> {
        self.live().await.admit(controller, request, ordinal).await
    }

    async fn run_root(
        &self,
        controller: lash_core::ScopedEffectController<'_>,
        admitted: lash_core::engine::Admitted,
    ) -> Result<lash_core::engine::RootOutcome, lash_core::engine::DriveAbort> {
        self.live().await.run_root(controller, admitted).await
    }
}

// ---------------------------------------------------------------------------
// Session work and its control half
// ---------------------------------------------------------------------------

/// The engine's session work as a deployment reaches it: every answer is the
/// engine's, a core's driver installs into the [`DriverProxy`], and the
/// drive ask and the control verbs cross [`HostFaults`].
pub struct CrashSessionWork {
    inner: Arc<dyn SessionWorkEngine>,
    proxy: Arc<DriverProxy>,
    faults: Arc<HostFaults>,
    /// The engine's installation of the proxy, installed once and kept for
    /// the world's life.
    installed: std::sync::OnceLock<Arc<dyn SessionDriver>>,
    /// Every drive request a waiter awaited, and every ask the engine
    /// accepted, in order.
    drives: Arc<DriveLog>,
}

/// The drive requests a deployment's session work saw: those a waiter
/// awaited, and those the engine accepted as asks.
#[derive(Debug, Default)]
pub struct DriveLog {
    awaited: std::sync::Mutex<Vec<String>>,
    asked: std::sync::Mutex<Vec<String>>,
}

impl DriveLog {
    /// Every drive request a waiter awaited, in order.
    #[must_use]
    pub fn awaited(&self) -> Vec<String> {
        self.awaited.lock_recover().clone()
    }

    /// Every drive ask the engine accepted, in order.
    #[must_use]
    pub fn asked(&self) -> Vec<String> {
        self.asked.lock_recover().clone()
    }
}

impl std::fmt::Debug for CrashSessionWork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CrashSessionWork")
            .finish_non_exhaustive()
    }
}

impl CrashSessionWork {
    #[must_use]
    pub fn new(
        inner: Arc<dyn SessionWorkEngine>,
        proxy: Arc<DriverProxy>,
        faults: Arc<HostFaults>,
        drives: Arc<DriveLog>,
    ) -> Self {
        Self {
            inner,
            proxy,
            faults,
            installed: std::sync::OnceLock::new(),
            drives,
        }
    }
}

#[async_trait::async_trait]
impl SessionWorkEngine for CrashSessionWork {
    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        Arc::new(CrashControl {
            inner: self.inner.control(),
            faults: Arc::clone(&self.faults),
        })
    }

    fn schedule_drive(&self, session: &SessionId, request: lash_core::engine::DriveRequestId) {
        let detail = format!("{session}/{}", request.as_str());
        if self.faults.take(HostSite::DriveAsk, &detail).is_some() {
            return;
        }
        self.inner.schedule_drive(session, request);
    }

    /// Every deployment's driver is served through the one proxy the engine
    /// holds; the core keeps its own driver alive.
    fn install_session_driver(&self, driver: Arc<dyn SessionDriver>) -> Arc<dyn SessionDriver> {
        self.proxy.serve(Arc::clone(&driver));
        self.installed.get_or_init(|| {
            self.inner
                .install_session_driver(Arc::clone(&self.proxy) as Arc<dyn SessionDriver>)
        });
        driver
    }

    async fn await_drive(
        &self,
        session: &SessionId,
        request: &lash_core::engine::DriveRequestId,
    ) -> Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        self.drives
            .awaited
            .lock_recover()
            .push(request.as_str().to_owned());
        self.inner.await_drive(session, request).await
    }

    /// The awaited drive ask an admitted row's ingress obligation delivers
    /// (ADR 0109 §3): a host that dies here never hears the engine's answer,
    /// so the claim its delivery took lapses and the relay retakes it.
    async fn request_drive(
        &self,
        session: &SessionId,
        request: lash_core::engine::DriveRequestId,
    ) -> Result<(), lash_core::engine::EngineRefusal> {
        let detail = format!("{session}/{}", request.as_str());
        match self.faults.take(HostSite::DriveAsk, &detail) {
            Some(ArmEffect::Crash) => return die().await,
            Some(effect) => return Err(refusal(effect, HostSite::DriveAsk)),
            None => {}
        }
        let asked = request.as_str().to_owned();
        self.inner.request_drive(session, request).await?;
        self.drives.asked.lock_recover().push(asked);
        Ok(())
    }
}

struct CrashControl {
    inner: Arc<dyn lash_core::engine::SessionControlEngine>,
    faults: Arc<HostFaults>,
}

fn refusal(effect: ArmEffect, site: HostSite) -> lash_core::engine::EngineRefusal {
    match effect {
        ArmEffect::Refuse => lash_core::engine::EngineRefusal::Permanent {
            code: lash_core::RuntimeErrorCode::EngineTurnTerminalAttach,
            message: format!("crash matrix: {site:?} refused for good"),
        },
        ArmEffect::Crash | ArmEffect::FailRetryable => lash_core::engine::EngineRefusal::Retryable(
            format!("crash matrix: {site:?} failed retryably"),
        ),
    }
}

#[async_trait::async_trait]
impl lash_core::engine::SessionControlEngine for CrashControl {
    async fn reconcile_parks(
        &self,
        parks: &dyn lash_core::engine::ParkRecoveryWriter,
        page: lash_core::engine::EnginePage,
    ) -> Result<lash_core::engine::ParkReconcileReport, lash_core::engine::EngineRefusal> {
        self.inner.reconcile_parks(parks, page).await
    }

    async fn resume_root(
        &self,
        target: &lash_core::engine::RootRef,
        engine: Option<&lash_core::store::EnginePark>,
    ) -> Result<lash_core::engine::EngineAck, lash_core::engine::EngineRefusal> {
        self.inner.resume_root(target, engine).await
    }

    async fn resume_process(
        &self,
        process: &lash_core::ProcessId,
        park: lash_core::engine::ParkId,
    ) -> Result<lash_core::engine::EngineAck, lash_core::engine::EngineRefusal> {
        self.inner.resume_process(process, park).await
    }

    async fn release_root(
        &self,
        target: &lash_core::engine::RootRef,
        engine: Option<&lash_core::store::EnginePark>,
    ) -> Result<lash_core::engine::EngineAck, lash_core::engine::EngineRefusal> {
        let detail = format!("{target:?}");
        match self.faults.take(HostSite::ReleaseRootBefore, &detail) {
            Some(ArmEffect::Crash) => return die().await,
            Some(effect) => return Err(refusal(effect, HostSite::ReleaseRootBefore)),
            None => {}
        }
        let answer = self.inner.release_root(target, engine).await;
        match self.faults.take(HostSite::ReleaseRootAfter, &detail) {
            Some(ArmEffect::Crash) => die().await,
            Some(effect) => Err(refusal(effect, HostSite::ReleaseRootAfter)),
            None => answer,
        }
    }
}

// ---------------------------------------------------------------------------
// The session store factory
// ---------------------------------------------------------------------------

/// The deployment's session catalog with [`HostFaults`] on the writes a seam
/// cuts between: the intent acknowledgement and the physical delete.
pub struct CrashSessionFactory {
    inner: Arc<dyn SessionStoreFactory>,
    faults: Arc<HostFaults>,
}

impl CrashSessionFactory {
    #[must_use]
    pub fn new(inner: Arc<dyn SessionStoreFactory>, faults: Arc<HostFaults>) -> Self {
        Self { inner, faults }
    }
}

type StoreResult<T> = Result<T, lash_core::StoreError>;

#[async_trait::async_trait]
impl SessionStoreFactory for CrashSessionFactory {
    fn bind_effect_host(&self, effect_host: &Arc<dyn lash_core::EffectHost>) {
        self.inner.bind_effect_host(effect_host);
    }

    fn bind_artifact_stores(
        &self,
        process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
        process_engines: lash_core::ProcessEngineRegistry,
    ) {
        self.inner
            .bind_artifact_stores(process_env_store, process_engines);
    }

    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> StoreResult<Arc<dyn lash_core::RuntimePersistence>> {
        self.inner.create_store(request).await
    }

    async fn open_existing_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> Result<Option<Arc<dyn lash_core::RuntimePersistence>>, String> {
        self.inner.open_existing_store(request).await
    }

    async fn open_unbound_store(&self) -> StoreResult<Arc<dyn lash_core::RuntimePersistence>> {
        self.inner.open_unbound_store().await
    }

    async fn read_session(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<lash_core::SessionReadView>> {
        self.inner.read_session(session_id).await
    }

    async fn list_sessions(
        &self,
        filter: &lash_core::SessionListFilter,
    ) -> StoreResult<Vec<lash_core::SessionSummary>> {
        self.inner.list_sessions(filter).await
    }

    async fn count_unsettled_turns(&self) -> StoreResult<lash_core::store::UnsettledTurnCounts> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash_core::store::TurnParkQuery,
    ) -> StoreResult<Vec<lash_core::store::TurnPark>> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash_core::store::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> StoreResult<lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>> {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash_core::store::ParkFeedCursor,
    ) -> StoreResult<()> {
        self.inner.compact_turn_park_feed(through).await
    }

    async fn root_terminal(
        &self,
        session_id: &SessionId,
        root: &lash_core::TurnId,
    ) -> StoreResult<Option<lash_core::store::RootTerminal>> {
        self.inner.root_terminal(session_id, root).await
    }

    async fn non_terminal_roots_page(
        &self,
        after: Option<&lash_core::engine::RootRef>,
        limit: std::num::NonZeroUsize,
    ) -> StoreResult<Vec<lash_core::engine::RootRef>> {
        self.inner.non_terminal_roots_page(after, limit).await
    }

    async fn end_lost_root(
        &self,
        target: &lash_core::engine::RootRef,
        at_ms: u64,
    ) -> StoreResult<Option<lash_core::store::RootTerminal>> {
        self.inner.end_lost_root(target, at_ms).await
    }

    async fn list_control_intents(
        &self,
        after: Option<lash_core::store::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> StoreResult<Vec<lash_core::store::ControlIntent>> {
        self.inner.list_control_intents(after, limit).await
    }

    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Option<Arc<dyn lash_core::RuntimePersistence>>> {
        self.inner.open_existing_store_by_id(session_id).await
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        session_id: &SessionId,
    ) -> StoreResult<Vec<lash_core::TurnCancelClosureAuthorization>> {
        self.inner
            .pending_turn_cancel_closure_pins(session_id)
            .await
    }

    async fn retire_turn_cancel_closure_scope(
        &self,
        scope: &lash_core::ExecutionScope,
    ) -> StoreResult<()> {
        self.inner.retire_turn_cancel_closure_scope(scope).await
    }

    async fn has_claimable_queued_work(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> StoreResult<Option<bool>> {
        self.inner.has_claimable_queued_work(request).await
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash_core::store::MaintenanceResult<lash_core::store::SessionBlobReclaimReport> {
        if let Some(effect) = self
            .faults
            .take(HostSite::DeleteStorageBefore, session_id.as_str())
        {
            if effect == ArmEffect::Crash {
                return die().await;
            }
            return Err(
                lash_core::store::MaintenanceFailure::failed_before_any_work(
                    lash_core::StoreError::Backend(format!(
                        "crash matrix: {effect:?} at the physical delete"
                    )),
                ),
            );
        }
        self.inner.delete_session(session_id).await
    }

    async fn reclaim_retained_evidence(
        &self,
        bound: lash_core::store::RetentionBound,
    ) -> lash_core::store::MaintenanceResult<lash_core::store::RetentionReport> {
        self.inner.reclaim_retained_evidence(bound).await
    }

    async fn pin(&self, node_id: &str) -> StoreResult<lash_core::ForkPoint> {
        self.inner.pin(node_id).await
    }

    async fn unpin(&self, node_id: &str) -> StoreResult<()> {
        self.inner.unpin(node_id).await
    }

    async fn fork_points(&self) -> StoreResult<Vec<lash_core::ForkPoint>> {
        self.inner.fork_points().await
    }

    async fn fork_at(
        &self,
        request: &lash_core::ForkSessionRequest,
    ) -> StoreResult<lash_core::ForkSessionReceipt> {
        self.inner.fork_at(request).await
    }
}

#[async_trait::async_trait]
impl lash_core::store::ControlIntentStore for CrashSessionFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> StoreResult<Option<lash_core::store::ControlIntent>> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash_core::store::ControlIntentId,
        at_ms: u64,
    ) -> StoreResult<lash_core::store::IntentApplication> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        at_ms: u64,
    ) -> StoreResult<lash_core::store::IntentSettle> {
        match self
            .faults
            .take(HostSite::AcknowledgeIntentBefore, &id.to_string())
        {
            Some(ArmEffect::Crash) => die().await,
            Some(effect) => Err(lash_core::StoreError::Backend(format!(
                "crash matrix: {effect:?} at the intent acknowledgement"
            ))),
            None => self.inner.acknowledge_intent(id, claim, at_ms).await,
        }
    }

    async fn record_intent_failure(
        &self,
        id: lash_core::store::ControlIntentId,
        claim: &lash_core::store::ClaimToken,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> StoreResult<lash_core::store::IntentSettle> {
        self.inner
            .record_intent_failure(id, claim, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash_core::store::ControlIntentId,
    ) -> StoreResult<Option<lash_core::store::ControlIntent>> {
        self.inner.load_intent(id).await
    }

    async fn open_root_intent(
        &self,
        request: &lash_core::store::RootIntentRequest,
        at_ms: u64,
    ) -> Result<lash_core::store::ControlIntent, lash_core::store::RootIntentRefused> {
        self.inner.open_root_intent(request, at_ms).await
    }
}

#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for CrashSessionFactory {
    fn can_prove_process_owner_death(&self) -> bool {
        self.inner.can_prove_process_owner_death()
    }

    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> StoreResult<std::collections::BTreeSet<lash_core::AttachmentId>> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn list_condemnations(
        &self,
    ) -> StoreResult<Vec<lash_core::store::AttachmentCondemnationRecord>> {
        self.inner.list_condemnations().await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash_core::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> StoreResult<bool> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}

// ---------------------------------------------------------------------------
// The process-work port
// ---------------------------------------------------------------------------

/// The deployment's process-work port with [`HostFaults`] on a child's
/// cancel delivery.
pub struct CrashProcessPort {
    inner: Arc<dyn lash_core::ProcessWorkSubstrate>,
    faults: Arc<HostFaults>,
}

impl CrashProcessPort {
    #[must_use]
    pub fn new(inner: Arc<dyn lash_core::ProcessWorkSubstrate>, faults: Arc<HostFaults>) -> Self {
        Self { inner, faults }
    }
}

/// What the process-work port answers at an armed site: a refusal for good
/// is a terminal engine error (the engine has nothing that could ever take
/// the delivery), a retryable failure an opaque one a later attempt may
/// clear.
fn port_failure(effect: ArmEffect, site: HostSite) -> lash_core::PluginError {
    match effect {
        ArmEffect::Refuse => lash_core::PluginError::Runtime(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::EngineServiceUnregistered,
            format!("crash matrix: {site:?} refused for good"),
        )),
        ArmEffect::Crash | ArmEffect::FailRetryable => {
            lash_core::PluginError::Invoke(format!("crash matrix: {effect:?} at {site:?}"))
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessWorkSubstrate for CrashProcessPort {
    async fn deliver_process_start(
        &self,
        record: &lash_core::ProcessRecord,
    ) -> Result<(), lash_core::PluginError> {
        self.inner.deliver_process_start(record).await
    }

    async fn await_process_terminal(
        &self,
        process_id: &lash_core::ProcessId,
    ) -> Result<lash_core::ProcessTerminalWait, lash_core::PluginError> {
        self.inner.await_process_terminal(process_id).await
    }

    async fn deliver_cancel(
        &self,
        process_id: &lash_core::ProcessId,
        request: &lash_core::CancelRequest,
        key: &str,
    ) -> Result<(), lash_core::PluginError> {
        let detail = format!("{process_id}/{key}");
        match self.faults.take(HostSite::DeliverCancelBefore, &detail) {
            Some(ArmEffect::Crash) => return die().await,
            Some(effect) => return Err(port_failure(effect, HostSite::DeliverCancelBefore)),
            None => {}
        }
        let answer = self.inner.deliver_cancel(process_id, request, key).await;
        match self.faults.take(HostSite::DeliverCancelAfter, &detail) {
            Some(ArmEffect::Crash) => die().await,
            Some(effect) => Err(port_failure(effect, HostSite::DeliverCancelAfter)),
            None => answer,
        }
    }

    async fn publish_process_terminal(
        &self,
        process_id: &lash_core::ProcessId,
        output: &lash_core::ProcessAwaitOutput,
        key: &str,
    ) -> Result<(), lash_core::PluginError> {
        self.inner
            .publish_process_terminal(process_id, output, key)
            .await
    }

    /// The drain's wake reaches the engine unchanged: the trait default
    /// refuses, which would leave every drained process where it waits.
    async fn deliver_hand_over(
        &self,
        process_id: &lash_core::ProcessId,
        generation: &lash_core::engine::BuildGeneration,
    ) -> Result<(), lash_core::PluginError> {
        self.inner.deliver_hand_over(process_id, generation).await
    }
}
