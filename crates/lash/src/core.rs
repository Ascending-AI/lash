use crate::support::*;
use lash_core::facade_support;
use lash_core::runtime::{
    ProcessCommand, ProcessEffectOutcome, RuntimeEffectCommand, RuntimeEffectEnvelope,
    RuntimeEffectKind, RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeInvocation,
    RuntimeScope,
};
use std::collections::HashSet;

mod advanced_builder;
mod drain;
mod queued_work;
mod runtime_host_config;
mod session_policy;
mod work_drivers;
mod worker_capacity;

pub use advanced_builder::AdvancedLashCoreBuilder;
pub use drain::DeploymentDrainStatus;
use queued_work::NativeQueuedWorkRunConfig;
use work_drivers::{
    NativeSubstrateSetup, NativeSubstrateSlot, ProcessPortSetup, ProcessWorkSelection,
    ProcessWorkSource, QueuedPortSetup, QueuedWorkSource, WakeDeliveryDriverSetup,
};
#[derive(Clone)]
/// Owns the configured runtime services used to create and resume Lash sessions.
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
    /// Shared across core clones so native substrate ports are constructed at most once.
    pub(crate) substrate_slot: Arc<NativeSubstrateSlot>,
    /// Host-facing process event sink, retained so a worker config built from
    /// this core reports its worker faults to the same sink the registry
    /// decorator emits events on.
    pub(crate) process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    /// Store-less session ids rejected for reuse by this core.
    pub(crate) ephemeral_session_ids: Arc<std::sync::Mutex<HashSet<String>>>,
    pub(crate) tool_intent_submission_gates:
        Arc<crate::tool_intent_ingress::RuntimeSubmissionGates>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
/// Report produced by session delete.
pub struct SessionDeleteReport {
    /// Identifier of the deleted session.
    pub session_id: String,
    /// Storage reclaimed while deleting the session.
    pub storage: lash_core::SessionBlobReclaimReport,
    /// Process-state deletion report, when a process registry was configured.
    pub process: Option<lash_core::ProcessSessionDeleteReport>,
}

impl LashCore {
    /// Creates a core builder with the supplied turn budget.
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

    /// Creates a builder for the identified session.
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
            pending_observer_intents: Vec::new(),
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

