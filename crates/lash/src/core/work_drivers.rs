use crate::support::{Arc, ProcessWorkWiring, SessionWorkEngine, async_trait};

pub(super) struct CoreWorkSetup {
    /// The backend's process work, which the core's sessions and host
    /// process APIs admit into.
    pub(super) process: ProcessWorkWiring,
    /// The backend's session-work engine, with the core's `SessionShifts` installed.
    pub(super) session_work: Arc<dyn SessionWorkEngine>,
    /// The backend's store binding: the settled-run mailbox keys by it.
    pub(super) store_binding: lash_core::StoreBindingId,
}

#[derive(Clone)]
pub(crate) struct ResolvedPorts {
    pub(crate) process: ProcessWorkWiring,
    pub(crate) queued: Arc<ResolvedQueuedWork>,
}

impl ResolvedPorts {
    pub(crate) fn queued_port(&self) -> Arc<dyn SessionWorkEngine> {
        self.queued.clone()
    }
}

pub(crate) struct ResolvedQueuedWork {
    port: Arc<dyn SessionWorkEngine>,
    /// The store binding the core's sessions live in.
    store_binding: lash_core::StoreBindingId,
}

impl ResolvedQueuedWork {
    fn new(port: Arc<dyn SessionWorkEngine>, store_binding: lash_core::StoreBindingId) -> Self {
        Self {
            port,
            store_binding,
        }
    }

    /// The store binding the core's sessions live in: with the session and
    /// the input, what names one input across the stores of one process.
    pub(crate) fn store_binding(&self) -> &lash_core::StoreBindingId {
        &self.store_binding
    }
}

#[async_trait]
impl SessionWorkEngine for ResolvedQueuedWork {
    fn schedule_shift(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::ShiftRequestId,
    ) {
        self.port.schedule_shift(session, request);
    }

    async fn request_shift(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::ShiftRequestId,
    ) -> std::result::Result<(), lash_core::engine::EngineRefusal> {
        self.port.request_shift(session, request).await
    }

    fn install_session_shifts(
        &self,
        shifts: Arc<dyn lash_core::SessionShifts>,
    ) -> Arc<dyn lash_core::SessionShifts> {
        self.port.install_session_shifts(shifts)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.port.control()
    }

    async fn await_shift(
        &self,
        session: &lash_core::SessionId,
        request: &lash_core::engine::ShiftRequestId,
    ) -> std::result::Result<lash_core::engine::ShiftOutcome, lash_core::engine::ShiftAbort> {
        self.port.await_shift(session, request).await
    }
}

/// Shared, lazily-initialized host-work state for a [`LashCore`].
///
/// The once-guard ([`tokio::sync::OnceCell`]) resolves the ports exactly
/// once across `LashCore` clones, on the first `session().open()` or admin
/// path that needs them.
pub(crate) struct CoreWorkSlot {
    pub(super) setup: CoreWorkSetup,
    drivers: tokio::sync::OnceCell<ResolvedPorts>,
}

impl CoreWorkSlot {
    pub(super) fn new(setup: CoreWorkSetup) -> Self {
        Self {
            setup,
            drivers: tokio::sync::OnceCell::new(),
        }
    }

    /// Idempotent: the once-guard resolves the ports once.
    pub(crate) async fn ports(&self) -> ResolvedPorts {
        self.drivers
            .get_or_init(|| async {
                let queued = Arc::new(ResolvedQueuedWork::new(
                    Arc::clone(&self.setup.session_work),
                    self.setup.store_binding.clone(),
                ));
                ResolvedPorts {
                    process: self.setup.process.clone(),
                    queued,
                }
            })
            .await
            .clone()
    }
}
