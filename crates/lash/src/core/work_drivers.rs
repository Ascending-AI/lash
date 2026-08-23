use super::queued_work::{InlineQueuedWorkRunConfig, InlineQueuedWorkRunHandle};
use crate::support::*;
use lash_core::facade_support;

/// How a [`LashCore`] resolves its process work driver, decided at `build()`
/// and shared across clones.
pub(super) enum ProcessWorkDriverSetup {
    /// No process registry is wired; there is nothing to run.
    None,
    /// Lazily construct the default inline process driver on first
    /// `session().open()`. A store factory is required to build the config (the
    /// worker rebuilds a session runtime per process); a registry with no store
    /// factory is rejected at build with
    /// [`EmbedError::ProcessRegistryRequiresStoreFactory`].
    LazyDefault {
        config: Box<DurableProcessWorkerConfig>,
    },
    /// The host wired an external driver.
    External { driver: ProcessWorkDriver },
}

#[derive(Clone, Default)]
pub(super) enum ProcessWorkSource {
    #[default]
    None,
    Inline {
        registry: Arc<dyn ProcessRegistry>,
        hub: Option<facade_support::ProcessChangeHub>,
    },
    External(ProcessWorkDriver),
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
            Self::External(driver) => Some(driver.process_registry()),
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
            // An external driver was wrapped by its host, which installs any
            // sink through the driver constructor; the inline sink does not
            // apply here. Already-watched inline sources keep their wrap.
            other => other,
        }
    }
}

#[derive(Clone, Default)]
pub(super) enum QueuedWorkSource {
    None,
    #[default]
    LazyDefault,
    External(QueuedWorkDriver),
}

pub(super) enum QueuedWorkDriverSetup {
    None,
    LazyDefault {
        config: Arc<InlineQueuedWorkRunConfig>,
        slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        execution_concurrency: usize,
    },
    External {
        driver: QueuedWorkDriver,
    },
}

pub(super) struct WakeDeliveryDriverSetup {
    pub(super) registry: Arc<dyn ProcessRegistry>,
    pub(super) factory: Arc<dyn SessionStoreFactory>,
    pub(super) clock: Arc<dyn lash_core::Clock>,
    pub(super) delivery_policy: lash_core::DeliveryPolicy,
}

pub(super) struct InlineWorkDriverSetup {
    pub(super) process: ProcessWorkDriverSetup,
    pub(super) queued: QueuedWorkDriverSetup,
    pub(super) wake: Option<WakeDeliveryDriverSetup>,
}

#[derive(Clone, Default)]
pub(crate) struct ResolvedWorkDrivers {
    pub(crate) process: Option<ProcessWorkDriver>,
    pub(crate) queued: Option<QueuedWorkDriver>,
    pub(crate) _wake: Option<facade_support::WakeDeliveryDriver>,
    pub(crate) drive_process_on_open: bool,
}

/// Shared, lazily-initialized host-work state for a [`LashCore`].
///
/// The once-guard ([`tokio::sync::OnceCell`]) constructs inline drivers exactly
/// once across `LashCore` clones, on the first `session().open()` or admin path
/// that needs them.
pub(crate) struct InlineWorkDriverSlot {
    setup: InlineWorkDriverSetup,
    drivers: tokio::sync::OnceCell<ResolvedWorkDrivers>,
    phase_probe_slot: Option<lash_core::runtime::RuntimeTurnPhaseProbeSlot>,
}

impl InlineWorkDriverSlot {
    pub(super) fn new(setup: InlineWorkDriverSetup) -> Self {
        let phase_probe_slot = match &setup.process {
            ProcessWorkDriverSetup::LazyDefault { config } => {
                Some(config.turn_phase_probe_slot.clone())
            }
            ProcessWorkDriverSetup::None | ProcessWorkDriverSetup::External { .. } => None,
        };
        Self {
            setup,
            drivers: tokio::sync::OnceCell::new(),
            phase_probe_slot,
        }
    }

    /// Resolve host work drivers for a session host. Idempotent: the once-guard
    /// ensures inline drivers are constructed once.
    pub(crate) async fn drivers(&self) -> ResolvedWorkDrivers {
        self.drivers
            .get_or_init(|| async {
                let queued = match &self.setup.queued {
                    QueuedWorkDriverSetup::None => None,
                    QueuedWorkDriverSetup::External { driver } => Some(driver.clone()),
                    QueuedWorkDriverSetup::LazyDefault {
                        config,
                        slot_supplier,
                        execution_concurrency,
                    } => {
                        let run_handle =
                            Arc::new(InlineQueuedWorkRunHandle::new(Arc::clone(config)));
                        Some(match slot_supplier {
                            Some(slot_supplier) => QueuedWorkDriver::with_worker_slot_supplier(
                                run_handle,
                                Arc::clone(slot_supplier),
                            ),
                            None => QueuedWorkDriver::with_execution_concurrency(
                                run_handle,
                                *execution_concurrency,
                            )
                            .expect("queued-work concurrency was validated at build"),
                        })
                    }
                };
                let (process, drive_process_on_open) = match &self.setup.process {
                    ProcessWorkDriverSetup::None => (None, false),
                    ProcessWorkDriverSetup::External { driver } => (Some(driver.clone()), false),
                    ProcessWorkDriverSetup::LazyDefault { config } => {
                        let mut config = (**config).clone();
                        if let Some(driver) = queued.clone() {
                            config = config.with_queued_work_driver(driver);
                        }
                        let registry = Arc::clone(&config.process_registry);
                        let hub = config.process_change_hub.clone();
                        let worker = DurableProcessWorker::new(config);
                        let driver = if let Some(hub) = hub {
                            ProcessWorkDriver::from_watched(
                                registry,
                                hub,
                                Arc::new(facade_support::InlineProcessRunHandle::new(worker)),
                            )
                        } else {
                            ProcessWorkDriver::inline(registry, worker)
                        };
                        (Some(driver), true)
                    }
                };
                let wake = self.setup.wake.as_ref().map(|setup| {
                    facade_support::WakeDeliveryDriver::new(
                        Arc::clone(&setup.registry),
                        Arc::clone(&setup.factory),
                        queued.clone(),
                        Arc::clone(&setup.clock),
                        setup.delivery_policy,
                    )
                });
                ResolvedWorkDrivers {
                    process,
                    queued,
                    _wake: wake,
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
            ProcessWorkDriverSetup::LazyDefault { config } => Some(config),
            ProcessWorkDriverSetup::None | ProcessWorkDriverSetup::External { .. } => None,
        }
    }

    pub(super) fn configured_process_work_driver(&self) -> Option<ProcessWorkDriver> {
        match &self.setup.process {
            ProcessWorkDriverSetup::External { driver } => Some(driver.clone()),
            ProcessWorkDriverSetup::None | ProcessWorkDriverSetup::LazyDefault { .. } => None,
        }
    }

    pub(super) fn configured_queued_work_driver(&self) -> Option<QueuedWorkDriver> {
        match &self.setup.queued {
            QueuedWorkDriverSetup::External { driver } => Some(driver.clone()),
            QueuedWorkDriverSetup::None | QueuedWorkDriverSetup::LazyDefault { .. } => None,
        }
    }
}

impl Drop for InlineWorkDriverSlot {
    fn drop(&mut self) {
        if let Some(drivers) = self.drivers.get()
            && let Some(wake) = drivers._wake.as_ref()
        {
            wake.request_shutdown();
        }
    }
}