    /// Read the canonical settled view of a durable session without opening a
    /// live runtime, acquiring its execution lease, or exposing mutations.
    ///
    /// This is the inspection path for exporters, debuggers, and administrative
    /// tooling that must coexist with a live writer. `Ok(None)` means the store
    /// has no readable committed state for `session_id`; unsupported backends
    /// return [`StoreError::UnsupportedStoreOperation`](lash_core::StoreError::UnsupportedStoreOperation).
    pub async fn read_session(
        &self,
        session_id: impl AsRef<str>,
    ) -> Result<Option<crate::persistence::SessionReadView>> {
        let session_id = session_id.as_ref();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory
            .read_session(session_id)
            .await
            .map_err(EmbedError::Store)
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
        let ports = self.substrate_slot.ports().await;
        env = env.with_work_ports(ports.process.clone(), ports.queued_port());
        let process_work = env.process_work();
        let runtime =
            LashRuntime::resume(parked.inner, &env, self.session_execution_owner.clone()).await?;
        let handle =
            RuntimeHandle::with_live_replay_store(runtime, Arc::clone(&self.live_replay_store));
        Ok(LashSession {
            runtime: handle,
            effect_host,
            parent_session_id: None,
            active_plugins: Vec::new(),
            process_work,
            process_phase_probe_slot: self.substrate_slot.phase_probe_slot(),
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

    /// Returns the trigger administration facade.
    pub fn triggers(&self) -> crate::admin::CoreTriggerAdmin {
        crate::admin::CoreTriggerAdmin { core: self.clone() }
    }

    /// Returns the process administration facade.
    pub fn processes(&self) -> crate::process_admin::Processes {
        crate::process_admin::Processes { core: self.clone() }
    }

    /// Returns the completion facade for this core.
    pub fn completions(&self) -> crate::admin::Completions {
        crate::admin::Completions { core: self.clone() }
    }

    /// Returns the effect host used by this runtime.
    pub fn effect_host(&self) -> Arc<dyn EffectHost> {
        Arc::clone(&self.env.core.control.effect_host)
    }

    /// Exact-turn cooperative control for this deployment's effect host.
    ///
    /// The returned driver is independently usable from any session handle.
    /// Session and turn ids are routing identity, not authorization; authorize
    /// requests in the host API before forwarding them to Lash.
    pub fn turn_work_driver(&self) -> facade_support::TurnWorkDriver {
        let driver = facade_support::TurnWorkDriver::new(self.effect_host());
        self.store_factory
            .as_ref()
            .map_or(driver.clone(), |factory| {
                driver.with_session_store_factory(Arc::clone(factory))
            })
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
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::default(),
                policy,
            })
            .await
            .map_err(EmbedError::Store)?;
        let is_next_turn = matches!(ingress, lash_core::TurnInputIngress::NextTurn);
        let mut draft = lash_core::PendingTurnInputDraft::new(session_id, ingress, input);
        draft.source_key = id.map(|id| format!("host:{id}"));
        store
            .read_session_state_version()
            .await
            .map_err(EmbedError::Store)?;
        let enqueued = store
            .enqueue_pending_turn_input(draft)
            .await
            .map_err(|err| {
                EmbedError::Runtime(lash_core::RuntimeError::new(
                    lash_core::RuntimeErrorCode::StoreCommitFailed,
                    err.to_string(),
                ))
            })?;
        if is_next_turn {
            self.substrate_slot
                .ports()
                .await
                .queued
                .notify_session_work(
                    SessionWorkTarget::Session(enqueued.session_id.clone()),
                    "queued_turn_input",
                );
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
    ) -> Result<lash_core::ForkSessionReceipt> {
        self.fork_at_with_observer_inheritance(
            node_id,
            session_id,
            lash_core::ObserverInheritance::All,
        )
        .await
    }

    /// Forks a session at the requested point while preserving observer inheritance.
    pub async fn fork_at_with_observer_inheritance(
        &self,
        node_id: impl Into<String>,
        session_id: impl Into<String>,
        observer_inheritance: lash_core::ObserverInheritance,
    ) -> Result<lash_core::ForkSessionReceipt> {
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
                let mut seen_inherited = std::collections::HashSet::new();
                ids.iter()
                    .filter(|id| observed.contains(*id) && seen_inherited.insert(id.as_str()))
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
        let pending_observer_intents = inherited
            .iter()
            .cloned()
            .map(facade_support::SessionObserverIntent::fork_inherited)
            .collect();
        let request = lash_core::ForkSessionRequest {
            session_id,
            node_id,
            relation: lash_core::SessionRelation::Fork {
                source_session_id: point.source_session_id,
                source_node_id: point.node_id,
                observer_inheritance: resolved_observer_inheritance,
            },
            pending_observer_intents,
            policy: fork_policy,
        };
        let mut fork = store_factory.fork_at(&request).await?;
        let create_request = lash_core::SessionStoreCreateRequest {
            session_id: request.session_id,
            relation: request.relation,
            pending_observer_intents: request.pending_observer_intents,
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
        fork.observed_processes = lash_core::runtime::reconcile_session_process_observer_intents(
            self.process_registry().as_deref(),
            &fork.session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(branch_store.as_ref()),
        )
        .await?;
        Ok(fork)
    }

    /// Deletes the session and reports reclaimed storage and process state.
    pub async fn delete_session(
        &self,
        session_id: impl AsRef<str>,
        scoped_effect_controller: ScopedEffectController<'_>,
    ) -> Result<SessionDeleteReport> {
        let session_id = session_id.as_ref().to_string();
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        match lash_core::facade_support::ScopedEffectControllerFacadeOps::execution_scope(
            &scoped_effect_controller,
        ) {
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
        let ports = self.substrate_slot.ports().await;
        let resolved_env = self
            .env
            .clone()
            .with_work_ports(ports.process.clone(), ports.queued_port());
        let process = if let (Some(process_registry), Some(process_work)) = (
            resolved_env.process_registry.as_ref(),
            resolved_env.process_work(),
        ) {
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
                        process_work,
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
        let storage = store_factory
            .delete_session(&session_id)
            .await
            .map_err(|failure| EmbedError::SessionDeleteStorage {
                session_id: session_id.clone(),
                failure: Box::new(failure),
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
            storage,
            process,
        })
    }

    /// Returns the configured process registry, if present.
    pub fn process_registry(&self) -> Option<Arc<dyn ProcessRegistry>> {
        self.env.process_registry.as_ref().cloned()
    }

    /// Builds the durable process-worker configuration for this core.
    pub fn durable_process_worker_config(&self) -> Result<DurableProcessWorkerConfig> {
        self.durable_process_worker_config_with_plugins(std::iter::empty::<Arc<dyn PluginFactory>>())
    }

    /// Builds the durable process-worker configuration with additional plugins.
    pub fn durable_process_worker_config_with_plugins(
        &self,
        extra_plugin_factories: impl IntoIterator<Item = Arc<dyn PluginFactory>>,
    ) -> Result<DurableProcessWorkerConfig> {
        if self.process_registry().is_none() {
            return Err(EmbedError::MissingProcessRegistry);
        }
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            extra_plugin_factories.into_iter().collect(),
        )?;
        let Some(process_work) = self.substrate_slot.configured_worker_process_work() else {
            return Err(EmbedError::MissingProcessRegistry);
        };
        let queued_work: Arc<dyn QueuedWorkSubstrate> = match &self.substrate_slot.setup.queued {
            QueuedPortSetup::External { port } => Arc::clone(port),
            // The outer dispatcher owns the native queued-work lane; nested
            // process runtimes must not start a competing dispatcher.
            QueuedPortSetup::Disabled | QueuedPortSetup::Native { .. } => {
                Arc::new(NoQueuedWork::new())
            }
        };
        worker_config(
            &plugin_host,
            &self.env,
            self.process_lifecycle_available,
            self.policy.clone(),
            self.process_execution_concurrency,
            self.worker_slot_supplier.clone(),
            self.session_execution_owner.clone(),
            process_work,
            queued_work,
            self.process_event_sink.clone(),
            lash_core::runtime::RuntimeTurnPhaseProbeSlot::default(),
            self.substrate_slot.setup.config.clone(),
        )
    }
}

#[derive(Clone)]
struct NativeProcessWorkerSetup {
    worker_plugin_host: PluginHost,
    env: RuntimeEnvironment,
    process_lifecycle_available: bool,
    policy: SessionPolicy,
    process_execution_concurrency: usize,
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    session_execution_owner: lash_core::LeaseOwnerIdentity,
    process_work: WorkerProcessWork,
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
    native_substrate: NativeSubstrateConfig,
}

impl NativeProcessWorkerSetup {
    fn build(
        &self,
        queued_work: Arc<dyn QueuedWorkSubstrate>,
    ) -> Result<DurableProcessWorkerConfig> {
        worker_config(
            &self.worker_plugin_host,
            &self.env,
            self.process_lifecycle_available,
            self.policy.clone(),
            self.process_execution_concurrency,
            self.worker_slot_supplier.clone(),
            self.session_execution_owner.clone(),
            self.process_work.clone(),
            queued_work,
            self.process_event_sink.clone(),
            self.turn_phase_probe_slot.clone(),
            self.native_substrate.clone(),
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn worker_config(
    worker_plugin_host: &PluginHost,
    env: &RuntimeEnvironment,
    process_lifecycle_available: bool,
    policy: SessionPolicy,
    process_execution_concurrency: usize,
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    session_execution_owner: lash_core::LeaseOwnerIdentity,
    process_work: WorkerProcessWork,
    queued_work: Arc<dyn QueuedWorkSubstrate>,
    process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
    turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot,
    native_substrate: NativeSubstrateConfig,
) -> Result<DurableProcessWorkerConfig> {
    let Some(store_factory) = env.session_store_factory.as_ref() else {
        return Err(EmbedError::MissingProcessWorkerStoreFactory);
    };
    let runtime_host = worker_plugin_host
        .install_process_engine_contributions(env.core.clone(), process_lifecycle_available)?;
    let mut config = DurableProcessWorkerConfig::new(
        Arc::new(worker_plugin_host.clone()),
        runtime_host,
        Arc::clone(store_factory),
        process_work,
        queued_work,
        session_execution_owner,
    )
    .with_session_policy(policy)
    .with_turn_phase_probe_slot(turn_phase_probe_slot)
    .with_process_execution_concurrency(process_execution_concurrency)?;
    config.native_substrate = native_substrate;
    if let Some(worker_slot_supplier) = worker_slot_supplier {
        config = config.with_worker_slot_supplier(worker_slot_supplier);
    }
    if let Some(trigger_store) = env.trigger_store.as_ref() {
        config = config.with_trigger_store(Arc::clone(trigger_store));
    }
    if let Some(sink) = process_event_sink {
        config = config.with_process_event_sink(sink);
    }
    Ok(config)
}

fn default_runtime_stack() -> PluginStack {
    lash_plugin_tool_output_budget::tool_output_budget_stack()
}

/// Builder for configuring lash core.
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
    max_attachment_bytes: Option<Option<u64>>,
    queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
    process_wake_delivery_policy: Option<lash_core::DeliveryPolicy>,
    native_substrate: NativeSubstrateConfig,
    trigger_store: Option<Arc<dyn lash_core::TriggerStore>>,
    // Core fields applied while constructing a config from individual builder
    // setters. They conflict with a whole-config override when duplicated.
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
    process_work_source: ProcessWorkSelection,
    // Per-worker bound for the default native process executor.
    process_execution_concurrency: Option<usize>,
    // Per-driver bound for the default native queued-work executor.
    queued_work_execution_concurrency: Option<usize>,
    // Optional host admission controller replacing both fixed worker lanes.
    worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
    // Optional host-facing best-effort feed of appended process events,
    // installed on the native process-registry decorator at build time.
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
            max_attachment_bytes: None,
            queued_work_batching: None,
            process_wake_delivery_policy: None,
            native_substrate: NativeSubstrateConfig::default(),
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
            process_work_source: ProcessWorkSelection::default(),
            process_execution_concurrency: None,
            queued_work_execution_concurrency: None,
            worker_slot_supplier: None,
            process_event_sink: None,
            process_tool_visibility_filter: None,
            queued_work_source: QueuedWorkSource::Unset,
            live_replay_store: None,
        }
    }

    /// Configures the protocol plugin and returns the updated builder.
    pub fn protocol_plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.protocol_factory = Some(plugin);
        self
    }

    /// Configures the provider and returns the updated builder.
    pub fn provider(mut self, provider: ProviderHandle) -> Self {
        self.session_spec = self.session_spec.provider_id(provider.kind());
        self.provider = Some(provider);
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

    /// Configures the attachment store and returns the updated builder.
    pub fn attachment_store(mut self, attachment_store: Arc<dyn AttachmentStore>) -> Self {
        self.attachment_store = Some(attachment_store);
        self
    }

    /// Configures the process env store and returns the updated builder.
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

    /// Configure the maximum bytes accepted by one attachment put.
    ///
    /// The default `None` preserves unbounded attachment puts. `Some(max_bytes)`
    /// rejects larger puts before the configured attachment backend is called.
    /// This deployment limit is independent from [`Self::commit_budget`].
    pub fn max_attachment_bytes(mut self, max_attachment_bytes: Option<u64>) -> Self {
        self.max_attachment_bytes = Some(max_attachment_bytes);
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

    /// Configure pacing for Lash's native process and queued-work scheduler loops.
    pub fn native_substrate_config(mut self, config: NativeSubstrateConfig) -> Self {
        self.native_substrate = config;
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
    /// crosses. Pass [`NativeEffectHost`](crate::durability::NativeEffectHost)
    /// for in-process execution, or a workflow-backed host for durable
    /// execution.
    pub fn effect_host(mut self, effect_host: Arc<dyn EffectHost>) -> Self {
        self.effect_host = Some(effect_host);
        self
    }

    /// Adds a tool provider to the built core.
    pub fn tools(mut self, tools: Arc<dyn ToolProvider>) -> Self {
        self.tool_providers.push(tools);
        self
    }

    /// Configures the plugin and returns the updated builder.
    pub fn plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.plugin_stack.push(plugin);
        self
    }

    /// Configures the plugins and returns the updated builder.
    pub fn plugins(mut self, stack: PluginStack) -> Self {
        self.plugin_stack = stack;
        self
    }

    /// Applies a callback to configure the plugin stack.
    pub fn configure_plugins(mut self, configure: impl FnOnce(&mut PluginStack)) -> Self {
        configure(&mut self.plugin_stack);
        self
    }

    /// Configures the trace sink and returns the updated builder.
    pub fn trace_sink(mut self, trace_sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace_sink = Some(trace_sink);
        self
    }

    /// Configures the trace jsonl path and returns the updated builder.
    pub fn trace_jsonl_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path.into())));
        self
    }

    /// Configures the trace level and returns the updated builder.
    pub fn trace_level(mut self, trace_level: lash_trace::TraceLevel) -> Self {
        self.trace_level = Some(trace_level);
        self
    }

    /// Configures the trace context and returns the updated builder.
    pub fn trace_context(mut self, trace_context: lash_trace::TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    /// Configures the termination and returns the updated builder.
    pub fn termination(mut self, termination: TerminationPolicy) -> Self {
        self.termination = Some(termination);
        self
    }

    /// Configure the lease timing capability for every durable single-writer
    /// lease lane this deployment renews: session execution leases, process
    /// leases, and durable effect-replay leases. Queued-work and turn-input
    /// claims are not leases and carry no TTL.
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
        if matches!(self.queued_work_source, QueuedWorkSource::Unset) {
            return Err(EmbedError::MissingQueuedWorkSource);
        }
        if matches!(self.queued_work_source, QueuedWorkSource::Native)
            && self.child_store_factory.is_none()
            && self.store_factory.is_none()
        {
            return Err(EmbedError::NativeQueuedWorkRequiresStoreFactory);
        }
        let process_execution_concurrency = self
            .process_execution_concurrency
            .unwrap_or(facade_support::DEFAULT_PROCESS_EXECUTION_CONCURRENCY);
        DurableProcessWorkerConfig::validate_process_execution_concurrency(
            process_execution_concurrency,
        )?;
        let queued_work_execution_concurrency = self
            .queued_work_execution_concurrency
            .unwrap_or(facade_support::DEFAULT_QUEUED_WORK_EXECUTION_CONCURRENCY);
        NativeQueuedWork::validate_execution_concurrency(queued_work_execution_concurrency)?;
        self.native_substrate.validate()?;
        let worker_slot_supplier = self.worker_slot_supplier.clone();
        let native_substrate = self.native_substrate.clone();
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

        let core = self.resolve_runtime_host_config()?;
        let process_event_sink = self.process_event_sink.clone();
        let process_work_source = self
            .process_work_source
            .clone()
            .resolve(Arc::clone(&core.clock), process_event_sink.clone());
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
        let default_plugin_host = Arc::new(build_plugin_host(
            protocol_factory.as_ref(),
            &plugin_factories,
            Vec::new(),
        )?);
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
        let native_process_registry = process_work_source.process_registry();
        // Build the native config eagerly so a missing factory fails at build.
        let live_replay_clock = Arc::clone(&core.clock);
        let mut env_builder = RuntimeEnvironment::builder(
            core.durability.commit_budget,
            core.durability.queued_work_batching.clone(),
        )
        .with_plugin_host(Arc::clone(&default_plugin_host))
        .with_runtime_host_config(core);
        if let Some(process_registry) = native_process_registry.as_ref() {
            env_builder = env_builder.with_process_registry(Arc::clone(process_registry));
        } else if let Some(wiring) = process_work_source.external_wiring() {
            env_builder = env_builder.with_process_work(wiring);
        }
        if let Some(child_store_factory) = self
            .child_store_factory
            .as_ref()
            .or(self.store_factory.as_ref())
        {
            env_builder = env_builder.with_session_store_factory(Arc::clone(child_store_factory));
        }
        let trigger_store = self.trigger_store.as_ref().cloned().unwrap_or_else(|| {
            Arc::new(facade_support::InMemoryTriggerStore::with_clock(
                Arc::clone(&live_replay_clock),
            ))
        });
        env_builder = env_builder.with_trigger_store(trigger_store);
        let live_replay_store = self.live_replay_store.take().unwrap_or_else(|| {
            Arc::new(InMemoryLiveReplayStore::with_clock(
                facade_support::InMemoryLiveReplayStoreConfig::default(),
                live_replay_clock,
            ))
        });
        let env = env_builder.build();
        let process_registry = env.process_registry.as_ref().cloned();
        let process_port = Self::resolve_process_work(
            &process_work_source,
            default_plugin_host.as_ref(),
            &env,
            process_lifecycle_available,
            &policy,
            process_execution_concurrency,
            worker_slot_supplier.clone(),
            session_execution_owner.clone(),
            process_event_sink.clone(),
            native_substrate.clone(),
        )?;
        let queued_port = Self::resolve_queued_work(
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
        )?;
        let substrate = NativeSubstrateSetup {
            config: native_substrate,
            process: process_port,
            queued: queued_port,
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
            substrate_slot: Arc::new(NativeSubstrateSlot::new(substrate)),
            process_event_sink,
            ephemeral_session_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
            tool_intent_submission_gates: Default::default(),
        })
    }

