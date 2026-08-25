#[derive(Default)]
struct SessionOpenAdmissionGateState {
    armed: bool,
    held_authority: Option<lash::persistence::SessionExecutionLeaseAuthority>,
    released: bool,
    tracking: bool,
}

struct SessionOpenAdmissionGate {
    session_id: String,
    state: std::sync::Mutex<SessionOpenAdmissionGateState>,
    admitted: tokio::sync::Notify,
    contended: tokio::sync::Notify,
    release: tokio::sync::Notify,
    attempts: std::sync::atomic::AtomicUsize,
    acquisitions: std::sync::atomic::AtomicUsize,
    admissions: std::sync::atomic::AtomicUsize,
    contentions: std::sync::atomic::AtomicUsize,
}

impl SessionOpenAdmissionGate {
    fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            state: std::sync::Mutex::new(SessionOpenAdmissionGateState::default()),
            admitted: tokio::sync::Notify::new(),
            contended: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            attempts: std::sync::atomic::AtomicUsize::new(0),
            acquisitions: std::sync::atomic::AtomicUsize::new(0),
            admissions: std::sync::atomic::AtomicUsize::new(0),
            contentions: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn arm(&self) {
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

    fn observe_claim(
        &self,
        session_id: &str,
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
                self.contended.notify_waiters();
            }
        }
    }

    async fn observe_admission(
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
            notified.await;
        }
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .held_authority = None;
    }

    async fn wait_until_admitted(&self) {
        use std::sync::atomic::Ordering;

        loop {
            let notified = self.admitted.notified();
            if self.admissions.load(Ordering::SeqCst) > 0 {
                return;
            }
            notified.await;
        }
    }

    async fn wait_until_contended(&self) {
        use std::sync::atomic::Ordering;

        loop {
            let notified = self.contended.notified();
            if self.contentions.load(Ordering::SeqCst) > 0 {
                return;
            }
            notified.await;
        }
    }

    fn release(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .released = true;
        self.release.notify_waiters();
    }

    fn finish(&self) {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .tracking = false;
    }

    fn counts(&self) -> (usize, usize, usize, usize) {
        use std::sync::atomic::Ordering;

        (
            self.attempts.load(Ordering::SeqCst),
            self.acquisitions.load(Ordering::SeqCst),
            self.admissions.load(Ordering::SeqCst),
            self.contentions.load(Ordering::SeqCst),
        )
    }
}

fn registered_session_open_admission_gates(
) -> &'static std::sync::Mutex<BTreeMap<String, Arc<SessionOpenAdmissionGate>>> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<BTreeMap<String, Arc<SessionOpenAdmissionGate>>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

fn register_session_open_admission_gate(gate: Arc<SessionOpenAdmissionGate>) {
    registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(gate.session_id.clone(), gate);
}

pub(crate) fn arm_registered_session_open_admission_gate(session_id: &str, reason: &str) {
    if reason != "queued_turn" {
        return;
    }
    if let Some(gate) = registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(session_id)
        .cloned()
    {
        gate.arm();
    }
}

fn unregister_session_open_admission_gate(session_id: &str) {
    registered_session_open_admission_gates()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(session_id);
}

struct GatedRuntimePersistence {
    inner: Arc<dyn lash::persistence::RuntimePersistence>,
    gate: Arc<SessionOpenAdmissionGate>,
}

// The host-facing seam stays on `lash::persistence`; only the test-only
// delegating implementation trait lacks a facade home.
use lash_core as lash_test_internals;

#[async_trait::async_trait]
impl lash_test_internals::store::RuntimePersistenceDecorator for GatedRuntimePersistence {
    fn inner(&self) -> &(dyn lash::persistence::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &lash::persistence::LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<
        lash::persistence::SessionExecutionLeaseClaimOutcome,
        lash::persistence::StoreError,
    > {
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

struct GatedSessionStoreFactory {
    inner: Arc<dyn lash::persistence::SessionStoreFactory>,
    gate: Arc<SessionOpenAdmissionGate>,
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

    async fn session_was_deleted(&self, session_id: &str) -> Result<bool, String> {
        self.inner.session_was_deleted(session_id).await
    }

    async fn delete_session(
        &self,
        session_id: &str,
    ) -> lash::persistence::MaintenanceResult<lash::persistence::SessionBlobReclaimReport> {
        self.inner.delete_session(session_id).await
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
