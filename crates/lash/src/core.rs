use crate::support::*;
use lash_core::facade_support;
use lash_core::facade_support::ScopedEffectControllerFacadeOps;
use lash_core::runtime::{
    ProcessCommand, ProcessEffectOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimeScope,
};
use std::collections::HashSet;

mod drain;
mod queued_work;
mod runtime_host_config;
mod worker_capacity;

pub use drain::DeploymentDrainStatus;
use queued_work::{InlineQueuedWorkRunConfig, InlineQueuedWorkRunHandle};
#[derive(Clone)]
pub struct LashCore {
    pub(crate) session_execution_owner: lash_core::LeaseOwnerIdentity,
    pub(crate) env: RuntimeEnvironment,
    pub(crate) tool_registry: Arc<lash_core::ToolRegistry>,
    pub(crate) policy: SessionPolicy,
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    pub(crate) store_factory: Option<Arc<dyn SessionStoreFactory>>,
    pub(crate) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(crate) provider: Option<ProviderHandle>,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
    /// Whether process lifecycle is available; threaded into rebuilt session plugin hosts.
    pub(crate) process_lifecycle_available: bool,
    pub(crate) process_execution_concurrency: usize,
    /// Explicit host supplier; `None` preserves a fresh bound per process worker.
    pub(crate) worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    /// Shared across core clones so inline drivers are constructed at most once.
    pub(crate) work_driver: Arc<InlineWorkDriverSlot>,
    /// Store-less session ids rejected for reuse by this core.
    pub(crate) ephemeral_session_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    pub(crate) tool_intent_submission_gates:
        Arc<crate::tool_intent_ingress::RuntimeSubmissionGates>,
}

/// How a [`LashCore`] resolves its process work driver, decided at `build()`
/// and shared across clones.
pub(crate) enum ProcessWorkDriverSetup {
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
pub(crate) enum ProcessWorkSource {
    #[default]
    None,
    Inline {
        registry: Arc<dyn ProcessRegistry>,
        hub: Option<facade_support::ProcessChangeHub>,
    },
    External(ProcessWorkDriver),
}

impl ProcessWorkSource {
    fn with_runtime_clock(self, clock: Arc<dyn lash_core::Clock>) -> Self {
        match self {
            Self::Inline { registry, hub } => Self::Inline {
                registry: registry.with_runtime_clock(clock).unwrap_or(registry),
                hub,
            },
            other => other,
        }
    }

    fn process_registry(&self) -> Option<Arc<dyn ProcessRegistry>> {
        match self {
            Self::None => None,
            Self::Inline { registry, .. } => Some(Arc::clone(registry)),
            Self::External(driver) => Some(driver.process_registry()),
        }
    }

