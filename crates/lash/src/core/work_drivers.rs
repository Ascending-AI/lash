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
        config: Box<DurableProcessWorkerConfig>,
        hub: facade_support::ProcessChangeHub,
    },
    /// The host wired an external process-work port.
    External { wiring: ProcessWorkWiring },
}

#[derive(Clone, Default)]
pub(super) enum ProcessWorkSource {
    #[default]
    None,
    Inline {
        registry: Arc<dyn ProcessRegistry>,
        hub: Option<facade_support::ProcessChangeHub>,
    },
    External(ProcessWorkWiring),
}

impl ProcessWorkSource {
    pub(super) fn with_runtime_clock(self, clock: Arc<dyn lash_core::Clock>) -> Self {
        match self {
            Self::Inline { registry, hub } => Self::Inline {
                registry: registry.with_runtime_clock(clock).unwrap_or(registry),
                hub,
            },
            other => other,
        }
    }

    pub(super) fn process_registry(&self) -> Option<Arc<dyn ProcessRegistry>> {
        match self {
            Self::None => None,
            Self::Inline { registry, .. } => Some(Arc::clone(registry)),
            Self::External(_) => None,
        }
    }

    pub(super) fn external_wiring(&self) -> Option<ProcessWorkWiring> {
        match self {
            Self::External(wiring) => Some(wiring.clone()),
            Self::None | Self::Inline { .. } => None,
        }
    }

    pub(super) fn has_registry(&self) -> bool {
        !matches!(self, Self::None)
    }

    pub(super) fn watched(self, sink: Option<Arc<dyn facade_support::ProcessEventSink>>) -> Self {
        match self {
            Self::Inline {
                registry,
                hub: None,
            } => {
                let (registry, hub) =
                    facade_support::watch_process_registry_with_sink(registry, sink);
                Self::Inline {
                    registry,
                    hub: Some(hub),
                }
            }
            // An external deployment was wrapped by its host, which installs any
            // sink through its constructor; the inline sink does not
            // apply here. Already-watched inline sources keep their wrap.
            other => other,
        }
    }
}

#[derive(Clone, Default)]
pub(super) enum QueuedWorkSource {
    Disabled,
    #[default]
    LazyDefault,
    External(Arc<dyn QueuedWorkSubstrate>),
}

pub(super) enum QueuedPortSetup {
    Disabled,
    LazyDefault {
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
    setup: NativeSubstrateSetup,
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
                    QueuedPortSetup::LazyDefault {
                        config,
                        slot_supplier,
                        execution_concurrency,
                    } => {
                        let run_handle =
                            Arc::new(InlineQueuedWorkRunHandle::new(Arc::clone(config)));
                        Arc::new(match slot_supplier {
                            Some(slot_supplier) => NativeQueuedWork::with_worker_slot_supplier(
                                run_handle,
                                Arc::clone(slot_supplier),
                            ),
                            None => NativeQueuedWork::with_execution_concurrency(
                                run_handle,
                                *execution_concurrency,
                            )
                            .expect("queued-work concurrency was validated at build"),
                        })
                    }
                };
                let (process, drive_process_on_open) =
                    match &self.setup.process {
                        ProcessPortSetup::None => (None, false),
                        ProcessPortSetup::External { wiring } => (Some(wiring.clone()), false),
                        ProcessPortSetup::LazyDefault { config, hub } => {
                            let config = (**config)
                                .clone()
                                .with_queued_work(Arc::clone(&queued_port));
                            let registry = Arc::clone(&config.process_registry);
                            let worker = DurableProcessWorker::new(config);
                            let port: Arc<dyn ProcessWorkSubstrate> = Arc::new(
                                NativeProcessWork::new(Arc::clone(&registry), hub.clone(), worker),
                            );
                            (
                                Some(ProcessWorkWiring::new(registry, hub.clone(), port)),
                                true,
                            )
                        }
                    };
                let queued = Arc::new(ResolvedQueuedWork::new(queued_port));
                if let Some(setup) = self.setup.wake.as_ref() {
                    let queued_for_wake: Arc<dyn QueuedWorkSubstrate> = queued.clone();
                    let wake = facade_support::WakeDeliveryDriver::new(
                        Arc::clone(&setup.registry),
                        Arc::clone(&setup.factory),
                        queued_for_wake,
                        Arc::clone(&setup.clock),
                        setup.delivery_policy,
                    );
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
    pub(crate) fn process_worker_config(&self) -> Option<&DurableProcessWorkerConfig> {
        match &self.setup.process {
            ProcessPortSetup::LazyDefault { config, .. } => Some(config),
            ProcessPortSetup::None | ProcessPortSetup::External { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn native_process_change_hub(&self) -> Option<facade_support::ProcessChangeHub> {
        match &self.setup.process {
            ProcessPortSetup::LazyDefault { hub, .. } => Some(hub.clone()),
            ProcessPortSetup::None | ProcessPortSetup::External { .. } => None,
        }
    }

    pub(super) fn configured_worker_process_work(
        &self,
    ) -> Option<(facade_support::ProcessChangeHub, WorkerProcessWork)> {
        match &self.setup.process {
            ProcessPortSetup::None => None,
            ProcessPortSetup::LazyDefault { hub, .. } => {
                Some((hub.clone(), WorkerProcessWork::SelfNative))
            }
            ProcessPortSetup::External { wiring } => Some((
                // External nested waits use the external port. The required
                // native hub is consequently never consulted on this variant.
                facade_support::ProcessChangeHub::new(),
                WorkerProcessWork::External(wiring.clone()),
            )),
        }
    }

    pub(super) fn configured_queued_port(&self) -> Arc<dyn QueuedWorkSubstrate> {
        match &self.setup.queued {
            QueuedPortSetup::External { port } => Arc::clone(port),
            QueuedPortSetup::Disabled | QueuedPortSetup::LazyDefault { .. } => {
                Arc::new(NoQueuedWork::new())
            }
        }
    }
}
