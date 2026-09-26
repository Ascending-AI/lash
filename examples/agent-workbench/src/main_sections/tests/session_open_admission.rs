use super::*;
use lash::SessionId;

#[derive(Default)]
pub(crate) struct SessionOpenAdmissionGateState {
    pub(super) armed: bool,
    pub(super) held_authority: Option<lash::persistence::SessionExecutionLeaseAuthority>,
    pub(super) released: bool,
    pub(super) tracking: bool,
}

pub(crate) struct SessionOpenAdmissionGate {
    pub(super) session_id: SessionId,
    pub(super) state: std::sync::Mutex<SessionOpenAdmissionGateState>,
    pub(super) admitted: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
    pub(super) attempts: std::sync::atomic::AtomicUsize,
    pub(super) acquisitions: std::sync::atomic::AtomicUsize,
    pub(super) admissions: std::sync::atomic::AtomicUsize,
    pub(super) contentions: std::sync::atomic::AtomicUsize,
}

/// How long a gate wait may block before the premise it rests on is declared
/// dead, and how long a gated admission may stay held.
///
/// A wait that cannot be satisfied leaves the admitted open held, and the
/// handler that opened it parked inside `admit_session_state`. Restate then
/// retries that handler on a growing backoff, and every retry re-arms the gate
/// against a still-held admission — 16 panics over the job's 40-minute timeout,
/// with the real cause (a premise that no longer holds) nowhere in the log.
/// Both bounds turn that into one named failure in seconds (FIG-3151).
pub(super) const ADMISSION_GATE_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const ADMISSION_GATE_MAX_HOLD: Duration = Duration::from_secs(60);

impl SessionOpenAdmissionGate {
    pub(super) fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            state: std::sync::Mutex::new(SessionOpenAdmissionGateState::default()),
            admitted: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            acquisitions: std::sync::atomic::AtomicUsize::new(0),
            admissions: std::sync::atomic::AtomicUsize::new(0),
            contentions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(super) fn arm(&self) {
        use std::sync::atomic::Ordering;

        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        assert!(!state.armed, "session-open admission gate is already armed");
        assert!(
            state.held_authority.is_none(),
            "session-open admission gate still holds an admitted open"
        );
        state.armed = true;
        state.released = false;
        state.tracking = true;
        self.attempts.store(0, Ordering::SeqCst);
        self.acquisitions.store(0, Ordering::SeqCst);
        self.admissions.store(0, Ordering::SeqCst);
        self.contentions.store(0, Ordering::SeqCst);
    }