    fn has_registry(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn watched(self, sink: Option<Arc<dyn facade_support::ProcessEventSink>>) -> Self {
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
pub(crate) enum QueuedWorkSource {
    None,
    #[default]
    LazyDefault,
    External(QueuedWorkDriver),
}

pub(crate) enum QueuedWorkDriverSetup {
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

pub(crate) struct WakeDeliveryDriverSetup {
    registry: Arc<dyn ProcessRegistry>,
    factory: Arc<dyn SessionStoreFactory>,
    clock: Arc<dyn lash_core::Clock>,
    delivery_policy: lash_core::DeliveryPolicy,
}

pub(crate) struct InlineWorkDriverSetup {
    process: ProcessWorkDriverSetup,
    queued: QueuedWorkDriverSetup,
    wake: Option<WakeDeliveryDriverSetup>,
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
    fn new(setup: InlineWorkDriverSetup) -> Self {
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

    fn configured_process_work_driver(&self) -> Option<ProcessWorkDriver> {
        match &self.setup.process {
            ProcessWorkDriverSetup::External { driver } => Some(driver.clone()),
            ProcessWorkDriverSetup::None | ProcessWorkDriverSetup::LazyDefault { .. } => None,
        }
    }

    fn configured_queued_work_driver(&self) -> Option<QueuedWorkDriver> {
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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct SessionDeleteReport {
    pub session_id: String,
    pub process: Option<lash_core::ProcessSessionDeleteReport>,
}

impl LashCore {
    pub fn builder(turn_budget: lash_core::TurnBudget) -> LashCoreBuilder {
        LashCoreBuilder::new(turn_budget)
    }

    /// Sugar entry point: a [`LashCoreBuilder`] pre-seeded with the standard
    /// protocol plugin and the default runtime plugin stack.
    pub fn standard_builder(turn_budget: lash_core::TurnBudget) -> LashCoreBuilder {
        LashCore::builder(turn_budget)
            .protocol_plugin(Arc::new(
                lash_protocol_standard::StandardProtocolPluginFactory::new(),
            ))
            .plugins(default_runtime_stack())
    }

    /// Read whether this deployment is safe for a host to retire.
    ///
    /// The host owns admission and must pass its current admission state. Lash
    /// reads the process registry on demand; it does not maintain a counter or
    /// orchestrate routing, deadlines, worker shutdown, or retirement. A core
    /// without a process registry reports zero remaining invocations.
    pub async fn drain_status(&self, accepting_new_work: bool) -> Result<DeploymentDrainStatus> {
        let remaining_invocations = match self.process_registry() {
            Some(registry) => registry.count_non_terminal_processes().await?,
            None => 0,
        };
        let checked_at = self.env.core.clock.timestamp_ms();
        Ok(DeploymentDrainStatus {
            accepting_new_work,
            remaining_invocations,
            checked_at,
            drained: !accepting_new_work && remaining_invocations == 0,
        })
    }

    /// Sugar entry point: a [`LashCoreBuilder`] pre-seeded with a
    /// host-configured RLM protocol factory and the default runtime plugin
    /// stack.
    ///
    /// The host configures the factory (projection resolver, deferred tool
    /// resolver, execution sink/jsonl path, and — required at construction — the
    /// Lashlang artifact store) before passing it in.
    #[cfg(feature = "rlm")]
    pub fn rlm_builder(
        turn_budget: lash_core::TurnBudget,
        factory: crate::rlm::RlmProtocolPluginFactory,
    ) -> LashCoreBuilder {
        LashCore::builder(turn_budget)
            .protocol_plugin(Arc::new(factory))
            .plugins(default_runtime_stack())
    }

    pub fn session(&self, session_id: impl Into<String>) -> SessionBuilder {
        SessionBuilder {
            core: self.clone(),
            session_id: session_id.into(),
            spec: SessionSpec::inherit(),
            parent_session_id: None,
            store: None,
            provider: None,
            active_plugins: Vec::new(),
            plugin_factories: Vec::new(),
            plugin_options: PluginOptions::default(),
        }
    }

    /// Shut down registered plugin factories after the host has stopped intake.
    ///
    /// This method releases plugin-factory resources; it does not stop intake,
    /// drain active turns, abort work, or orchestrate host shutdown. The host
    /// owns those steps and must call `shutdown` only after no new work can enter.
    /// The protocol factory is visited first, followed by common factories in
    /// configured order. These factories own disjoint resources, so the order
    /// carries no dependency semantics; it is fixed only for determinism and log
    /// auditability. A host that shares a resource across factories must not rely
    /// on this order.
    ///
    /// Every factory is visited even after failures. Each failure is warned and
    /// the first is returned after the walk. Implementations own their timeout
    /// policy and must make repeated shutdown calls idempotent. Factories added
    /// through [`AdvancedLashCoreBuilder::plugin_host`] are included. Extra
    /// factories supplied only to durable-process-worker configuration or to an
    /// individual session are host-owned and are not walked by this method.
    pub async fn shutdown(&self) -> Result<()> {
        let factories = self
            .protocol_factory
            .iter()
            .chain(self.plugin_factories.iter());
        let mut first_error = None;
        for factory in factories {
            let started = std::time::Instant::now();
            tracing::debug!(
                plugin_factory = factory.id(),
                "plugin factory shutdown started"
            );
            match factory.shutdown().await {
                Ok(()) => tracing::debug!(
                    plugin_factory = factory.id(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "plugin factory shutdown completed"
                ),
                Err(error) => {
                    tracing::warn!(
                        plugin_factory = factory.id(),
                        elapsed_ms = started.elapsed().as_millis(),
                        error = %error,
                        "plugin factory shutdown failed"
                    );
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(EmbedError::Plugin(error)),
            None => Ok(()),
        }
    }

    /// Report whether `session_id` has durable live session metadata.
    ///
    /// This is a cheap existence read: it does not create, hydrate, or open the
    /// session. A permanently deleted session returns `false`; callers that try
    /// to recreate the id still receive the store's typed deletion error.
    pub async fn session_exists(&self, session_id: impl AsRef<str>) -> Result<bool> {
        let session_id = session_id.as_ref();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        let request = lash_core::SessionStoreCreateRequest {
            session_id: session_id.to_string(),
            relation: lash_core::SessionRelation::Root,
            policy: self.policy.clone(),
        };
        let Some(store) = store_factory
            .open_existing_store(&request)
            .await
            .map_err(|message| EmbedError::StoreFactory {
                session_id: session_id.to_string(),
                message,
            })?
        else {
            return Ok(false);
        };
        Ok(store.load_session_meta().await?.is_some())
    }

    /// Report whether the durable single-use tombstone for `session_id` exists.
    /// A `false` result means only "no tombstone"; it is not evidence that the session is live.
    /// Tombstones are monotonic: once this returns `true`, the session id cannot become live again.
    /// Compose this read with [`Self::session_exists`] when deciding live/retired/unknown disposition.
    pub async fn session_was_deleted(&self, session_id: impl AsRef<str>) -> Result<bool> {
        let session_id = session_id.as_ref();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory
            .session_was_deleted(session_id)
            .await
            .map_err(|message| EmbedError::StoreFactory {
                session_id: session_id.to_string(),
                message,
            })
    }

    /// Build the effect scope required to delete the stored session.
    pub async fn session_delete_scope(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<lash_core::ExecutionScope> {
        let session_id = session_id.as_ref();
        if !self.session_exists(session_id).await? {
            return Err(EmbedError::StoreFactory {
                session_id: session_id.to_string(),
                message: "session does not exist".to_string(),
            });
        }
        Ok(lash_core::ExecutionScope::session_delete(session_id))
    }

    /// Rebuild a live session from a [`ParkedSession`](crate::ParkedSession)
    /// handle produced by [`LashSession::park`](crate::LashSession::park).
    ///
    /// Resume reloads the flushed state from the parked store (honoring this
    /// core's residency), reinstalls this core's plugin configuration and work
    /// drivers, and returns a ready [`LashSession`]. The parked store instance
    /// is reused directly, so the transcript the session flushed at park time is
    /// visible again after resume.
    ///
    /// This restores the core-level plugin stack. Session-specific plugins added
    /// per open via [`SessionBuilder::plugin`] are not re-applied here; parking
    /// is the round-trip for the core's own configuration.
    pub async fn resume(&self, parked: ParkedSession) -> Result<LashSession> {
        // Build the per-session env exactly like `SessionBuilder::open_resolved`
        // (minus builder-scoped plugins): a fresh plugin host with this core's
        // factories, the shared work drivers, and the core provider resolver
        // already carried on `self.env`.
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            Vec::new(),
        )?;
        let mut env = self.env.clone();
        env.core = plugin_host.install_process_engine_contributions(
            env.core.clone(),
            self.process_lifecycle_available,
        )?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let effect_host = Arc::clone(&env.core.control.effect_host);
        let drivers = self.work_driver.drivers().await;
        env.process_work_driver = drivers.process.clone();
        env.queued_work_driver = drivers.queued.clone();
        let runtime =
            LashRuntime::resume(parked.inner, &env, self.session_execution_owner.clone()).await?;
        let handle =
            RuntimeHandle::with_live_replay_store(runtime, Arc::clone(&self.live_replay_store));
        Ok(LashSession {
            runtime: handle,
            effect_host,
            parent_session_id: None,
            active_plugins: Vec::new(),
            process_phase_probe_slot: self.work_driver.phase_probe_slot(),
            turn_cancels: crate::turn::TurnCancelRegistry::default(),
        })
    }

    /// Flush this core's configured trace sink, if any.
    ///
    /// Hosts that hand `lash` a trace sink via
    /// [`LashCoreBuilder::trace_sink`] already hold their own `Arc` and can
    /// flush it directly; this is the equivalent lever for hosts that did not
    /// retain the handle. It flushes the core's copy — for a
    /// [`JsonlTraceSink`](lash_trace::JsonlTraceSink) that fsyncs the file, and
    /// for an OTel sink it is a no-op (the host still owns provider flush; see
    /// the tracing docs). Call it before process exit alongside the host's own
    /// exporter/provider shutdown.
    pub fn flush_trace_sink(&self) -> Result<()> {
        if let Some(sink) = self.env.core.tracing.trace_sink.as_ref() {
            sink.flush()?;
        }
        Ok(())
    }

    pub fn triggers(&self) -> crate::admin::CoreTriggerAdmin {
        crate::admin::CoreTriggerAdmin { core: self.clone() }
    }

    pub fn processes(&self) -> crate::process_admin::Processes {
        crate::process_admin::Processes { core: self.clone() }
    }

    pub fn completions(&self) -> crate::admin::Completions {
        crate::admin::Completions { core: self.clone() }
    }

    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.env.core.control.effect_host)
    }

    /// Exact-turn cooperative control for this deployment's effect host.
    ///
    /// The returned driver is independently usable from any session handle.
    /// Session and turn ids are routing identity, not authorization; authorize
    /// requests in the host API before forwarding them to Lash.
    pub fn turn_work_driver(&self) -> facade_support::TurnWorkDriver {
        facade_support::TurnWorkDriver::new(self.effect_host())
    }

    /// Persist host input without opening a competing session writer.
    ///
    /// This is the downstream-host ingress seam for input submitted while a
    /// durable turn may already own the session execution lease. Active-turn
    /// input is claimed by that exact turn at a checkpoint; next-turn input is
    /// handed to the configured queued-work driver after it is durably stored.
    /// Success acknowledges durable acceptance only; queue dispatch is a
    /// separate best-effort wake and is reconciled from the pending row.
    pub async fn enqueue_turn_input(
        &self,
        session_id: impl Into<String>,
        input: lash_core::TurnInput,
        ingress: lash_core::TurnInputIngress,
        id: Option<String>,
    ) -> Result<facade_support::TurnInputAcceptanceReceipt> {
        facade_support::ensure_durable_effect_input(&input).map_err(EmbedError::Runtime)?;
        let session_id = session_id.into();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        let mut policy = self.policy.clone();
        policy.session_id = Some(session_id.clone());
        let store = store_factory
            .create_store(&SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: SessionRelation::default(),
                policy,
            })
            .await
            .map_err(EmbedError::Store)?;
        let is_next_turn = matches!(ingress, lash_core::TurnInputIngress::NextTurn);
        let mut draft = lash_core::PendingTurnInputDraft::new(session_id, ingress, input);
        draft.source_key = id.map(|id| format!("host:{id}"));
        let enqueued = store
            .enqueue_pending_turn_input(draft)
            .await
            .map_err(|err| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                ))
            })?;
        if is_next_turn && let Some(driver) = self.work_driver.drivers().await.queued.as_ref() {
            driver.notify_pending_work(Some(&enqueued.session_id), "queued_turn_input");
        }
        Ok(facade_support::TurnInputAcceptanceReceipt::from(&enqueued))
    }

    /// Retain the current continuation checkpoint for a turn-boundary node.
    ///
    /// A point must still be retained when this is called: ordinarily that
    /// means it is the leaf of a live session. Pin before advancing the head if
    /// a host wants to make a past turn forkable later.
    pub async fn pin(&self, node_id: impl AsRef<str>) -> Result<lash_core::ForkPoint> {
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory
            .pin(node_id.as_ref())
            .await
            .map_err(Into::into)
    }

    /// Release an explicit continuation pin. A live tip at the same node
    /// remains forkable through its session-head checkpoint.
    pub async fn unpin(&self, node_id: impl AsRef<str>) -> Result<()> {
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory
            .unpin(node_id.as_ref())
            .await
            .map_err(Into::into)
    }

    /// Enumerate pinned past turns and unpinned live tips that can be forked.
    pub async fn fork_points(&self) -> Result<Vec<lash_core::ForkPoint>> {
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory.fork_points().await.map_err(Into::into)
    }

    /// Create `session_id` at a retained turn boundary without writing graph
    /// nodes.
    ///
    /// Unpinned past turns are ordinarily not retained. That normal outcome is
    /// returned as
    /// `EmbedError::Store(StoreError::ForkPointNotRetained { .. })`; Lash never
    /// silently substitutes a different checkpoint. An explicit pin remains
    /// forkable after its source session is deleted because the retained frame
    /// carries the provider and model needed to create the branch.
    pub async fn fork_at(
        &self,
        node_id: impl Into<String>,
        session_id: impl Into<String>,
    ) -> Result<lash_core::ForkSessionResult> {
        self.fork_at_with_observer_inheritance(
            node_id,
            session_id,
            lash_core::ObserverInheritance::All,
        )
        .await
    }

    pub async fn fork_at_with_observer_inheritance(
        &self,
        node_id: impl Into<String>,
        session_id: impl Into<String>,
        observer_inheritance: lash_core::ObserverInheritance,
    ) -> Result<lash_core::ForkSessionResult> {
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        let node_id = node_id.into();
        let session_id = session_id.into();
        let point = store_factory
            .fork_points()
            .await?
            .into_iter()
            .find(|point| point.node_id == node_id)
            .ok_or_else(|| lash_core::StoreError::ForkPointNotRetained {
                node_id: node_id.clone(),
            })?;
        let inherited = match (&observer_inheritance, self.process_registry()) {
            (lash_core::ObserverInheritance::None, _) | (_, None) => Vec::new(),
            (lash_core::ObserverInheritance::All, Some(process_registry)) => process_registry
                .list_observed_by(&point.source_session_id)
                .await?
                .into_iter()
                .map(|record| record.id)
                .collect(),
            (lash_core::ObserverInheritance::Only(ids), Some(process_registry)) => {
                let observed = process_registry
                    .list_observed_by(&point.source_session_id)
                    .await?
                    .into_iter()
                    .map(|record| record.id)
                    .collect::<std::collections::HashSet<_>>();
                ids.iter()
                    .filter(|id| observed.contains(*id))
                    .cloned()
                    .collect()
            }
        };
        let resolved_observer_inheritance = match observer_inheritance {
            lash_core::ObserverInheritance::Only(_) => {
                lash_core::ObserverInheritance::Only(inherited.clone())
            }
            selector => selector,
        };
        let mut fork_policy = self.policy.clone();
        fork_policy.provider_id = point.config.provider_id;
        fork_policy.model = point.config.model;
        let request = lash_core::ForkSessionRequest {
            session_id,
            node_id,
            relation: lash_core::SessionRelation::Fork {
                source_session_id: point.source_session_id,
                source_node_id: point.node_id,
                observer_inheritance: resolved_observer_inheritance,
                pending_observer_process_ids: inherited.clone(),
            },
            policy: fork_policy,
        };
        let fork = store_factory.fork_at(&request).await?;
        let create_request = lash_core::SessionStoreCreateRequest {
            session_id: request.session_id,
            relation: request.relation,
            policy: request.policy,
        };
        let branch_store = store_factory
            .open_existing_store(&create_request)
            .await
            .map_err(|error| {
                lash_core::StoreError::Backend(format!(
                    "failed to reopen fork store `{}`: {error}",
                    create_request.session_id
                ))
            })?
            .ok_or_else(|| {
                lash_core::StoreError::Backend(format!(
                    "fork store `{}` disappeared before observer publication completed",
                    create_request.session_id
                ))
            })?;
        lash_core::runtime::reconcile_session_process_observer_intents(
            self.process_registry().as_deref(),
            &fork.session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(branch_store.as_ref()),
        )
        .await?;
        Ok(fork)
    }

    pub async fn delete_session(
        &self,
        session_id: impl AsRef<str>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SessionDeleteReport> {
        let session_id = session_id.as_ref().to_string();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        match scoped_effect_controller.execution_scope() {
            lash_core::ExecutionScope::SessionDelete {
                session_id: scoped_session_id,
            } if scoped_session_id == &session_id => {}
            _ => {
                return Err(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::SessionDeleteScopeMismatch,
                    "session deletion requires a matching SessionDelete scope",
                )
                .into());
            }
        }
        let process = if let Some(process_registry) = self.env.process_registry.as_ref() {
            let invocation = RuntimeInvocation::effect(
                RuntimeScope::new(session_id.clone()),
                format!("process:delete-session:{session_id}"),
                RuntimeEffectKind::Process,
                format!("{session_id}:delete-session"),
            );
            let outcome = scoped_effect_controller
                .controller()
                .execute_effect(
                    RuntimeEffectEnvelope::new(
                        invocation,
                        RuntimeEffectCommand::process(ProcessCommand::DeleteSession {
                            session_id: session_id.clone(),
                        }),
                    ),
                    RuntimeEffectLocalExecutor::processes(
                        Arc::clone(process_registry),
                        self.env.process_work_driver.clone(),
                    ),
                )
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
            match outcome {
                RuntimeEffectOutcome::Process {
                    result: ProcessEffectOutcome::DeleteSession { report },
                } => Some(report),
                other => {
                    return Err(EmbedError::SessionDeleteProcess {
                        session_id,
                        message: format!(
                            "process delete returned the wrong outcome: {}",
                            other.kind().as_str()
                        ),
                    });
                }
            }
        } else {
            None
        };
        if let Some(trigger_store) = self.env.trigger_store.as_ref() {
            trigger_store
                .delete_session_subscriptions(&session_id)
                .await
                .map_err(|err| EmbedError::SessionDeleteProcess {
                    session_id: session_id.clone(),
                    message: err.to_string(),
                })?;
        }
        self.env
            .core
            .control
            .effect_host
            .revoke_await_events_for_session(&session_id)
            .await
            .map_err(|err| EmbedError::SessionDeleteProcess {
                session_id: session_id.clone(),
                message: err.to_string(),
            })?;
        store_factory
            .delete_session(&session_id)
            .await
            .map_err(|message| EmbedError::StoreFactory {
                session_id: session_id.clone(),
                message,
            })?;
        self.env
            .core
            .control
            .effect_host
            .retire_effect_journal(lash_core::EffectJournalRetirement::session(&session_id))
            .await
            .map_err(|err| EmbedError::SessionDeleteProcess {
                session_id: session_id.clone(),
                message: err.to_string(),
            })?;
        Ok(SessionDeleteReport {
            session_id,
            process,
        })
    }

    pub fn process_registry(&self) -> Option<Arc<dyn ProcessRegistry>> {
        self.env.process_registry.as_ref().cloned()
    }

    pub fn durable_process_worker_config(&self) -> Result<DurableProcessWorkerConfig> {
        self.durable_process_worker_config_with_plugins(std::iter::empty::<Arc<dyn PluginFactory>>())
    }

    pub fn durable_process_worker_config_with_plugins(
        &self,
        extra_plugin_factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
    ) -> Result<DurableProcessWorkerConfig> {
        let Some(process_registry) = self.process_registry() else {
            return Err(EmbedError::MissingProcessRegistry);
        };
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingProcessWorkerStoreFactory);
        };
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            extra_plugin_factories.into_iter().collect(),
        )?;
        let runtime_host = plugin_host.install_process_engine_contributions(
            self.env.core.clone(),
            self.process_lifecycle_available,
        )?;
        let mut config = DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            Arc::clone(store_factory),
            process_registry,
            self.session_execution_owner.clone(),
        )
        .with_session_policy(self.policy.clone())
        .with_process_execution_concurrency(self.process_execution_concurrency)?;
        if let Some(worker_slot_supplier) = self.worker_slot_supplier.as_ref() {
            config = config.with_worker_slot_supplier(Arc::clone(worker_slot_supplier));
        }
        if let Some(trigger_store) = self.env.trigger_store.as_ref() {
            config = config.with_trigger_store(Arc::clone(trigger_store));
        }
        if let Some(driver) = self.work_driver.configured_process_work_driver() {
            config = config
                .with_change_hub(driver.change_hub())
                .with_process_work_driver(driver);
        }
        if let Some(driver) = self.work_driver.configured_queued_work_driver() {
            config = config.with_queued_work_driver(driver);
        }
        Ok(config)
    }
}

fn default_runtime_stack() -> PluginStack {
    lash_plugin_tool_output_budget::tool_output_budget_stack()
}

pub struct LashCoreBuilder {
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    session_spec: SessionSpec,
    provider: Option<ProviderHandle>,
    pub(crate) store_factory: Option<Arc<dyn SessionStoreFactory>>,
    child_store_factory: Option<Arc<dyn SessionStoreFactory>>,
    // `RuntimeHostConfig` has no `Default`: the generic host-owned durability
    // dependencies must be named. They are collected here and resolved in
    // `build()`, which errors if any is unset.
    effect_host: Option<Arc<dyn EffectHost>>,
    attachment_store: Option<Arc<dyn AttachmentStore>>,
    process_env_store: Option<Arc<dyn ProcessExecutionEnvStore>>,
    commit_budget: Option<facade_support::CommitBudget>,
    queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
    process_wake_delivery_policy: Option<lash_core::DeliveryPolicy>,
    trigger_store: Option<Arc<dyn lash_core::TriggerStore>>,
    // Benign core overrides applied on top of the resolved core.
    prompt: Option<PromptLayer>,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    trace_level: Option<lash_trace::TraceLevel>,
    trace_context: Option<lash_trace::TraceContext>,
    termination: Option<TerminationPolicy>,
    // Advanced full-config override; used as the base core when present.
    runtime_host_config: Option<RuntimeHostConfig>,
    tool_providers: Vec<Arc<dyn ToolProvider>>,
    plugin_stack: PluginStack,
    plugin_host: Option<PluginHost>,
    lease_timings: Option<facade_support::LeaseTimings>,
    clock: Option<Arc<dyn lash_core::Clock>>,
    // Single source of truth for process lifecycle support and process-work
    // consumption.
    process_work_source: ProcessWorkSource,
    // Per-worker bound for the default inline process executor.
    process_execution_concurrency: Option<usize>,
    // Per-driver bound for the default inline queued-work executor.
    queued_work_execution_concurrency: Option<usize>,
    // Optional host admission controller replacing both fixed worker lanes.
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    // Optional host-facing best-effort feed of appended process events,
    // installed on the inline process-registry decorator at build time.
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    process_tool_visibility_filter: Option<Arc<dyn facade_support::ProcessToolVisibilityFilter>>,
    queued_work_source: QueuedWorkSource,
    live_replay_store: Option<Arc<dyn LiveReplayStore>>,
}

impl LashCoreBuilder {
    fn new(turn_budget: lash_core::TurnBudget) -> Self {
        Self {
            protocol_factory: None,
            session_spec: SessionSpec::new().turn_budget(turn_budget),
            provider: None,
            store_factory: None,
            child_store_factory: None,
            effect_host: None,
            attachment_store: None,
            process_env_store: None,
            commit_budget: None,
            queued_work_batching: None,
            process_wake_delivery_policy: None,
            trigger_store: None,
            prompt: None,
            trace_sink: None,
            trace_level: None,
            trace_context: None,
            termination: None,
            runtime_host_config: None,
            tool_providers: Vec::new(),
            plugin_stack: PluginStack::default(),
            plugin_host: None,
            lease_timings: None,
            clock: None,
            process_work_source: ProcessWorkSource::default(),
            process_execution_concurrency: None,
            queued_work_execution_concurrency: None,
            worker_slot_supplier: None,
            process_event_sink: None,
            process_tool_visibility_filter: None,
            queued_work_source: QueuedWorkSource::default(),
            live_replay_store: None,
        }
    }

