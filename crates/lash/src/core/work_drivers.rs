use super::queued_work::{InlineQueuedWorkRunConfig, InlineQueuedWorkRunHandle};
use crate::support::*;
use lash_core::facade_support;

/// How a [`LashCore`] resolves its process-work port, decided at `build()`
/// and shared across clones.
pub(super) enum ProcessPortSetup {
    /// No process registry is wired; there is nothing to run.
    None,
    /// Lazily construct the native process-work port on first
    /// `session().open()`. A store factory is required to build the config (the
    /// worker rebuilds a session runtime per process); a registry with no store
    /// factory is rejected at build with
    /// [`EmbedError::ProcessRegistryRequiresStoreFactory`].
    LazyDefault {
        config: Box<super::InlineProcessWorkerSetup>,
        watched: facade_support::WatchedRegistry,
    },
    /// The host wired an external process-work port.
    External { wiring: ProcessWorkWiring },
}

#[derive(Clone, Default)]
pub(super) enum ProcessWorkSelection {
    #[default]
    None,
    Inline(Arc<dyn ProcessRegistry>),
    External(ProcessWorkWiring),
}

impl ProcessWorkSelection {
    pub(super) fn resolve(
        self,
        clock: Arc<dyn lash_core::Clock>,
        sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    ) -> ProcessWorkSource {
        match self {
            Self::None => ProcessWorkSource::None,
            Self::Inline(registry) => {
                let registry = registry.with_runtime_clock(clock).unwrap_or(registry);
                ProcessWorkSource::Inline(facade_support::watch_process_registry_with_sink(
                    registry, sink,
                ))
            }
            Self::External(wiring) => ProcessWorkSource::External(wiring),
        }
    }
}

#[derive(Clone)]
pub(super) enum ProcessWorkSource {
    None,
    Inline(facade_support::WatchedRegistry),
    External(ProcessWorkWiring),
}

impl ProcessWorkSource {
    pub(super) fn process_registry(&self) -> Option<Arc<dyn ProcessRegistry>> {
        match self {
            Self::None => None,
            Self::Inline(watched) => Some(Arc::clone(watched.registry())),
            Self::External(_) => None,
        }
    }

    pub(super) fn external_wiring(&self) -> Option<ProcessWorkWiring> {
        match self {
            Self::External(wiring) => Some(wiring.clone()),
            Self::None | Self::Inline(_) => None,
        }
    }

    pub(super) fn has_registry(&self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone)]
pub(super) enum QueuedWorkSource {
    Unset,
    Disabled,
    Native,
    External(Arc<dyn QueuedWorkSubstrate>),
}

pub(super) enum QueuedPortSetup {
    Disabled,
    Native {
        config: Arc<InlineQueuedWorkRunConfig>,
        slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        execution_concurrency: usize,
    },
    External {
        port: Arc<dyn QueuedWorkSubstrate>,
    },
}

pub(super) struct WakeDeliveryDriverSetup {
    pub(super) registry: Arc<dyn ProcessRegistry>,
    pub(super) factory: Arc<dyn SessionStoreFactory>,
    pub(super) clock: Arc<dyn lash_core::Clock>,
    pub(super) delivery_policy: lash_core::DeliveryPolicy,
}

pub(super) struct NativeSubstrateSetup {
    pub(super) config: NativeSubstrateConfig,
    pub(super) process: ProcessPortSetup,
    pub(super) queued: QueuedPortSetup,
    pub(super) wake: Option<WakeDeliveryDriverSetup>,
}

#[derive(Clone)]
pub(crate) struct ResolvedPorts {
    pub(crate) process: Option<ProcessWorkWiring>,
    pub(crate) queued: Arc<ResolvedQueuedWork>,
    pub(crate) drive_process_on_open: bool,
}

impl ResolvedPorts {
    pub(crate) fn queued_port(&self) -> Arc<dyn QueuedWorkSubstrate> {
        self.queued.clone()
    }
}

pub(crate) struct ResolvedQueuedWork {
    port: Arc<dyn QueuedWorkSubstrate>,
    wake: std::sync::Mutex<Option<facade_support::WakeDeliveryDriver>>,
}