    pub(super) fn observe_claim(
        &self,
        session_id: &SessionId,
        outcome: &lash::persistence::SessionExecutionLeaseClaimOutcome,
    ) {
        use std::sync::atomic::Ordering;

        if session_id != self.session_id {
            return;
        }
        let tracking = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tracking;
        if !tracking {
            return;
        }
        self.attempts.fetch_add(1, Ordering::SeqCst);
        match outcome {
            lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => {
                self.acquisitions.fetch_add(1, Ordering::SeqCst);
                let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if state.armed {
                    state.armed = false;
                    state.held_authority = Some(acquisition.lease.fence());
                }
            }
            lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { .. } => {
                self.contentions.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    pub(super) async fn observe_admission(
        &self,
        authority: &lash::persistence::SessionExecutionLeaseAuthority,
    ) {
        use std::sync::atomic::Ordering;

        let should_hold = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if !state.tracking {
                return;
            }
            self.admissions.fetch_add(1, Ordering::SeqCst);
            state.held_authority.as_ref() == Some(authority)
        };
        if !should_hold {
            return;
        }
        self.admitted.notify_waiters();
        let hold_deadline = tokio::time::Instant::now() + ADMISSION_GATE_MAX_HOLD;
        loop {
            let notified = self.release.notified();
            if self
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .released
            {
                break;
            }
            if tokio::time::timeout_at(hold_deadline, notified)
                .await
                .is_err()
            {
                break;
            }
        }
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .held_authority = None;
    }

    pub(super) async fn wait_until_admitted(&self) {
        use std::sync::atomic::Ordering;

        let deadline = tokio::time::Instant::now() + ADMISSION_GATE_WAIT_TIMEOUT;
        loop {
            let notified = self.admitted.notified();
            if self.admissions.load(Ordering::SeqCst) > 0 {
                return;
            }
            assert!(
                tokio::time::timeout_at(deadline, notified).await.is_ok(),
                "no gated open reached admit_session_state within {ADMISSION_GATE_WAIT_TIMEOUT:?}"
            );
        }
    }

    pub(super) fn release(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .released = true;
        self.release.notify_waiters();
    }

    pub(super) fn finish(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tracking = false;
    }

    pub(super) fn counts(&self) -> (usize, usize, usize, usize) {
        use std::sync::atomic::Ordering;

        (
            self.attempts.load(Ordering::SeqCst),
            self.acquisitions.load(Ordering::SeqCst),
            self.admissions.load(Ordering::SeqCst),
            self.contentions.load(Ordering::SeqCst),
        )
    }
}

pub(crate) fn registered_session_open_admission_gates()
-> &'static std::sync::Mutex<BTreeMap<SessionId, Arc<SessionOpenAdmissionGate>>> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<BTreeMap<SessionId, Arc<SessionOpenAdmissionGate>>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

pub(crate) fn register_session_open_admission_gate(gate: Arc<SessionOpenAdmissionGate>) {
    registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(gate.session_id.clone(), gate);
}

pub(crate) fn unregister_session_open_admission_gate(session_id: &SessionId) {
    registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(session_id);
}

struct GatedRuntimePersistence {
    pub(super) inner: Arc<dyn lash::persistence::RuntimePersistence>,
    pub(super) gate: Arc<SessionOpenAdmissionGate>,
}

// The test-only decorator is still implemented through the host-facing
// `lash::persistence` facade so this example has no internal-crate imports.
#[async_trait::async_trait]
impl lash::persistence::RuntimePersistenceDecorator for GatedRuntimePersistence {
    fn inner(&self) -> &(dyn lash::persistence::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn try_claim_session_execution_lease(
        &self,
        session_id: &SessionId,
        owner: &lash::persistence::LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<lash::persistence::SessionExecutionLeaseClaimOutcome, lash::persistence::StoreError>
    {
        let outcome = self
            .inner
            .try_claim_session_execution_lease(session_id, owner, executor_id, lease_ttl_ms)
            .await?;
        self.gate.observe_claim(session_id, &outcome);
        Ok(outcome)
    }

    async fn admit_session_state(
        &self,
        authority: &lash::persistence::SessionExecutionLeaseAuthority,
    ) -> Result<lash::persistence::SessionStateAdmission, lash::persistence::StoreError> {
        let admission = self.inner.admit_session_state(authority).await?;
        self.gate.observe_admission(authority).await;
        Ok(admission)
    }
}

/// `inner` with its session catalog replaced by `catalog`, so a backend
/// over it opens sessions through the admission gate.
pub(crate) struct GatedStoreSet {
    pub(super) inner: Arc<dyn lash::StoreSet>,
    pub(super) catalog: Arc<dyn lash::persistence::SessionStoreFactory>,
}

impl lash::StoreSet for GatedStoreSet {
    fn binding_identity(&self) -> &lash::StoreBindingId {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash::runtime::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn lash::persistence::SessionStoreFactory> {
        Arc::clone(&self.catalog)
    }

    fn process_registry(&self) -> Arc<dyn lash::process::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn process_continuations(&self) -> Arc<dyn lash::process::ProcessContinuationStore> {
        self.inner.process_continuations()
    }

    fn trigger_store(&self) -> Arc<dyn lash::triggers::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash::process::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash::persistence::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash::persistence::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash::persistence::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }
}

pub(crate) struct GatedSessionStoreFactory {
    pub(super) inner: Arc<dyn lash::persistence::SessionStoreFactory>,
    pub(super) gate: Arc<SessionOpenAdmissionGate>,
}

#[async_trait::async_trait]
impl lash::persistence::SessionStoreFactory for GatedSessionStoreFactory {
    async fn create_store(
        &self,
        request: &lash::persistence::SessionStoreCreateRequest,
    ) -> Result<Arc<dyn lash::persistence::RuntimePersistence>, lash::persistence::StoreError> {
        let inner = self.inner.create_store(request).await?;
        Ok(Arc::new(GatedRuntimePersistence {
            inner,
            gate: Arc::clone(&self.gate),
        }))
    }

    // A decorator forwards the non-creating by-id seam, keeping the gate on
    // the store it hands back.
    async fn open_existing_store_by_id(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Arc<dyn lash::persistence::RuntimePersistence>>, lash::persistence::StoreError>
    {
        Ok(self
            .inner
            .open_existing_store_by_id(session_id)
            .await?
            .map(|inner| {
                Arc::new(GatedRuntimePersistence {
                    inner,
                    gate: Arc::clone(&self.gate),
                }) as Arc<dyn lash::persistence::RuntimePersistence>
            }))
    }

    async fn session_was_deleted(&self, session_id: &SessionId) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &SessionId,
    ) -> lash::persistence::MaintenanceResult<lash::persistence::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
    }

    // A decorator forwards the deployment turn count to the catalog it wraps.
    async fn count_unsettled_turns(
        &self,
    ) -> Result<lash::persistence::UnsettledTurnCounts, lash::persistence::StoreError> {
        self.inner.count_unsettled_turns().await
    }

    async fn list_turn_parks(
        &self,
        query: &lash::persistence::TurnParkQuery,
    ) -> Result<Vec<lash::persistence::TurnPark>, lash::persistence::StoreError> {
        self.inner.list_turn_parks(query).await
    }

    async fn turn_park_feed(
        &self,
        after: lash::persistence::ParkFeedCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<
        lash::persistence::ParkFeedPage<lash::persistence::TurnParkTarget>,
        lash::persistence::StoreError,
    > {
        self.inner.turn_park_feed(after, limit).await
    }

    async fn root_terminal(
        &self,
        session_id: &lash::SessionId,
        root: &lash::TurnId,
    ) -> std::result::Result<Option<lash::persistence::RootTerminal>, lash::persistence::StoreError>
    {
        self.inner.root_terminal(session_id, root).await
    }

    async fn list_open_control_intents(
        &self,
        after: Option<lash::persistence::ControlIntentId>,
        limit: std::num::NonZeroUsize,
    ) -> std::result::Result<Vec<lash::persistence::ControlIntent>, lash::persistence::StoreError>
    {
        self.inner.list_open_control_intents(after, limit).await
    }

    async fn compact_turn_park_feed(
        &self,
        through: lash::persistence::ParkFeedCursor,
    ) -> Result<(), lash::persistence::StoreError> {
        self.inner.compact_turn_park_feed(through).await
    }
}

#[async_trait::async_trait]
impl lash::persistence::ControlIntentStore for GatedSessionStoreFactory {
    async fn begin_session_close(
        &self,
        session_id: &SessionId,
        at_ms: u64,
    ) -> Result<Option<lash::persistence::ControlIntent>, lash::persistence::StoreError> {
        self.inner.begin_session_close(session_id, at_ms).await
    }

    async fn claim_intent_application(
        &self,
        id: lash::persistence::ControlIntentId,
        at_ms: u64,
    ) -> Result<lash::persistence::IntentApplication, lash::persistence::StoreError> {
        self.inner.claim_intent_application(id, at_ms).await
    }

    async fn acknowledge_intent(
        &self,
        id: lash::persistence::ControlIntentId,
        at_ms: u64,
    ) -> Result<(), lash::persistence::StoreError> {
        self.inner.acknowledge_intent(id, at_ms).await
    }

    async fn record_intent_failure(
        &self,
        id: lash::persistence::ControlIntentId,
        error: &str,
        retryable: bool,
        at_ms: u64,
    ) -> Result<lash::persistence::ControlIntent, lash::persistence::StoreError> {
        self.inner
            .record_intent_failure(id, error, retryable, at_ms)
            .await
    }

    async fn load_intent(
        &self,
        id: lash::persistence::ControlIntentId,
    ) -> Result<Option<lash::persistence::ControlIntent>, lash::persistence::StoreError> {
        self.inner.load_intent(id).await
    }
}

#[async_trait::async_trait]
impl lash::persistence::AttachmentRootSet for GatedSessionStoreFactory {
    async fn live_attachment_refs(
        &self,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<BTreeSet<lash::attachments::AttachmentId>, lash::persistence::StoreError> {
        self.inner
            .live_attachment_refs(intent_grace_cutoff_epoch_ms)
            .await
    }

    async fn has_live_attachment_ref(
        &self,
        id: &lash::attachments::AttachmentId,
        intent_grace_cutoff_epoch_ms: u64,
    ) -> Result<bool, lash::persistence::StoreError> {
        self.inner
            .has_live_attachment_ref(id, intent_grace_cutoff_epoch_ms)
            .await
    }
}