    pub fn protocol_plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.protocol_factory = Some(plugin);
        self
    }

    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.session_spec = self.session_spec.provider_id(provider.kind());
        self.provider = Some(provider);
        self
    }

    pub fn model(mut self, model: lash_core::ModelSpec) -> Self {
        self.session_spec = self.session_spec.model(model);
        self
    }

    pub fn turn_budget(mut self, turn_budget: lash_core::TurnBudget) -> Self {
        self.session_spec = self.session_spec.turn_budget(turn_budget);
        self
    }

    /// Generation options — output token cap, temperature, seed — carried by
    /// every LLM call in every session this core opens.
    pub fn generation(mut self, generation: lash_core::GenerationOptions) -> Self {
        self.session_spec = self.session_spec.generation(generation);
        self
    }

    pub fn session_spec(mut self, spec: SessionSpec) -> Self {
        self.session_spec = spec;
        self
    }

    /// Configure a factory that can create a persistence store for any root
    /// session opened from this core.
    ///
    /// The factory must honor `SessionStoreCreateRequest::session_id` and
    /// return a store for that specific session. Do not use this to wrap one
    /// pre-opened root store; pass root-only stores with
    /// `LashCore::session(...).store(store)` instead.
    ///
    /// Durable attachment GC never guesses process-registry co-location. Hosts
    /// using owner-aware GC must explicitly call
    /// `SqliteSessionStoreFactory::new_with_process_registry(...)` or
    /// `PostgresStorage::session_store_factory_with_shared_process_registry()`
    /// on the concrete factory before erasing it behind this trait object.
    pub fn store_factory(mut self, store_factory: Arc<dyn SessionStoreFactory>) -> Self {
        self.store_factory = Some(store_factory);
        self
    }

    /// Configure the persistence factory used by managed child sessions, such
    /// as local subagents.
    ///
    /// Child factories must return a distinct store bound to the requested
    /// child session id. Hosts that pass an explicit root store with
    /// `SessionBuilder::store` should set this when child sessions need
    /// persistence.
    /// The same explicit process-registry wiring required by `store_factory`
    /// applies when this factory participates in attachment GC.
    pub fn child_store_factory(mut self, store_factory: Arc<dyn SessionStoreFactory>) -> Self {
        self.child_store_factory = Some(store_factory);
        self
    }

    pub fn attachment_store(mut self, attachment_store: Arc<dyn AttachmentStore>) -> Self {
        self.attachment_store = Some(attachment_store);
        self
    }

    pub fn process_env_store(
        mut self,
        process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    ) -> Self {
        self.process_env_store = Some(process_env_store);
        self
    }

    /// Configure the byte and graph-node limits for each atomic runtime
    /// commit. Hosts must choose bounded or unbounded behavior explicitly for
    /// both dimensions.
    pub fn commit_budget(mut self, commit_budget: facade_support::CommitBudget) -> Self {
        self.commit_budget = Some(commit_budget);
        self
    }

    /// Configure queued-work batching with a required model-action reserve.
    /// Row-count and pending-age bounds default inside the supplied value and
    /// may be overridden by the host.
    pub fn queued_work_batching(
        mut self,
        policy: facade_support::QueuedWorkBatchingConfig,
    ) -> Self {
        self.queued_work_batching = Some(policy);
        self
    }

    /// Select when process wakes may enter an active target session.
    pub fn process_wake_delivery_policy(mut self, policy: lash_core::DeliveryPolicy) -> Self {
        self.process_wake_delivery_policy = Some(policy);
        self
    }

    /// Install a synchronous, in-process, narrow-only filter for the
    /// model-facing session process tools.
    pub fn process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn facade_support::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.process_tool_visibility_filter = Some(filter);
        self
    }

    /// Set the deployment effect host — the durability boundary every operation
    /// crosses. Pass [`InlineEffectHost`](crate::durability::InlineEffectHost)
    /// for in-process execution, or a workflow-backed host for durable
    /// execution.
    pub fn effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = Some(effect_host);
        self
    }

    pub fn tools(mut self, tools: Arc<dyn ToolProvider>) -> Self {
        self.tool_providers.push(tools);
        self
    }

    pub fn plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.plugin_stack.push(plugin);
        self
    }

    pub fn plugins(mut self, stack: PluginStack) -> Self {
        self.plugin_stack = stack;
        self
    }

    pub fn configure_plugins(mut self, configure: impl FnOnce(&mut PluginStack)) -> Self {
        configure(&mut self.plugin_stack);
        self
    }

    pub fn trace_sink(mut self, trace_sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace_sink = Some(trace_sink);
        self
    }

    pub fn trace_jsonl_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path.into())));
        self
    }

    pub fn trace_level(mut self, trace_level: lash_trace::TraceLevel) -> Self {
        self.trace_level = Some(trace_level);
        self
    }

    pub fn trace_context(mut self, trace_context: lash_trace::TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    pub fn termination(mut self, termination: TerminationPolicy) -> Self {
        self.termination = Some(termination);
        self
    }

    /// Configure the lease timing capability for every durable single-writer
    /// lane this deployment claims: session execution leases, turn-input and
    /// queued-work claims, and process leases.
    ///
    /// This is the failover-latency vs false-takeover-risk knob. Like
    /// [`process_execution_concurrency`](Self::process_execution_concurrency)
    /// it is an operational deployment decision, so it lives on the main
    /// builder tier rather than behind
    /// [`advanced`](Self::advanced). Construct the value with
    /// [`LeaseTimings::new`](facade_support::LeaseTimings::new), which enforces
    /// `ttl >= 3 * renew_interval`. Effect hosts accept the same type at
    /// construction (e.g. SQLite/Postgres effect-replay options), so a host can
    /// share one timing decision across both boundaries.
    pub fn lease_timings(mut self, lease_timings: facade_support::LeaseTimings) -> Self {
        self.lease_timings = Some(lease_timings);
        self
    }

    /// Use one host clock for runtime sleeps and embedded-store time.
    pub fn clock(mut self, clock: Arc<dyn lash_core::Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Configure the bounded live replay buffer used by session observation
    /// cursors. This is best-effort reconnect recovery only; durable state
    /// still comes from the session store and [`SessionReadView`].
    pub fn live_replay_store(mut self, live_replay_store: Arc<dyn LiveReplayStore>) -> Self {
        self.live_replay_store = Some(live_replay_store);
        self
    }

    /// Build a core under the host's stable worker/process lease identity.
    ///
    /// The owner id is stable for the worker or process and never scoped to a
    /// turn. The incarnation id changes once per process boot.
    pub fn build(
        mut self,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
    ) -> Result<LashCore> {
        let process_execution_concurrency = self
            .process_execution_concurrency
            .unwrap_or(facade_support::DEFAULT_PROCESS_EXECUTION_CONCURRENCY);
        DurableProcessWorkerConfig::validate_process_execution_concurrency(
            process_execution_concurrency,
        )?;
        let queued_work_execution_concurrency = self
            .queued_work_execution_concurrency
            .unwrap_or(facade_support::DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY);
        facade_support::QueuedWorkDriver::validate_execution_concurrency(
            queued_work_execution_concurrency,
        )?;
        let worker_slot_supplier = self.worker_slot_supplier.clone();
        let protocol_factory = self.protocol_factory.clone();
        if protocol_factory.is_none() && self.plugin_host.is_none() {
            return Err(EmbedError::MissingProtocolPlugin);
        }
        let provider_id = self
            .session_spec
            .provider_id
            .clone()
            .or_else(|| {
                self.provider
                    .as_ref()
                    .map(|provider| provider.kind().to_string())
            })
            .unwrap_or_default();
        let model = self
            .session_spec
            .model
            .clone()
            .ok_or(EmbedError::MissingModelSpec)?;
        let turn_budget = self
            .session_spec
            .turn_budget
            .ok_or(EmbedError::MissingTurnBudget)?;

        let base_policy = SessionPolicy {
            provider_id,
            model,
            ..SessionPolicy::new(turn_budget)
        };
        let policy = self.session_spec.resolve_against(&base_policy);

        let mut core = self.resolve_runtime_host_config()?;
        let process_work_source = self
            .process_work_source
            .clone()
            .with_runtime_clock(Arc::clone(&core.clock))
            .watched(self.process_event_sink.clone());
        if let Some(provider) = self.provider.clone() {
            core.providers.provider_resolver =
                Arc::new(facade_support::SingleProviderResolver::new(provider));
        }
        let plugin_factories = if let Some(plugin_host) = self.plugin_host {
            plugin_host.factories().to_vec()
        } else {
            let mut factories = Vec::new();
            if !self.tool_providers.is_empty() {
                let spec = self
                    .tool_providers
                    .into_iter()
                    .fold(PluginSpec::new(), PluginSpec::with_tool_provider);
                factories.push(Arc::new(StaticPluginFactory::new("embed_tools", spec))
                    as Arc<dyn PluginFactory>);
            }
            factories.extend(self.plugin_stack.into_factories());
            factories
        };
        let default_plugin_host =
            build_plugin_host(protocol_factory.as_ref(), &plugin_factories, Vec::new())?;
        // Whether process lifecycle is available (a process registry is wired).
        // Threaded to every plugin host so core installs the same
        // plugin-contributed process engines wherever it rebuilds a runtime.
        let process_lifecycle_available = process_work_source.has_registry();
        // Install onto a throwaway clone to validate unique engine kinds.
        // Runtime-construction sites install the contributions again onto their
        // own clean registries.
        let _ = default_plugin_host
            .install_process_engine_contributions(core.clone(), process_lifecycle_available)?;
        let tool_registry =
            lash_core::facade_support::build_core_tool_registry(&default_plugin_host)?;

        let process_registry = process_work_source.process_registry();

        // Resolve process work before the process source is moved into the
        // environment. The default inline driver's config is built
        // eagerly so a missing store factory fails loudly at build, not at
        // first open. It is built from the same single-protocol plugin host the
        // live runtime uses, so the worker can rebuild a runtime for a process.
        let process_work_driver = Self::resolve_process_work_driver(
            &process_work_source,
            &default_plugin_host,
            &core,
            process_lifecycle_available,
            // The worker rebuilds sessions with the same factory `build()` wires
            // below: `child_store_factory.or(store_factory)`.
            self.child_store_factory
                .as_ref()
                .or(self.store_factory.as_ref()),
            &policy,
            self.trigger_store.as_ref(),
            process_execution_concurrency,
            worker_slot_supplier.clone(),
            session_execution_owner.clone(),
        )?;

        let live_replay_clock = Arc::clone(&core.clock);
        let mut env_builder = RuntimeEnvironment::builder(
            core.durability.commit_budget,
            core.durability.queued_work_batching,
        )
        .with_plugin_host(Arc::new(default_plugin_host))
        .with_runtime_host_config(core);
        if let Some(process_registry) = process_registry.as_ref() {
            env_builder = env_builder.with_process_registry(Arc::clone(process_registry));
        }
        if let Some(child_store_factory) = self
            .child_store_factory
            .as_ref()
            .or(self.store_factory.as_ref())
        {
            env_builder = env_builder.with_session_store_factory(Arc::clone(child_store_factory));
        }
        if let Some(trigger_store) = self.trigger_store.as_ref() {
            env_builder = env_builder.with_trigger_store(Arc::clone(trigger_store));
        }
        let live_replay_store = self.live_replay_store.take().unwrap_or_else(|| {
            Arc::new(InMemoryLiveReplayStore::with_clock(
                facade_support::InMemoryLiveReplayStoreConfig::default(),
                live_replay_clock,
            ))
        });
        let env = env_builder.build();
        let queued_work_driver = Self::resolve_queued_work_driver(
            &self.queued_work_source,
            session_execution_owner.clone(),
            env.clone(),
            policy.clone(),
            protocol_factory.clone(),
            Arc::new(plugin_factories.clone()),
            self.child_store_factory
                .as_ref()
                .or(self.store_factory.as_ref()),
            Arc::clone(&live_replay_store),
            process_lifecycle_available,
            worker_slot_supplier.clone(),
            queued_work_execution_concurrency,
        );
        let work_driver = InlineWorkDriverSetup {
            process: process_work_driver,
            queued: queued_work_driver,
            wake: process_registry
                .clone()
                .zip(
                    self.child_store_factory
                        .as_ref()
                        .or(self.store_factory.as_ref())
                        .cloned(),
                )
                .map(|(registry, factory)| WakeDeliveryDriverSetup {
                    registry,
                    factory,
                    clock: Arc::clone(&env.core.clock),
                    delivery_policy: env.core.control.process_wake_delivery_policy,
                }),
        };

        Ok(LashCore {
            session_execution_owner,
            env,
            tool_registry,
            policy,
            store_factory: self.store_factory,
            plugin_factories: Arc::new(plugin_factories),
            provider: self.provider,
            live_replay_store,
            protocol_factory,
            process_lifecycle_available,
            process_execution_concurrency,
            worker_slot_supplier,
            work_driver: Arc::new(InlineWorkDriverSlot::new(work_driver)),
            ephemeral_session_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            tool_intent_submission_gates: Default::default(),
        })
    }

    /// Decide how a built [`LashCore`] sources its process work driver.
    ///
    /// - no registry => nothing to run ([`ProcessWorkDriverSetup::None`]);
    /// - external driver wired => use it ([`ProcessWorkDriverSetup::External`]);
    /// - inline registry wired => lazily construct the default inline driver on first open. Its
    ///   [`DurableProcessWorkerConfig`] is built eagerly when a store factory is
    ///   present; without one the inline worker cannot rebuild session runtimes.
    // Mirrors the sibling `resolve_queued_work_driver`: a builder helper whose
    // inputs are the heterogeneous, all-required driver-resolution state and
    // have no cohesive sub-grouping.
    #[allow(clippy::too_many_arguments)]
    fn resolve_process_work_driver(
        process_work_source: &ProcessWorkSource,
        worker_plugin_host: &PluginHost,
        core: &RuntimeHostConfig,
        process_lifecycle_available: bool,
        store_factory: Option<&Arc<dyn SessionStoreFactory>>,
        policy: &SessionPolicy,
        trigger_store: Option<&Arc<dyn lash_core::TriggerStore>>,
        process_execution_concurrency: usize,
        worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
    ) -> Result<ProcessWorkDriverSetup> {
        let (process_registry, process_change_hub) = match process_work_source {
            ProcessWorkSource::None => return Ok(ProcessWorkDriverSetup::None),
            ProcessWorkSource::External(driver) => {
                return Ok(ProcessWorkDriverSetup::External {
                    driver: driver.clone(),
                });
            }
            ProcessWorkSource::Inline { registry, hub } => (Arc::clone(registry), hub.clone()),
        };
        // The worker rebuilds a session runtime per process, so it needs a store
        // factory; without one the default runner could not execute anything, so
        // fail loudly rather than silently leave processes unexecuted.
        let Some(store_factory) = store_factory else {
            return Err(EmbedError::ProcessRegistryRequiresStoreFactory);
        };
        // The worker rebuilds with the same plugin host the live runtime uses,
        // including the protocol plugin that supplies the protocol session
        // capability. Install its plugin-contributed process engines onto a
        // clean copy of the base host — `core` deliberately carries none.
        let runtime_host = worker_plugin_host
            .install_process_engine_contributions(core.clone(), process_lifecycle_available)?;
        let phase_probe_slot = lash_core::runtime::RuntimeTurnPhaseProbeSlot::default();
        let mut config = DurableProcessWorkerConfig::new(
            Arc::new(worker_plugin_host.clone()),
            runtime_host,
            Arc::clone(store_factory),
            process_registry,
            session_execution_owner,
        )
        .with_session_policy(policy.clone())
        .with_trigger_store(trigger_store.cloned().unwrap_or_else(|| {
            Arc::new(facade_support::InMemoryTriggerStore::with_clock(
                Arc::clone(&core.clock),
            ))
        }))
        .with_turn_phase_probe_slot(phase_probe_slot)
        .with_process_execution_concurrency(process_execution_concurrency)?;
        if let Some(worker_slot_supplier) = worker_slot_supplier {
            config = config.with_worker_slot_supplier(worker_slot_supplier);
        }
        if let Some(hub) = process_change_hub {
            config = config.with_change_hub(hub);
        }
        let config = Box::new(config);
        Ok(ProcessWorkDriverSetup::LazyDefault { config })
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_queued_work_driver(
        queued_work_source: &QueuedWorkSource,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
        env: RuntimeEnvironment,
        policy: SessionPolicy,
        protocol_factory: Option<Arc<dyn PluginFactory>>,
        plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
        store_factory: Option<&Arc<dyn SessionStoreFactory>>,
        live_replay_store: Arc<dyn LiveReplayStore>,
        process_lifecycle_available: bool,
        worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        queued_work_execution_concurrency: usize,
    ) -> QueuedWorkDriverSetup {
        match queued_work_source {
            QueuedWorkSource::None => QueuedWorkDriverSetup::None,
            QueuedWorkSource::External(driver) => QueuedWorkDriverSetup::External {
                driver: driver.clone(),
            },
            QueuedWorkSource::LazyDefault => match store_factory {
                Some(store_factory) => QueuedWorkDriverSetup::LazyDefault {
                    config: Arc::new(InlineQueuedWorkRunConfig {
                        session_execution_owner,
                        env,
                        policy,
                        protocol_factory,
                        plugin_factories,
                        store_factory: Arc::clone(store_factory),
                        live_replay_store,
                        process_lifecycle_available,
                    }),
                    slot_supplier: worker_slot_supplier,
                    execution_concurrency: queued_work_execution_concurrency,
                },
                None => QueuedWorkDriverSetup::None,
            },
        }
    }

    pub fn advanced(self) -> AdvancedLashCoreBuilder {
        AdvancedLashCoreBuilder { builder: self }
    }

    pub fn process_registry(mut self, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        self.process_work_source = ProcessWorkSource::Inline {
            registry: process_registry,
            hub: None,
        };
        self
    }

    /// Install a best-effort, host-facing [`ProcessEventSink`] on the inline
    /// process registry.
    ///
    /// Each appended process event is pushed to the sink after its durable
    /// write, in per-process append order. This is freshness, not truth: it
    /// never buffers or retries, and consumers reconcile from the durable event
    /// log. Observe completion via the await seam even though the terminal
    /// append is also emitted. See [`ProcessEventSink`] for the full contract.
    ///
    /// Applies to the inline registry path ([`Self::process_registry`]); a host
    /// that supplies its own [`ProcessWorkDriver`](facade_support::ProcessWorkDriver)
    /// installs the sink through the driver's constructor instead.
    ///
    /// [`ProcessEventSink`]: facade_support::ProcessEventSink
    pub fn process_event_sink(mut self, sink: Arc<dyn facade_support::ProcessEventSink>) -> Self {
        self.process_event_sink = Some(sink);
        self
    }

    pub fn trigger_store(mut self, store: Arc<dyn lash_core::TriggerStore>) -> Self {
        self.trigger_store = Some(store);
        self
    }

    /// Configure an externally owned process work runner.
    ///
    /// Durable hosts construct a [`ProcessWorkDriver`] from the same process
    /// registry and wake handle used by their deployment runner, then pass it
    /// here. The driver registry becomes the core's process registry and no
    /// inline runner is spawned.
    pub fn process_work_driver(mut self, driver: ProcessWorkDriver) -> Self {
        self.process_work_source = ProcessWorkSource::External(driver);
        self
    }

    /// Configure an externally owned queued-work driver.
    pub fn queued_work_driver(mut self, driver: QueuedWorkDriver) -> Self {
        self.queued_work_source = QueuedWorkSource::External(driver);
        self
    }

    pub fn disable_queued_work_driver(mut self) -> Self {
        self.queued_work_source = QueuedWorkSource::None;
        self
    }
}

pub(crate) fn build_plugin_host(
    protocol_factory: Option<&Arc<dyn PluginFactory>>,
    common_factories: &[Arc<dyn PluginFactory>],
    extra_factories: Vec<Arc<dyn PluginFactory>>,
) -> Result<PluginHost> {
    let mut factories = Vec::with_capacity(
        usize::from(protocol_factory.is_some()) + common_factories.len() + extra_factories.len(),
    );
    if let Some(protocol_factory) = protocol_factory {
        factories.push(Arc::clone(protocol_factory));
    }
    factories.extend(common_factories.iter().cloned());
    factories.extend(extra_factories);
    Ok(PluginHost::new(factories))
}

impl PromptLayerSink for LashCoreBuilder {
    fn prompt_layer_mut(&mut self) -> &mut PromptLayer {
        self.prompt.get_or_insert_with(PromptLayer::new)
    }
}

pub struct AdvancedLashCoreBuilder {
    builder: LashCoreBuilder,
}

impl AdvancedLashCoreBuilder {
    pub fn runtime_host_config(mut self, core: facade_support::RuntimeHostConfig) -> Self {
        self.builder.runtime_host_config = Some(core);
        self
    }

    pub fn plugin_host(mut self, plugin_host: PluginHost) -> Self {
        self.builder.plugin_host = Some(plugin_host);
        self
    }

    pub fn build(self, session_execution_owner: lash_core::LeaseOwnerIdentity) -> Result<LashCore> {
        self.builder.build(session_execution_owner)
    }
}
