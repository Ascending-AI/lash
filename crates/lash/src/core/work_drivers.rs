use crate::support::{
    Arc, ProcessRegistry, ProcessWorkWiring, SessionStoreFactory, SessionWorkEngine, async_trait,
};
use lash_core::facade_support;

pub(super) struct WakeDeliveryDriverSetup {
    pub(super) registry: Arc<dyn ProcessRegistry>,
    pub(super) factory: Arc<dyn SessionStoreFactory>,
    pub(super) clock: Arc<dyn lash_core::Clock>,
    pub(super) delivery_policy: lash_core::DeliveryPolicy,
}

pub(super) struct CoreWorkSetup {
    /// The backend's process work, which the core's sessions and host
    /// process APIs admit into.
    pub(super) process: ProcessWorkWiring,
    /// The backend's session-work engine, with the core's driver installed.
    pub(super) session_work: Arc<dyn SessionWorkEngine>,
    pub(super) wake: WakeDeliveryDriverSetup,
    /// The backend's store binding: the settled-root mailbox keys by it.
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
    wake: std::sync::Mutex<Option<facade_support::WakeDeliveryDriver>>,
    /// The store binding the core's sessions live in.
    store_binding: lash_core::StoreBindingId,
}

impl ResolvedQueuedWork {
    fn new(port: Arc<dyn SessionWorkEngine>, store_binding: lash_core::StoreBindingId) -> Self {
        Self {
            port,
            wake: std::sync::Mutex::new(None),
            store_binding,
        }
    }

    /// The store binding the core's sessions live in: with the session and
    /// the input, what names one input across the stores of one process.
    pub(crate) fn store_binding(&self) -> &lash_core::StoreBindingId {
        &self.store_binding
    }

    fn install_wake(&self, wake: facade_support::WakeDeliveryDriver) {
        *self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(wake);
    }

    pub(crate) async fn drive_wake(
        &self,
    ) -> std::result::Result<facade_support::WakeDeliveryDriveReport, lash_core::PluginError> {
        let wake = self
            .wake
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(wake) = wake else {
            return Err(lash_core::PluginError::Session(
                "wake delivery driver is unavailable in this runtime".to_string(),
            ));
        };
        wake.drive_pending().await
    }
}

impl Drop for ResolvedQueuedWork {
    fn drop(&mut self) {
        if let Some(wake) = self
            .wake
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            wake.request_shutdown();
        }
    }
}

#[async_trait]
impl SessionWorkEngine for ResolvedQueuedWork {
    fn schedule_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) {
        self.port.schedule_drive(session, request);
    }

    async fn request_drive(
        &self,
        session: &lash_core::SessionId,
        request: lash_core::engine::DriveRequestId,
    ) -> std::result::Result<(), lash_core::engine::EngineRefusal> {
        self.port.request_drive(session, request).await
    }

    async fn session_work_in_flight(&self, session: &lash_core::SessionId) -> bool {
        self.port.session_work_in_flight(session).await
    }

    fn install_session_driver(
        &self,
        driver: Arc<dyn lash_core::SessionDriver>,
    ) -> Arc<dyn lash_core::SessionDriver> {
        self.port.install_session_driver(driver)
    }

    fn control(&self) -> Arc<dyn lash_core::engine::SessionControlEngine> {
        self.port.control()
    }

    async fn await_drive(
        &self,
        session: &lash_core::SessionId,
        request: &lash_core::engine::DriveRequestId,
    ) -> std::result::Result<lash_core::engine::DriveOutcome, lash_core::engine::DriveAbort> {
        self.port.await_drive(session, request).await
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
    #[expect(
        clippy::expect_used,
        reason = "the default work cadence is valid: \
                  `work_cadence_defaults_match_the_wait_and_delivery_constants` pins it"
    )]
    pub(crate) async fn ports(&self) -> ResolvedPorts {
        self.drivers
            .get_or_init(|| async {
                let queued = Arc::new(ResolvedQueuedWork::new(
                    Arc::clone(&self.setup.session_work),
                    self.setup.store_binding.clone(),
                ));
                let setup = &self.setup.wake;
                let queued_for_wake: Arc<dyn SessionWorkEngine> = queued.clone();
                let wake = facade_support::wake_delivery_driver_with_work_cadence(
                    Arc::clone(&setup.registry),
                    Arc::clone(&setup.factory),
                    queued_for_wake,
                    Arc::clone(&setup.clock),
                    setup.delivery_policy,
                    lash_core::WorkCadencePolicy::default(),
                )
                .expect("the default work cadence is valid");
                queued.install_wake(wake);
                ResolvedPorts {
                    process: self.setup.process.clone(),
                    queued,
                }
            })
            .await
            .clone()
    }
}