impl ResolvedQueuedWork {
    fn new(port: Arc<dyn QueuedWorkSubstrate>) -> Self {
        Self {
            port,
            wake: std::sync::Mutex::new(None),
        }
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
impl QueuedWorkSubstrate for ResolvedQueuedWork {
    fn notify_session_work(&self, target: SessionWorkTarget, reason: &str) {
        self.port.notify_session_work(target, reason);
    }

    async fn drain_session_work(
        &self,
        target: SessionWorkTarget,
        reason: &str,
    ) -> std::result::Result<lash_core::SessionDrainOutcome, lash_core::PluginError> {
        self.port.drain_session_work(target, reason).await
    }
}

/// Shared, lazily-initialized host-work state for a [`LashCore`].
///
/// The once-guard ([`tokio::sync::OnceCell`]) constructs native ports exactly
/// once across `LashCore` clones, on the first `session().open()` or admin path
/// that needs them.
pub(crate) struct NativeSubstrateSlot {
    pub(super) setup: NativeSubstrateSetup,
    drivers: tokio::sync::OnceCell<ResolvedPorts>,
    phase_probe_slot: Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot>,
}

impl NativeSubstrateSlot {
    pub(super) fn new(setup: NativeSubstrateSetup) -> Self {
        let phase_probe_slot = match &setup.process {
            ProcessPortSetup::LazyDefault { config, .. } => {
                Some(config.turn_phase_probe_slot.clone())
            }
            ProcessPortSetup::None | ProcessPortSetup::External { .. } => None,
        };
        Self {
            setup,
            drivers: tokio::sync::OnceCell::new(),
            phase_probe_slot,
        }
    }

    /// Resolve host work ports for a session host. Idempotent: the once-guard
    /// ensures native ports are constructed once.
    pub(crate) async fn ports(&self) -> ResolvedPorts {
        self.drivers
            .get_or_init(|| async {
                let queued_port: Arc<dyn QueuedWorkSubstrate> = match &self.setup.queued {
                    QueuedPortSetup::Disabled => Arc::new(NoQueuedWork::new()),
                    QueuedPortSetup::External { port } => Arc::clone(port),
                    QueuedPortSetup::Native {
                        config,
                        slot_supplier,
                        execution_concurrency,
                    } => {
                        let run_handle =
                            Arc::new(InlineQueuedWorkRunHandle::new(Arc::clone(config)));
                        let work_cadence = self.setup.config.work_cadence.clone();
                        Arc::new(match slot_supplier {
                            Some(slot_supplier) => {
                                facade_support::native_queued_work_with_worker_slot_supplier_and_work_cadence(
                                    run_handle,
                                    Arc::clone(slot_supplier),
                                    work_cadence,
                                )
                                .expect("native work cadence was validated at build")
                            }
                            None => facade_support::native_queued_work_with_execution_concurrency_and_work_cadence(
                                run_handle,
                                *execution_concurrency,
                                work_cadence,
                            )
                            .expect("queued-work concurrency was validated at build"),
                        })
                    }
                };
                let (process, drive_process_on_open) = match &self.setup.process {
                    ProcessPortSetup::None => (None, false),
                    ProcessPortSetup::External { wiring } => (Some(wiring.clone()), false),
                    ProcessPortSetup::LazyDefault { config, watched } => {
                        // The worker only forwards notifications through this port;
                        // the outer dispatcher remains the sole native-lane owner.
                        let config = config
                            .build(Arc::clone(&queued_port))
                            .expect("inline process-worker assembly was validated at build");
                        let watched = watched.clone();
                        let worker = DurableProcessWorker::new(config)
                            .expect("native substrate config was validated at build");
                        let port: Arc<dyn ProcessWorkSubstrate> =
                            Arc::new(NativeProcessWork::new(&watched, worker));
                        (Some(ProcessWorkWiring::new(watched, port)), true)
                    }
                };
                let queued = Arc::new(ResolvedQueuedWork::new(queued_port));
                if let Some(setup) = self.setup.wake.as_ref() {
                    let queued_for_wake: Arc<dyn QueuedWorkSubstrate> = queued.clone();
                    let wake = facade_support::wake_delivery_driver_with_work_cadence(
                        Arc::clone(&setup.registry),
                        Arc::clone(&setup.factory),
                        queued_for_wake,
                        Arc::clone(&setup.clock),
                        setup.delivery_policy,
                        self.setup.config.work_cadence.clone(),
                    )
                    .expect("native work cadence was validated at build");
                    queued.install_wake(wake);
                }
                ResolvedPorts {
                    process,
                    queued,
                    drive_process_on_open,
                }
            })
            .await
            .clone()
    }

    pub(crate) fn phase_probe_slot(&self) -> Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot> {
        self.phase_probe_slot.clone()
    }

    #[cfg(test)]
    pub(crate) fn process_worker_config(&self) -> Option<DurableProcessWorkerConfig> {
        match &self.setup.process {
            ProcessPortSetup::LazyDefault { config, .. } => Some(
                config
                    .build(Arc::new(NoQueuedWork::new()))
                    .expect("inline process-worker assembly was validated at build"),
            ),
            ProcessPortSetup::None | ProcessPortSetup::External { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn native_process_change_hub(&self) -> Option<facade_support::ProcessChangeHub> {
        match &self.setup.process {
            ProcessPortSetup::LazyDefault { watched, .. } => Some(watched.hub().clone()),
            ProcessPortSetup::None | ProcessPortSetup::External { .. } => None,
        }
    }

    pub(super) fn configured_worker_process_work(&self) -> Option<WorkerProcessWork> {
        match &self.setup.process {
            ProcessPortSetup::None => None,
            ProcessPortSetup::LazyDefault { watched, .. } => {
                Some(WorkerProcessWork::SelfNative(watched.clone()))
            }
            ProcessPortSetup::External { wiring } => {
                Some(WorkerProcessWork::External(wiring.clone()))
            }
        }
    }
}