    /// Decide how a built [`LashCore`] sources its process-work port.
    ///
    /// - no registry => nothing to run ([`ProcessWorkSource::None`]);
    /// - external wiring supplied => use it ([`ProcessPortSetup::External`]);
    /// - native registry wired => lazily construct the native port on first open. Its
    ///   [`DurableProcessWorkerConfig`] is built eagerly when a store factory is
    ///   present; without one the native worker cannot rebuild session runtimes.
    // Mirrors `resolve_queued_work`; inputs are the required driver state.
    #[allow(clippy::too_many_arguments)]
    fn resolve_process_work(
        process_work_source: &ProcessWorkSource,
        worker_plugin_host: &PluginHost,
        env: &RuntimeEnvironment,
        process_lifecycle_available: bool,
        policy: &SessionPolicy,
        process_execution_concurrency: usize,
        worker_slot_supplier: Option<Arc<dyn WorkerSlotSupplier>>,
        session_execution_owner: lash_core::LeaseOwnerIdentity,
        process_event_sink: Option<Arc<dyn facade_support::ProcessEventSink>>,
        native_substrate: NativeSubstrateConfig,
    ) -> Result<ProcessPortSetup> {
        let watched = match process_work_source {
            ProcessWorkSource::None => return Ok(ProcessPortSetup::None),
            ProcessWorkSource::External(wiring) => {
                return Ok(ProcessPortSetup::External {
                    wiring: wiring.clone(),
                });
            }
            ProcessWorkSource::Native(watched) => watched.clone(),
        };
        // The worker rebuilds a session runtime per process, so it needs a store
        // factory; without one the default runner could not execute anything, so
        // fail loudly rather than silently leave processes unexecuted.
        if env.session_store_factory.is_none() {
            return Err(EmbedError::ProcessRegistryRequiresStoreFactory);
        }
        let config = Box::new(NativeProcessWorkerSetup {
            worker_plugin_host: worker_plugin_host.clone(),
            env: env.clone(),
            process_lifecycle_available,
            policy: policy.clone(),
            process_execution_concurrency,
            worker_slot_supplier,
            session_execution_owner,
            process_work: WorkerProcessWork::SelfNative(watched.clone()),
            // Admission-only drive faults otherwise have no path to the host.
            process_event_sink,
            turn_phase_probe_slot: lash_core::runtime::RuntimeTurnPhaseProbeSlot::default(),
            native_substrate,
        });
        // Validate the same worker assembly eagerly. The live native worker is
        // constructed lazily once the outer queued-work dispatcher exists.
        config.build(Arc::new(NoQueuedWork::new()))?;
        Ok(ProcessPortSetup::NativeDefault { config, watched })
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_queued_work(
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
    ) -> Result<QueuedPortSetup> {
        Ok(match queued_work_source {
            QueuedWorkSource::Unset => return Err(EmbedError::MissingQueuedWorkSource),
            QueuedWorkSource::Disabled => QueuedPortSetup::Disabled,
            QueuedWorkSource::External(port) => QueuedPortSetup::External {
                port: Arc::clone(port),
            },
            QueuedWorkSource::Native => match store_factory {
                Some(store_factory) => QueuedPortSetup::Native {
                    config: Arc::new(NativeQueuedWorkRunConfig {
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
                None => return Err(EmbedError::NativeQueuedWorkRequiresStoreFactory),
            },
        })
    }

    /// Converts this builder into its advanced configuration facade.
    pub fn advanced(self) -> AdvancedLashCoreBuilder {
        AdvancedLashCoreBuilder { builder: self }
    }

    /// Configures the process registry used by the built core.
    pub fn process_registry(mut self, process_registry: Arc<dyn ProcessRegistry>) -> Self {
        self.process_work_source = ProcessWorkSelection::Native(process_registry);
        self
    }

    /// Install a best-effort, host-facing [`ProcessEventSink`] on the native
    /// process registry.
    ///
    /// Each appended process event is pushed to the sink after its durable
    /// write, in per-process append order. This is freshness, not truth: it
    /// never buffers or retries, and consumers reconcile from the durable event
    /// log. Observe completion via the await seam even though the terminal
    /// append is also emitted. See [`ProcessEventSink`] for the full contract.
    ///
    /// Event emission applies to the native registry path
    /// ([`Self::process_registry`]); a host that supplies its own
    /// [`ProcessWorkWiring`] installs the
    /// sink through the deployment's constructor for those.
    ///
    /// Worker faults are not registry events and do not follow that split: the
    /// durable process worker this core configures reports every
    /// [`ProcessWorkerFault`](facade_support::ProcessWorkerFault) to the sink
    /// installed here, whichever registry path the host chose. A host that
    /// drives pending processes wants this installed, because the drive is an
    /// admission call and a fault after admission has no other way home.
    ///
    /// [`ProcessWorkerFault`]: facade_support::ProcessWorkerFault
    ///
    /// [`ProcessEventSink`]: facade_support::ProcessEventSink
    pub fn process_event_sink(mut self, sink: Arc<dyn facade_support::ProcessEventSink>) -> Self {
        self.process_event_sink = Some(sink);
        self
    }

    /// Configures the trigger store and returns the updated builder.
    pub fn trigger_store(mut self, store: Arc<dyn lash_core::TriggerStore>) -> Self {
        self.trigger_store = Some(store);
        self
    }

    /// Configure an externally owned process work runner.
    ///
    /// Durable hosts construct [`ProcessWorkWiring`] from the same watched
    /// registry and port used by their deployment runner, then pass it here.
    /// The wiring's registry becomes the core's process registry and no native
    /// runner is spawned.
    pub fn process_work(mut self, wiring: ProcessWorkWiring) -> Self {
        self.process_work_source = ProcessWorkSelection::External(wiring);
        self
    }

    /// Configure an externally owned queued-work port.
    pub fn with_queued_work(mut self, port: Arc<dyn QueuedWorkSubstrate>) -> Self {
        self.queued_work_source = QueuedWorkSource::External(port);
        self
    }

    /// Configure Lash's native queued-work executor.
    pub fn with_native_queued_work(mut self) -> Self {
        self.queued_work_source = QueuedWorkSource::Native;
        self
    }

    /// Disables automatic queued-work execution for the built core.
    pub fn without_queued_work(mut self) -> Self {
        self.queued_work_source = QueuedWorkSource::Disabled;
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

impl LashCore {
    /// Enumerate every durable session catalog entry.
    ///
    /// This is a read-only catalog query. It does not open sessions, acquire
    /// execution leases, hydrate checkpoints, or mutate catalog generations.
    /// Results are ordered by creation time and then session id, and include
    /// permanent deletion tombstones.
    pub async fn sessions(&self) -> Result<Vec<SessionSummary>> {
        self.sessions_filtered(SessionListFilter::default()).await
    }

    /// Enumerate durable session catalog entries matching `filter`.
    ///
    /// Like [`Self::sessions`], this query never opens a session or acquires
    /// execution authority.
    pub async fn sessions_filtered(
        &self,
        filter: SessionListFilter,
    ) -> Result<Vec<SessionSummary>> {
        let Some(store_factory) = self.store_factory.as_ref() else {
            return Err(EmbedError::MissingSessionStoreFactory);
        };
        store_factory
            .list_sessions(&filter)
            .await
            .map_err(Into::into)
    }
}
