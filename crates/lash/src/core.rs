use crate::support::{
    Arc, DeploymentStore, EmbedError, InMemoryLiveReplayStore, LashRuntime, LashSession,
    LiveReplayStore, ParkedSession, PluginFactory, PluginHost, PluginSpec, PluginStack,
    ProcessRegistry, Result, RuntimeEnvironment, RuntimeHandle, RuntimeHostConfig, SessionBuilder,
    SessionListFilter, SessionView, StaticPluginFactory, ToolProvider,
};
use lash_core::ActorContext;
use lash_core::Backend;
use lash_core::facade_support;
use lash_core_worker::DurableProcessWorkerConfig;
use lash_sansio::SessionId;

mod drain;
mod node;
mod recovery;
mod runtime_host_config;
mod session_deletion;
pub use session_deletion::SessionDeleteCompletion;
mod work_drivers;

pub use drain::{DeploymentDrainStatus, NodeDrainError, NodeDrainReport};
use work_drivers::CoreWorkSetup;
pub(crate) use work_drivers::CoreWorkSlot;
#[derive(Clone)]
/// Owns the configured runtime services used to create and resume Lash sessions.
pub struct LashCore {
    pub(crate) runtime_owner: lash_core::LeaseOwnerIdentity,
    pub(crate) env: RuntimeEnvironment,
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    /// The one substrate every port and the effect host come from.
    pub(crate) backend: Backend,
    /// The backend's session catalog.
    pub(crate) store_factory: Arc<dyn DeploymentStore>,
    /// The backend's process registry, as the core sees it (watched, and
    /// on the core's clock).
    pub(crate) process_registry: Arc<dyn ProcessRegistry>,
    pub(crate) plugin_factories: Arc<Vec<Arc<dyn PluginFactory>>>,
    pub(crate) live_replay_store: Arc<dyn LiveReplayStore>,
    /// The tail of every process feed, apart from session live replay.
    pub(crate) process_replay_store: Arc<dyn lash_core::ProcessReplayStore>,
    pub(crate) language_observation_publisher:
        Arc<crate::language_observation::LanguageObservationPublisher>,
    /// What one process snapshot may read to fold its effect evidence.
    pub(crate) process_effect_fold_budget: crate::process_feed::EffectFoldBudget,
    pub(crate) process_observation_hub: Arc<crate::process_observation::ProcessObservationHub>,
    pub(crate) process_lifecycle_feed: Arc<crate::process_lifecycle::ProcessLifecycleFeed>,
    /// The core's process event sinks, its own lifecycle feed first: each
    /// stays attached while any clone of the core lives.
    pub(crate) _process_event_registrations: Arc<Vec<facade_support::ProcessEventSinkRegistration>>,
    /// Whether process lifecycle is available; threaded into rebuilt session plugin hosts.
    pub(crate) process_lifecycle_available: bool,
    /// Base plugin-contributed engines available to host-level process APIs.
    /// Session runtimes still install onto their own clean registries so
    /// session-scoped plugin overlays can contribute additional engines.
    pub(crate) host_process_engines: lash_core::ProcessEngineRegistry,
    /// Shared across core clones so the work ports are resolved at most once.
    pub(crate) substrate_slot: Arc<CoreWorkSlot>,
    /// This core's seat in the recovery leader election (ADR 0109 §1.6);
    /// the core resigns it at shutdown.
    pub(crate) recovery: Arc<recovery::RecoverySlot>,
    /// The node the backend's session actors run on (ADR 0132 §3).
    pub(crate) node: Arc<node::NodeSlot>,
    pub(crate) observer_pacing: Arc<crate::ObserverPacing>,
}

pub use lash_core::session_delete::SessionDeletion;

impl LashCore {
    pub(crate) fn transcript_decoders(&self) -> crate::transcript::TranscriptDecoders {
        self.protocol_factory
            .iter()
            .chain(self.plugin_factories.iter())
            .filter_map(|factory| factory.transcript_decoder())
            .fold(Default::default(), |decoders, decoder| {
                decoders.with_decoder(decoder)
            })
    }

    /// A [`LashCoreBuilder`] over `backend`, the one substrate every
    /// persistence port and the effect host of this core come from (ADR 0102).
    ///
    /// The backend is the builder's only source of ports: there is no
    /// setter for a store, a registry or an effect host, so a core cannot mix
    /// substrates, and there is no in-memory default. Build the backend with
    /// [`DurableBackendBuilder`](crate::durable::DurableBackendBuilder); the
    /// zero-infra backend for local tests is the durable backend over a
    /// SQLite memory store set. `docs/operations/durable-hosting.md` is the
    /// host guide.
    ///
    /// The builder takes deployment facts only: the backend, the plugins,
    /// the model registry, tracing and the like. A core keeps no session
    /// defaults (FIG-4594): every run is created from the
    /// [`SessionSpec`](crate::SessionSpec) its creator states, and everything
    /// else lash creates derives from a record.
    pub fn builder(backend: Backend) -> LashCoreBuilder {
        LashCoreBuilder::new(backend)
    }

    /// Sugar entry point: a [`LashCoreBuilder`] over `backend` pre-seeded
    /// with the standard protocol plugin.
    pub fn standard_builder(backend: Backend) -> LashCoreBuilder {
        LashCore::builder(backend).protocol_plugin(Arc::new(
            lash_protocol_standard::StandardProtocolPluginFactory::new(),
        ))
    }

    /// The backend this core takes every port and its effect host from.
    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// The host owns admission and must pass its current admission state. Lash
    /// reads the process registry and the session store on demand; it does
    /// not maintain a counter or orchestrate routing, deadlines, worker
    /// shutdown, or retirement.
    ///
    /// Turns are counted as well as processes (FIG-3586): an unfinished
    /// turn is unfinished work, so the deployment is not drained until none
    /// remains. A store that cannot count its turns refuses rather than
    /// report zero.
    pub async fn drain_status(&self, accepting_new_work: bool) -> Result<DeploymentDrainStatus> {
        let remaining_invocations = self.process_registry.count_non_terminal_processes().await?;
        let turns = self.store_factory.count_unsettled_turns().await?;
        let checked_at = self.env.core.clock.timestamp_ms();
        let mut stalled_obligations = std::collections::BTreeMap::new();
        for kind in lash_core::store::ObligationKind::ALL {
            let count = self.backend.obligation_ledger(kind).count_stalled().await?;
            lash_core::operational_metrics::record_obligations_stalled(
                self.env.core.tracing.metrics(),
                kind.label(),
                count,
            );
            stalled_obligations.insert(kind, count);
        }
        Ok(DeploymentDrainStatus {
            accepting_new_work,
            remaining_invocations,
            in_flight_turns: turns.in_flight_turns,
            stalled_obligations,
            checked_at,
        })
    }

    /// The stalled obligations of `kind` after `after`, in id order, at most
    /// `limit` (ADR 0109 §1.5): store→engine deliveries the relay stopped
    /// retrying, each with its reason, attempt count and last error.
    pub async fn stalled_obligations(
        &self,
        kind: lash_core::store::ObligationKind,
        after: Option<&lash_core::store::ObligationId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<lash_core::store::StalledObligation>> {
        Ok(self
            .backend
            .obligation_ledger(kind)
            .list_stalled(after, limit)
            .await?)
    }

    /// Put stalled obligation `id` of `kind` back in the relay's due index
    /// with its attempts reset, due now (ADR 0109 §1.5). Nothing re-arms a
    /// stalled obligation but this verb. `false` when `id` is not stalled.
    pub async fn rearm_obligation(
        &self,
        kind: lash_core::store::ObligationKind,
        id: &lash_core::store::ObligationId,
    ) -> Result<bool> {
        let now_ms = self.env.core.clock.timestamp_ms();
        Ok(self
            .backend
            .obligation_ledger(kind)
            .rearm(id, now_ms)
            .await?)
    }

    /// Reconcile durable turn and session terminals after a live replay gap.
    /// Persist the returned cursor only after applying every change in the page.
    pub async fn turns_changed_since(
        &self,
        after: lash_core::store::TurnChangeCursor,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::ChangePage<lash_core::store::TurnChange, lash_core::store::TurnChangeCursor>>
    {
        let page = self.store_factory.turns_changed_since(after, limit).await?;
        Ok(crate::ChangePage {
            changes: page.changes,
            next: page.next,
            retained_after: Some(page.retained_after),
        })
    }

    /// The standing session faults after session `after`, in session-id
    /// order, at most `limit` (ADR 0109 §9): corrupt stored data the engine
    /// met after a run's answer was published, at the run's owed scope
    /// close or at its shift's next admission. Each carries the typed code
    /// and cause the read failed with. A faulted session admits nothing:
    /// every send to it is answered with the fault until
    /// [`clear_session_fault`](Self::clear_session_fault).
    pub async fn session_faults(
        &self,
        after: Option<&lash_core::SessionId>,
        limit: std::num::NonZeroUsize,
    ) -> Result<crate::ChangePage<lash_core::store::SessionFault, Option<lash_core::SessionId>>>
    {
        let changes = self.store_factory.list_session_faults(after, limit).await?;
        let next = changes
            .last()
            .map(|fault| fault.session_id.clone())
            .or_else(|| after.cloned());
        Ok(crate::ChangePage {
            changes,
            next,
            retained_after: None,
        })
    }

    /// Clear `session_id`'s fault once its stored data is repaired (ADR 0109
    /// §9): the session admits again. Nothing clears a fault but this verb.
    /// A fault a scope close recorded also left that run's `ScopeClose`
    /// obligation stalled, which [`rearm_obligation`](Self::rearm_obligation)
    /// makes due again. `false` when the session has no fault.
    pub async fn clear_session_fault(&self, session_id: &lash_core::SessionId) -> Result<bool> {
        Ok(self.store_factory.clear_session_fault(session_id).await?)
    }

    /// The deployment's control intents, to list.
    pub fn parked_work(&self) -> crate::parked_work::ParkedWork {
        crate::parked_work::ParkedWork {
            store_factory: Arc::clone(&self.store_factory),
        }
    }

    /// Sugar entry point: a [`LashCoreBuilder`] pre-seeded with a
    /// host-configured RLM protocol factory and the default runtime plugin
    /// stack.
    ///
    /// The host configures the factory (projection resolver, separate deferred
    /// tool resolvers, execution sink/jsonl path) before
    /// passing it in. The factory is built over this same `backend`
    /// ([`RlmProtocolPluginFactory::new`](crate::rlm::RlmProtocolPluginFactory::new)),
    /// which supplies its Lash VM artifact store; a factory built over any
    /// other backend is refused when the core is built.
    #[cfg(feature = "rlm")]
    pub fn rlm_builder(
        backend: Backend,
        factory: crate::rlm::RlmProtocolPluginFactory,
    ) -> LashCoreBuilder {
        LashCore::builder(backend).protocol_plugin(Arc::new(factory))
    }

    pub fn session(&self, session_id: SessionId) -> SessionBuilder {
        self.node.ensure(self);
        SessionBuilder {
            core: self.clone(),
            session_id,
        }
    }

    /// Drain this core's node by release (ADR 0106 §1) and wait until it
    /// stops: what a host does to its old build when a release changes a
    /// durable format, in place of resetting lash's state.
    ///
    /// The node records itself draining and claims nothing more. Each
    /// session it owns stops at its next committed phase (before its next
    /// model call or cell) and each process once its running steps have
    /// committed their outcomes; each is released `ready` for whichever
    /// node of the next build decodes it, which resumes it from its rows.
    /// When none is left the node releases its lease and the drain answers
    /// what it released. The core still admits work: a send writes its mail
    /// and wakes its session, which the next build's nodes claim. A drained
    /// core never starts a node again, and draining it again answers the
    /// same report.
    ///
    /// # Errors
    ///
    /// [`NodeDrainError::NotServing`] when the core runs no node,
    /// [`NodeDrainError::Stopped`] when the node stopped for another reason
    /// first, and [`NodeDrainError::Store`] for the store's refusal.
    pub async fn drain(&self) -> std::result::Result<NodeDrainReport, NodeDrainError> {
        self.node.drain(self).await
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
    /// policy and must make repeated shutdown calls idempotent. Every worker
    /// built from [`Self::durable_process_worker_config`] shares these
    /// factories.
    pub async fn shutdown(&self) -> Result<()> {
        // A stopping deployment hands recovery leadership over now rather
        // than after the lease's TTL (ADR 0109 §1.6).
        self.recovery.resign().await;
        // The node stops before the plugins its turns run go.
        self.node.stop().await;
        self.language_observation_publisher.shutdown().await;
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

    /// Select the lifecycle owner used by administrative session operations.
    ///
    /// The returned handle keeps the catalog, effect host, process services,
    /// chosen by this core together. Provider, plugin,
    /// tracing, and other live turn wiring are deliberately excluded.
    pub async fn session_administration(&self) -> lash_core::SessionAdministration {
        let ports = self.substrate_slot.ports().await;
        let env = self.env.clone().with_work_ports(ports.process.clone());
        lash_core::SessionAdministration::new(
            Arc::clone(&self.store_factory),
            env.core.control.effect_host.clone(),
            Some(ports.process),
            Arc::clone(&env.core.durability.process_env_store),
            self.host_process_engines.clone(),
        )
    }

    /// Rebuild a live session from a [`ParkedSession`](crate::ParkedSession)
    /// handle produced by [`LashSession::park`](crate::LashSession::park).
    ///
    /// Resume reloads the flushed state from the parked store (honoring this
    /// core's residency), reinstalls this core's plugin configuration and work
    /// `SessionShifts` implementations, and returns a ready [`LashSession`]. The parked store instance
    /// is reused directly, so the transcript the session flushed at park time is
    /// visible again after resume.
    ///
    /// This restores the core-level plugin stack. Session-specific plugins added
    /// per open via [`SessionBuilder::plugin`] are not re-applied here; parking
    /// is the round-trip for the core's own configuration.
    pub async fn resume(&self, parked: ParkedSession) -> Result<LashSession> {
        let ParkedSession { inner, binding } = parked;
        // Build the per-session env exactly like `SessionBuilder::open_resolved`:
        // a fresh plugin host with this core's factories, the shared work
        // `SessionShifts` implementations, and the core provider resolver already carried on
        // `self.env`.
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            &self.env.core,
        )?;
        let mut env = binding.apply_owner(self.env.clone());
        env.core = plugin_host.install_process_engine_contributions(
            env.core.clone(),
            self.process_lifecycle_available,
        )?;
        env.plugin_host = Some(Arc::new(plugin_host));
        let runtime = LashRuntime::resume(inner, &env, self.runtime_owner.clone()).await?;
        let handle =
            RuntimeHandle::with_live_replay_store(runtime, Arc::clone(&self.live_replay_store));
        let process_lifecycle_route = self.process_lifecycle_feed.register(&handle);
        let parent_session_id =
            crate::session::recorded_parent_session_id(&binding.store()).await?;
        Ok(LashSession {
            runtime: handle,
            _process_lifecycle_route: process_lifecycle_route,
            binding,
            parent_session_id,
        })
    }

    /// Flush this core's configured trace sink, if any.
    ///
    /// Hosts that hand `lash` a trace sink via [`LashCoreBuilder::trace_sink`] already hold
    /// their own `Arc` and can flush it directly; this is the equivalent lever for hosts that
    /// did not retain the handle.
    /// It flushes the core's copy — for a [`JsonlTraceSink`](lash_trace::JsonlTraceSink) that
    /// fsyncs the file, and for an OTel sink it is a no-op (the host still owns provider
    /// flush; see the tracing docs).
    pub fn flush_trace_sink(&self) -> Result<()> {
        self.env.core.tracing.flush()?;
        Ok(())
    }

    pub fn processes(&self) -> crate::process_admin::Processes {
        crate::process_admin::Processes { core: self.clone() }
    }

    /// The core's artifact stores, as a host publishes into them under a
    /// pin it minted (ADR 0113 §2.6).
    pub fn host_artifacts(&self) -> crate::artifacts::HostArtifacts {
        crate::artifacts::HostArtifacts::new(
            self.backend().module_artifacts(),
            Arc::clone(&self.env.core.durability.process_env_store),
            self.backend().definition_store(),
            self.backend().attachment_referrers(),
            self.backend().artifact_cleanup(),
            Arc::clone(&self.env.core.clock),
            self.host_process_engines.clone(),
        )
    }

    /// Check start arguments against a retained definition's authoritative signature.
    pub fn process_definitions(&self) -> crate::artifacts::ProcessDefinitions {
        crate::artifacts::ProcessDefinitions {
            artifacts: self.host_artifacts(),
        }
    }

    pub fn completions(&self) -> crate::admin::Completions {
        crate::admin::Completions { core: self.clone() }
    }

    pub fn effect_host(&self) -> ActorContext {
        self.env.core.control.effect_host.clone()
    }

    /// Exact-turn cooperative control for this deployment's effect host.
    ///
    /// The returned driver is independently usable from any session handle.
    /// Its `request_cancel` also withdraws an input still queued: a turn no
    /// run opened yet is addressed by the run its input will open (its
    /// source key, or else its input id), and its cancel answers
    /// [`TurnCancelOutcome::Withdrawn`](facade_support::TurnCancelOutcome::Withdrawn),
    /// with the queue change published to this core's Live Replay, instead
    /// of `UnknownOrRevoked`. One request does exactly one of withdraw or
    /// cancel: when the session admitted the input first, it cancels the run.
    /// Session and turn ids are routing identity, not authorization; authorize
    /// requests in the host API before forwarding them to Lash.
    pub fn turn_work_driver(&self) -> facade_support::TurnWorkDriver {
        let backend = self.effect_host().backend().clone();
        let withdrawals = facade_support::QueueWithdrawalObservation::new(
            backend.session_store_factory(),
            Arc::clone(&self.live_replay_store),
        );
        facade_support::TurnWorkDriver::new(backend)
            .with_terminal_pacing(self.observer_pacing.terminal)
            .publishing_withdrawals(Arc::new(withdrawals))
    }

    /// Create `request.session_id` at the state `target` of `session` names,
    /// without writing graph nodes.
    ///
    /// The name of a state is `(session, head revision)`. A
    /// [`Target::Input`](lash_core::Target::Input) names the run that
    /// applied the input, a [`Target::Turn`](lash_core::Target::Turn) the
    /// revision that run's terminal commit published, and a
    /// [`Target::Revision`](lash_core::Target::Revision) the revision
    /// itself. A session that has never run a turn forks at its creation
    /// revision and records the config it was created with.
    ///
    /// The fork is taken only if the revision is still retained. Under the
    /// [`Retention::UntilGc`](lash_core::Retention::UntilGc), as chosen by
    /// [`DataRetention::standard`](crate::DataRetention::standard), every past
    /// turn is, until the host collects; a pin keeps one through
    /// collections. A target that names no retained state refuses with a
    /// typed `EmbedError::Store`, and Lash never substitutes another state:
    ///
    /// * [`StoreError::ForkTargetPending`](lash_core::StoreError::ForkTargetPending):
    ///   the target's run has not finished;
    /// * [`StoreError::ForkTargetUnavailable`](lash_core::StoreError::ForkTargetUnavailable):
    ///   the run ended without a commit, or the input was withdrawn;
    /// * [`StoreError::ForkTargetPruned`](lash_core::StoreError::ForkTargetPruned):
    ///   the revision was collected.
    ///
    /// The fork records the forked revision's recorded config in full
    /// (FIG-4594): model, turn budget, no-progress budget, charge
    /// safety, generation, attachment acceptance, tool access and plugin
    /// configuration are the revision's, and nothing this core or its host
    /// states today stands in for any of them. Change the fork's config
    /// afterwards with a config transaction.
    ///
    /// The fork gets fresh session and execution identities and copies no
    /// pending ingress, pin or retention policy. A
    /// [`SessionRelation::Fork`](lash_core::SessionRelation::Fork) that
    /// names no source node records the forked revision's leaf.
    pub async fn fork_at(
        &self,
        session: &SessionId,
        target: lash_core::Target,
        request: ForkRequest,
    ) -> Result<lash_core::ForkSessionReceipt> {
        let store_factory = &self.store_factory;
        let ForkRequest {
            session_id,
            relation,
            observed_processes,
        } = request;
        let revision = store_factory.resolve_target(session, &target).await?;
        let relation = match relation {
            lash_core::SessionRelation::Fork {
                source_session_id,
                source_node_id: None,
            } => lash_core::SessionRelation::Fork {
                source_session_id,
                source_node_id: revision.leaf_node_id.clone(),
            },
            relation => relation,
        };
        let config = revision.fork_config();
        let mut selected = std::collections::HashSet::new();
        let mut pending_observer_intents = Vec::new();
        for process_id in observed_processes {
            if !selected.insert(process_id.clone()) {
                continue;
            }
            pending_observer_intents.push(facade_support::SessionObserverIntent::host_requested(
                process_id,
            ));
        }
        let request = lash_core::ForkSessionRequest {
            session_id,
            source_session_id: session.clone(),
            head_revision: revision.head_revision,
            relation,
            pending_observer_intents,
            config,
            retention: self.env.core.durability.session_retention,
        };
        let mut fork = store_factory
            .fork_session(&request)
            .await
            .map_err(|error| {
                // The store names the revision it was asked for; the host asked
                // for a target.
                match error {
                    lash_core::StoreError::ForkTargetPruned { session_id, .. } => {
                        lash_core::StoreError::ForkTargetPruned {
                            session_id,
                            target: target.clone(),
                        }
                    }
                    error => error,
                }
            })?;
        match store_factory.lookup_session(&request.session_id).await? {
            lash_core::store::SessionLookup::Live(_) => {}
            lash_core::store::SessionLookup::Deleted | lash_core::store::SessionLookup::Absent => {
                return Err(lash_core::StoreError::Backend(format!(
                    "fork session `{}` disappeared before observer publication completed",
                    request.session_id
                ))
                .into());
            }
        }
        fork.observed_processes = lash_core::runtime::reconcile_session_process_observer_intents(
            Some(self.process_registry.as_ref()),
            &fork.session_id,
            lash_core::runtime::SessionObserverIntentSource::Persisted(store_factory.as_ref()),
        )
        .await?;
        Ok(fork)
    }

    /// Delete a session (ADR 0132 §12): its close request as the session's
    /// mail.
    ///
    /// The session actor closes itself, one durable step at a time: it
    /// cancels its open turn, revokes its waits, ends its `Until` processes
    /// and waits for each to be terminal, deletes its storage
    /// (arming the `ArtifactCleanup` of what it referred to), deletes its
    /// process state and writes its tombstone. A crash resumes the close at
    /// the step it interrupted; nothing is owed by the caller. Await the
    /// tombstone with [`await_session_deletion`](Self::await_session_deletion).
    ///
    /// Deleting an id that never materialized a session is a no-op (ADR
    /// 0049), answered [`SessionDeletion::Absent`]: nothing is requested, and
    /// the id stays creatable.
    pub async fn delete_session(
        context: lash_core::SessionDeleteContext<'_>,
    ) -> Result<SessionDeletion> {
        Ok(lash_core::session_delete::delete_session(&context).await?)
    }

    /// The process registry of this core's backend.
    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry> {
        Arc::clone(&self.process_registry)
    }

    /// Builds the durable process-worker configuration for this core: the
    /// core's own plugin set, the one every worker of its engine binding
    /// installs. A process runs under the environment its start captured, so
    /// the worker binds the physical plugins and selects no behaviour of its
    /// own; an incompatible plugin set belongs on a separate engine binding
    /// (FIG-4396).
    pub fn durable_process_worker_config(&self) -> Result<DurableProcessWorkerConfig> {
        let plugin_host = build_plugin_host(
            self.protocol_factory.as_ref(),
            self.plugin_factories.as_ref(),
            &self.env.core,
        )?;
        let runtime_host = plugin_host.install_process_engine_contributions(
            self.env.core.clone(),
            self.process_lifecycle_available,
        )?;
        Ok(DurableProcessWorkerConfig::new(
            Arc::new(plugin_host),
            runtime_host,
            self.substrate_slot.setup.process.clone(),
            self.runtime_owner.clone(),
        ))
    }
}

/// Builder for configuring lash core over one [`Backend`].
pub struct LashCoreBuilder {
    output_cuts: Option<crate::RuntimeOutputCuts>,
    prompt_render_pool: Option<Arc<lash_core::plugin::prompt::PromptRenderPool>>,
    pub(crate) protocol_factory: Option<Arc<dyn PluginFactory>>,
    models: Option<Arc<dyn lash_core::LlmProfiles>>,
    /// The run definitions sent inputs' specs may name (FIG-3838).
    run_definitions: lash_core::RunDefinitions,
    /// The one substrate every persistence port and the effect host come from.
    backend: Backend,
    commit_budget: Option<facade_support::CommitBudget>,
    queued_work_batching: Option<facade_support::QueuedWorkBatchingConfig>,
    data_retention: Option<crate::DataRetention>,
    provider_file_uploaders: Vec<Arc<dyn lash_core::attachments::ProviderFileUploader>>,
    provider_file_cache: lash_core::attachments::ProviderFileCacheLimits,
    delivery_fetch_horizon: crate::attachments::DeliveryFetchHorizon,
    // Core fields applied over the config the backend's ports assemble.
    trace_runtime: Option<lash_core::runtime::TraceRuntime>,
    trace_sink: Option<Arc<dyn lash_trace::TraceSink>>,
    #[cfg(feature = "otel-trace")]
    telemetry: Option<lash_trace::otel::OtelTelemetry>,
    trace_level: Option<lash_trace::TraceLevel>,
    telemetry_content: Option<lash_trace::TelemetryContent>,
    trace_limits: Option<crate::tracing::TraceLimits>,
    observation_work_limits: crate::tracing::ObservationWorkLimits,
    trace_context: Option<lash_trace::TraceContext>,
    tool_source_policy: Option<lash_core::ToolSourcePolicy>,
    execution_budgets: Option<lash_core::ExecutionBudgets>,
    delta_coalescing: Option<crate::DeltaCoalescing>,
    tool_providers: Vec<Arc<dyn ToolProvider>>,
    plugin_stack: PluginStack,
    recovery_lease: Option<lash_core::engine::RecoveryLeaseConfig>,
    recovery_pass: lash_core::engine::RecoveryPassBudget,
    process_tool_visibility_filter: Option<Arc<dyn facade_support::ProcessToolVisibilityFilter>>,
    live_replay_store: Option<Arc<dyn LiveReplayStore>>,
    process_replay_store: Option<Arc<dyn lash_core::ProcessReplayStore>>,
    process_event_sinks: Vec<Arc<dyn facade_support::ProcessEventSink>>,
    process_observation_work_limits: crate::process_observation::ProcessObservationWorkLimits,
    serves_sessions: bool,
    attachment_reclamation_retry: crate::persistence::AttachmentReclamationRetryPolicy,
    work_cadence: crate::WorkCadencePolicy,
    relay_policy: crate::RelayPolicy,
    commit_admission: crate::CommitAdmissionPolicy,
    observer_pacing: crate::ObserverPacing,
    runtime_pacing: crate::RuntimePacingPolicy,
    recovery_pacing: crate::RecoveryPacing,
}

impl LashCoreBuilder {
    /// Standard serving preset: this core serves its backend's sessions.
    /// This is the historical product choice, without workload measurements;
    /// `.serve_sessions(false)` selects an observer and producer only.
    pub const STANDARD_SERVE_SESSIONS: bool = true;

    /// Select readable value and raw-error transcript cuts. Omission uses
    /// `RuntimeOutputCuts::standard()`; retained output remains independently governed.
    pub fn output_cuts(mut self, cuts: crate::RuntimeOutputCuts) -> Self {
        self.output_cuts = Some(cuts);
        self
    }

    /// Select prompt composition workers and queue capacity for every model call.
    /// Omission uses `PromptRenderPoolConfig::standard()` in the shared pool.
    pub fn prompt_render_pool(mut self, config: crate::plugins::PromptRenderPoolConfig) -> Self {
        self.prompt_render_pool = Some(Arc::new(
            lash_core::plugin::prompt::PromptRenderPool::from_config(config),
        ));
        self
    }

    fn new(backend: Backend) -> Self {
        Self {
            output_cuts: None,
            prompt_render_pool: None,
            protocol_factory: None,
            models: None,
            run_definitions: lash_core::RunDefinitions::default(),
            backend,
            commit_budget: None,
            queued_work_batching: None,
            data_retention: None,
            provider_file_uploaders: Vec::new(),
            provider_file_cache: Default::default(),
            delivery_fetch_horizon: Default::default(),
            trace_runtime: None,
            trace_sink: None,
            #[cfg(feature = "otel-trace")]
            telemetry: None,
            trace_level: None,
            telemetry_content: None,
            trace_limits: None,
            observation_work_limits: Default::default(),
            trace_context: None,
            tool_source_policy: None,
            execution_budgets: None,
            delta_coalescing: None,
            tool_providers: Vec::new(),
            plugin_stack: PluginStack::default(),
            recovery_lease: None,
            recovery_pass: lash_core::engine::RecoveryPassBudget::default(),
            process_tool_visibility_filter: None,
            live_replay_store: None,
            process_replay_store: None,
            process_event_sinks: Vec::new(),
            process_observation_work_limits: Default::default(),
            serves_sessions: Self::STANDARD_SERVE_SESSIONS,
            attachment_reclamation_retry:
                crate::persistence::AttachmentReclamationRetryPolicy::standard(),
            work_cadence: crate::WorkCadencePolicy::standard(),
            relay_policy: crate::RelayPolicy::standard(),
            commit_admission: crate::CommitAdmissionPolicy::standard(),
            observer_pacing: crate::ObserverPacing::standard(),
            runtime_pacing: crate::RuntimePacingPolicy::standard(),
            recovery_pacing: crate::RecoveryPacing::standard(),
        }
    }

    /// Whether the core runs a node for its backend's session actors
    /// (ADR 0132 §3); it does unless told otherwise. A core that serves
    /// none still sends, reads and administers its sessions, and their
    /// turns run on the deployment's other nodes.
    pub fn serve_sessions(mut self, serve: bool) -> Self {
        self.serves_sessions = serve;
        self
    }

    pub fn protocol_plugin(mut self, plugin: Arc<dyn PluginFactory>) -> Self {
        self.protocol_factory = Some(plugin);
        self
    }

    /// Register a run definition a sent input's [`RunSpec`](crate::RunSpec)
    /// may name by its exact [`DefinitionRef`](crate::DefinitionRef). A run
    /// whose spec names a revision this deployment does not register retries
    /// and parks, and recovers once a deployment registers it; no other
    /// revision is ever used in its place.
    pub fn run_definition(mut self, definition: impl crate::RunDefinition + 'static) -> Self {
        self.run_definitions.register(Arc::new(definition));
        self
    }

    /// The host's models: the registry that mints a session's model binding
    /// by key at creation and at every model change, and binds a recorded
    /// binding to the transport that executes it. Without it no session can
    /// be created or run a turn.
    pub fn llm_profiles(mut self, models: Arc<dyn lash_core::LlmProfiles>) -> Self {
        self.models = Some(models);
        self
    }

    /// Test convenience: serve one model through `provider`. The registry
    /// keys the model by its wire model, so a test names it in its
    /// [`SessionSpec`](crate::SessionSpec) by the wire model of the metadata
    /// it built.
    #[cfg(any(test, feature = "testing"))]
    pub fn serve_test_llm_profile(
        self,
        provider: facade_support::ProviderHandle,
        metadata: lash_core::LlmProfileMetadata,
    ) -> Self {
        let key = lash_core::LlmProfileKey::new(metadata.wire_model.clone());
        self.llm_profiles(lash_core::testing::single_llm_profile_registry(
            key, metadata, provider,
        ))
    }

    /// Configure the byte and graph-node limits for each atomic runtime
    /// commit. Hosts must choose bounded or unbounded behavior explicitly for
    /// both dimensions.
    pub fn commit_budget(mut self, commit_budget: facade_support::CommitBudget) -> Self {
        self.commit_budget = Some(commit_budget);
        self
    }

    /// State what this host keeps, how much of it and for how long
    /// ([`DataRetention`](crate::DataRetention)): the attachment put bound,
    /// read budgets and upload expiry, which outputs leave history for a
    /// retained attachment, which revisions a session keeps, and what the
    /// live replay buffer and the process observation hub hold. Required:
    /// lash has no default for any of them, and
    /// [`DataRetention::standard`](crate::DataRetention::standard) is the
    /// named preset a host may choose.
    ///
    /// Each step that applies the retained-output policy journals it, and a
    /// session records its revision retention when it is created, so a
    /// changed statement applies to new work and never to what a replay
    /// serves.
    pub fn data_retention(mut self, data_retention: crate::DataRetention) -> Self {
        self.data_retention = Some(data_retention);
        self
    }

    /// Fetch slack recorded in each new call's request template. Resends use
    /// the admitted value. Defaults to [`crate::attachments::DeliveryFetchHorizon::standard`].
    pub fn delivery_fetch_horizon(
        mut self,
        horizon: crate::attachments::DeliveryFetchHorizon,
    ) -> Self {
        self.delivery_fetch_horizon = horizon;
        self
    }

    /// Optional provider Files API uploaders, scoped to this host's credentials.
    pub fn provider_file_uploaders(
        mut self,
        uploaders: Vec<Arc<dyn lash_core::attachments::ProviderFileUploader>>,
    ) -> Self {
        self.provider_file_uploaders = uploaders;
        self
    }

    /// Bound the cache of files those uploaders made: how many it remembers
    /// and for how long. Defaults to 1024 files for 24 hours; a file must
    /// outlive the call it is delivered to, so a lifetime shorter than one
    /// call refuses every upload.
    pub fn provider_file_cache(
        mut self,
        limits: lash_core::attachments::ProviderFileCacheLimits,
    ) -> Self {
        self.provider_file_cache = limits;
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

    pub fn process_tool_visibility_filter(
        mut self,
        filter: Arc<dyn facade_support::ProcessToolVisibilityFilter>,
    ) -> Self {
        self.process_tool_visibility_filter = Some(filter);
        self
    }

    /// Register tools under [`crate::tools::PLUGIN_TOOL_SOURCE_ID`], the owning
    /// plugin identity that deferred grants for these tools must name.
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

    /// Install the shared tracing runtime used by the engine and every plugin.
    pub fn trace_runtime(mut self, runtime: lash_core::runtime::TraceRuntime) -> Self {
        self.trace_runtime = Some(runtime);
        self
    }

    /// Installs the runtime's single admission, projection and metrics adapter.
    /// Replaces the previously configured adapter.
    #[cfg(feature = "otel-trace")]
    pub fn telemetry(mut self, telemetry: lash_trace::otel::OtelTelemetry) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Set the record sink, replacing sinks or paths configured through this builder.
    pub fn trace_sink(mut self, trace_sink: Arc<dyn lash_trace::TraceSink>) -> Self {
        self.trace_sink = Some(trace_sink);
        self
    }

    /// Set a JSONL record sink, replacing sinks or paths configured through this builder.
    pub fn trace_jsonl_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.trace_sink = Some(Arc::new(lash_trace::JsonlTraceSink::new(path.into())));
        self
    }

    pub fn trace_level(mut self, trace_level: lash_trace::TraceLevel) -> Self {
        self.trace_level = Some(trace_level);
        self
    }

    /// State whether built-in telemetry carries content: prompts, responses,
    /// rendered instructions, tool arguments and results, executed code and
    /// diagnostic or provider text. One choice governs every record sink and
    /// the telemetry adapter. Defaults to
    /// [`crate::tracing::TelemetryContent::standard`] (omitted): records keep
    /// identities, statuses and counts. It never changes what a session
    /// stores or returns.
    pub fn telemetry_content(mut self, content: crate::tracing::TelemetryContent) -> Self {
        self.telemetry_content = Some(content);
        self
    }

    /// Configure trace working capacities, evidence cuts and plugin-state warnings.
    /// Defaults to [`crate::tracing::TraceLimits::standard`]. No sink is installed.
    pub fn trace_limits(mut self, limits: crate::tracing::TraceLimits) -> Self {
        self.trace_limits = Some(limits);
        self
    }

    /// Configure expiry/publisher batches, process snapshot tails and session
    /// event deduplication. Defaults to [`crate::tracing::ObservationWorkLimits::standard`].
    pub fn observation_work_limits(
        mut self,
        limits: crate::tracing::ObservationWorkLimits,
    ) -> Self {
        self.observation_work_limits = limits;
        self
    }

    pub fn trace_context(mut self, trace_context: lash_trace::TraceContext) -> Self {
        self.trace_context = Some(trace_context);
        self
    }

    /// Required host choice for running with a missing persisted tool source.
    /// Both choices preserve typed tool-loss reporting.
    pub fn tool_source_policy(mut self, policy: lash_core::ToolSourcePolicy) -> Self {
        self.tool_source_policy = Some(policy);
        self
    }

    /// Required host choice of the shared execution bounds: the model call's
    /// hard total, the control-phase bound, stop grace, provider attempt limits
    /// and agent frame-switch limit. Tool and engine body bounds and park
    /// bounds belong to their own execution contracts. The
    /// stop grace also bounds how long a protocol-owned stream abort (an RLM
    /// cell boundary ending the model's turn) keeps draining the provider
    /// stream, so a cooperative provider's trailing usage lands on the
    /// aborted attempt.
    ///
    /// These bounds are the host's spend decision and have no default: a
    /// build without them is refused with
    /// [`EmbedError::MissingExecutionBudgets`]. A host with no numbers of its
    /// own states the named preset,
    /// [`ExecutionBudgets::recommended`](lash_core::ExecutionBudgets::recommended).
    pub fn execution_budgets(mut self, budgets: lash_core::ExecutionBudgets) -> Self {
        self.execution_budgets = Some(budgets);
        self
    }

    /// Required host choice of how a turn coalesces the prose and reasoning deltas of a
    /// stream block into frames before they reach the session's live feed:
    /// the frame interval, the frame size cap, whether a block's first delta
    /// is published at once, or [`DeltaCoalescing::off`](crate::DeltaCoalescing::off)
    /// for one event per delta. A frame is cut early by any other event, so
    /// the live feed keeps its order. Each frame is one live replay event
    /// named by the delta range it covers, so a redrive never duplicates or
    /// loses streamed text. Applies whatever live replay store the core
    /// uses.
    ///
    /// There is no default: a build without this choice is refused with
    /// [`EmbedError::MissingDeltaCoalescing`]. The named preset is
    /// [`DeltaCoalescing::recommended`](crate::DeltaCoalescing::recommended):
    /// 50 ms frames of at most 8 KiB, with an immediate first delta.
    pub fn delta_coalescing(mut self, coalescing: crate::DeltaCoalescing) -> Self {
        self.delta_coalescing = Some(coalescing);
        self
    }

    /// Configure how this deployment competes for the recovery leader lease
    /// (ADR 0109 §1.6): its build rank — a higher rank preempts a
    /// lower-ranked leader after the minimum tenure, so a rolling deploy that
    /// raises the rank per build hands recovery to the newest build — and the
    /// lease's cadence. Defaults to rank 0 on a 15 s TTL renewed every 5 s.
    pub fn recovery_lease(mut self, config: lash_core::engine::RecoveryLeaseConfig) -> Self {
        self.recovery_lease = Some(config);
        self
    }

    /// Bound every obligation delivery this core runs (ADR 0109 §1.8): each
    /// delivery attempt's budget, past which the attempt is abandoned and
    /// retried. The one policy source: the artifact-cleanup due pass and a
    /// producer's immediate `deliver_now` attempts run under it alike
    /// (FIG-4246). Defaults to a 30 s attempt budget. Keep it below the
    /// relay's 60 s claim TTL.
    pub fn recovery_pass_budget(mut self, budget: lash_core::engine::RecoveryPassBudget) -> Self {
        self.recovery_pass = budget;
        self
    }

    /// Configure attachment write-fence retries independently of retention.
    /// Omission selects the documented standard preset.
    pub fn attachment_reclamation_retry(
        mut self,
        policy: crate::persistence::AttachmentReclamationRetryPolicy,
    ) -> Self {
        self.attachment_reclamation_retry = policy;
        self
    }

    /// Pace registry awaiters. Omission selects [`crate::WorkCadencePolicy::standard`].
    pub fn work_cadence(mut self, policy: crate::WorkCadencePolicy) -> Self {
        self.work_cadence = policy;
        self
    }

    /// Configure obligation retries, claims and delivery budgets. Omission
    /// selects [`crate::RelayPolicy::standard`]. This and `recovery_pass_budget`
    /// share the delivery budget: the last call setting it wins.
    pub fn relay_policy(mut self, policy: crate::RelayPolicy) -> Self {
        self.recovery_pass.attempt = std::time::Duration::from_millis(policy.attempt_budget_ms);
        self.relay_policy = policy;
        self
    }

    /// Bound same-session admission waits. Omission selects the standard preset.
    pub fn commit_admission(mut self, policy: crate::CommitAdmissionPolicy) -> Self {
        self.commit_admission = policy;
        self
    }

    /// Pace facade reads and event buffers. Omission selects the standard preset.
    pub fn observer_pacing(mut self, pacing: crate::ObserverPacing) -> Self {
        self.observer_pacing = pacing;
        self
    }

    /// Set tool-fault retry pacing and checkpoint input chunks.
    /// Omission selects [`crate::RuntimePacingPolicy::standard`].
    pub fn runtime_pacing(mut self, pacing: crate::RuntimePacingPolicy) -> Self {
        self.runtime_pacing = pacing;
        self
    }

    /// Pace background cleanup and its page size. Omission selects the standard preset.
    pub fn recovery_pacing(mut self, pacing: crate::RecoveryPacing) -> Self {
        self.recovery_pacing = pacing;
        self
    }

    /// Replace the built-in live replay buffer used by session observation
    /// cursors with the host's own store, which carries the retention the
    /// host constructed it with;
    /// [`DataRetention::live_replay`](crate::DataRetention::live_replay)
    /// then configures nothing. This is best-effort reconnect recovery only;
    /// durable state still comes from the session store and
    /// [`SessionReadView`].
    pub fn live_replay_store(mut self, live_replay_store: Arc<dyn LiveReplayStore>) -> Self {
        self.live_replay_store = Some(live_replay_store);
        self
    }

    /// Replace the built-in process replay buffer behind process observation
    /// ([`Processes::observe`](crate::process::Processes::observe)) with the
    /// host's own store, which carries the retention the host constructed it
    /// with; [`DataRetention::process_replay`](crate::DataRetention::process_replay)
    /// then configures nothing. Cores that share one store share every
    /// process observation published to it. Durable process state still
    /// comes from the process registry.
    pub fn process_replay_store(
        mut self,
        process_replay_store: Arc<dyn lash_core::ProcessReplayStore>,
    ) -> Self {
        self.process_replay_store = Some(process_replay_store);
        self
    }

    /// Add a host sink for process events. Every event appended to a
    /// process's durable log, through the registry or by a commit of a
    /// process this core's node runs, reaches it after the commit: once
    /// each on this node, in sequence order per process. A node that takes
    /// a process over publishes from its durable publication mark, so an
    /// event may arrive again after a crash, under the same
    /// `(process_id, sequence)` (see [`facade_support::ProcessEventSink`]).
    /// It is a freshness feed, never truth:
    /// [`crate::process::Processes::events`] pages the log.
    pub fn process_event_sink(mut self, sink: Arc<dyn facade_support::ProcessEventSink>) -> Self {
        self.process_event_sinks.push(sink);
        self
    }

    /// Configure live graph history and fold batching separately from retention.
    /// Defaults to [`crate::process::ProcessObservationWorkLimits::standard`].
    pub fn process_observation_work_limits(
        mut self,
        limits: crate::process::ProcessObservationWorkLimits,
    ) -> Self {
        self.process_observation_work_limits = limits;
        self
    }

    /// Build a core under the host's stable worker identity.
    ///
    /// The owner id is stable for the worker or process and never scoped to a
    /// turn. The incarnation id changes once per process boot.
    pub fn build(mut self, runtime_owner: lash_core::LeaseOwnerIdentity) -> Result<LashCore> {
        let protocol_factory = self
            .protocol_factory
            .clone()
            .or_else(|| self.plugin_stack.protocol_factory().cloned());
        if protocol_factory.is_none() {
            return Err(EmbedError::MissingProtocolPlugin);
        }
        let backend = self.backend.clone();
        let store_factory = backend.session_store_factory();
        let data_retention = self
            .data_retention
            .take()
            .ok_or(EmbedError::MissingDataRetention)?;
        let core = self
            .resolve_runtime_host_config(lash_core::facade_support::DataRetentionConfig {
                attachments: data_retention.attachments,
                session_revisions: data_retention.session_revisions,
            })?
            .with_provider_file_uploaders(std::mem::take(&mut self.provider_file_uploaders));
        let process_observation_hub = Arc::new(
            crate::process_observation::ProcessObservationHub::new(
                data_retention.process_observation,
            )
            .with_work_limits(self.process_observation_work_limits),
        );
        let live_replay_store = self.live_replay_store.take().unwrap_or_else(|| {
            Arc::new(
                InMemoryLiveReplayStore::with_clock(
                    data_retention.live_replay,
                    Arc::clone(&core.clock),
                )
                .with_work_limits(core.observation_work_limits),
            )
        });
        let process_replay_store = self.process_replay_store.take().unwrap_or_else(|| {
            Arc::new(
                lash_core::InMemoryProcessReplayStore::with_clock(
                    data_retention.process_replay,
                    Arc::clone(&core.clock),
                )
                .with_work_limits(core.observation_work_limits),
            )
        });
        let language_observation_publisher = Arc::new(
            crate::language_observation::LanguageObservationPublisher::new(
                Arc::clone(&process_replay_store),
                Arc::clone(&live_replay_store),
            ),
        );
        let observation_sink: Arc<dyn lash_trace::TraceSink> =
            language_observation_publisher.clone();
        let observation_sink = match core.tracing.emitter().product_observer() {
            Some(configured) => Arc::new(lash_trace::TeeTraceSink::new([
                Arc::clone(configured),
                observation_sink,
            ])) as Arc<dyn lash_trace::TraceSink>,
            None => observation_sink,
        };
        let core = core.with_process_observation_sink(observation_sink);
        let process_effect_fold_budget = crate::process_feed::EffectFoldBudget {
            pages: data_retention.process_observation.snapshot_page_budget,
            page_size: data_retention.process_observation.snapshot_page_size,
        };
        // Appends through this core tick the other nodes' process change
        // hubs, and theirs tick this core's, through the backend's node hints.
        let watched = lash_core::runtime::watch_process_registry(backend.process_registry());
        watched.announce_through(backend.hints().clone());
        let process_work = lash_core::ProcessWorkWiring::new(
            watched,
            Arc::new(
                lash_core::DurableProcessWork::new(backend.clone())
                    .with_work_cadence(self.work_cadence.clone())?,
            ),
        );
        let process_lifecycle_feed = Arc::new(crate::process_lifecycle::ProcessLifecycleFeed::new(
            Arc::clone(&live_replay_store),
            Arc::clone(&process_observation_hub),
            Arc::clone(&language_observation_publisher),
        ));
        let process_lifecycle_sink: Arc<dyn facade_support::ProcessEventSink> =
            process_lifecycle_feed.clone();
        let process_event_registrations = Arc::new(
            std::iter::once(process_lifecycle_sink)
                .chain(std::mem::take(&mut self.process_event_sinks))
                .map(|sink| process_work.watched().add_event_sink(sink))
                .collect::<Vec<_>>(),
        );
        let mut plugin_factories = Vec::new();
        if !self.tool_providers.is_empty() {
            let spec = self
                .tool_providers
                .into_iter()
                .fold(PluginSpec::new(), PluginSpec::with_tool_provider);
            plugin_factories.push(Arc::new(StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial(
                    facade_support::PLUGIN_TOOL_SOURCE_ID,
                ),
                spec,
            )) as Arc<dyn PluginFactory>);
        }
        plugin_factories.extend(self.plugin_stack.into_factories());
        refuse_foreign_backend_factories(
            &backend,
            protocol_factory.iter().chain(plugin_factories.iter()),
        )?;
        let default_plugin_host = Arc::new(build_plugin_host(
            protocol_factory.as_ref(),
            &plugin_factories,
            &core,
        )?);
        // A plugin whose declaration contradicts itself refuses the build
        // before anything runs under it.
        default_plugin_host.composition()?;
        // Every backend supplies a process registry, so process lifecycle
        // is available on every core. Threaded to every plugin host so core
        // installs the same plugin-contributed process engines wherever it
        // rebuilds a runtime.
        let process_lifecycle_available = true;
        // Session construction still installs onto a clean clone so session-scoped plugin
        // overlays remain isolated.
        let host_process_engines = default_plugin_host
            .install_process_engine_contributions(core.clone(), process_lifecycle_available)?
            .process_engines;
        let process_registry = Arc::clone(process_work.registry());
        process_lifecycle_feed.bind_registry(Arc::clone(&process_registry));
        let env = RuntimeEnvironment::builder(core)
            .with_plugin_host(Arc::clone(&default_plugin_host))
            .with_process_work(process_work.clone())
            .build();
        let recovery = Arc::new(recovery::RecoverySlot::new(
            &env,
            self.recovery_lease
                .unwrap_or_else(lash_core::engine::RecoveryLeaseConfig::standard),
        ));
        // The artifact-cleanup outbox's due pass (ADR 0132 §12): a cleanup
        // whose producer died before its immediate attempt is delivered here.
        recovery.start_cleanup(
            Arc::new(
                lash_core::runtime::artifact_cleanup::ArtifactCleanupRelay::over_backend(
                    &backend,
                    host_process_engines.clone(),
                )
                .with_policy(env.core.control.relay_policy())
                .with_metrics(env.core.tracing.metrics().clone()),
            ),
            self.recovery_pacing,
        );
        let substrate = CoreWorkSetup {
            process: process_work,
        };

        let substrate_slot = Arc::new(CoreWorkSlot::new(substrate));
        let plugin_factories = Arc::new(plugin_factories);
        let core = LashCore {
            observer_pacing: Arc::new(self.observer_pacing),
            runtime_owner,
            env,
            backend,
            store_factory,
            process_registry,
            plugin_factories,
            live_replay_store,
            process_replay_store,
            language_observation_publisher,
            process_effect_fold_budget,
            process_observation_hub,
            process_lifecycle_feed,
            _process_event_registrations: process_event_registrations,
            protocol_factory,
            process_lifecycle_available,
            host_process_engines,
            substrate_slot,
            recovery,
            node: Arc::new(if self.serves_sessions {
                node::NodeSlot::new()
            } else {
                node::NodeSlot::detached()
            }),
        };
        core.node.ensure(&core);
        Ok(core)
    }
}

/// Refuses a plugin factory bound to a backend other than `backend`
/// ([`PluginFactory::bound_backend`]): its state would live in a substrate
/// this core neither reopens nor sweeps (ADR 0102, D2).
fn refuse_foreign_backend_factories<'a>(
    backend: &Backend,
    factories: impl IntoIterator<Item = &'a Arc<dyn PluginFactory>>,
) -> Result<()> {
    for factory in factories {
        if let Some(bound) = factory.bound_backend()
            && bound != backend.binding_identity().as_str()
        {
            return Err(EmbedError::PluginBackendMismatch {
                plugin_id: factory.id().to_string(),
                plugin_backend: bound.to_string(),
                backend: backend.binding_identity().to_string(),
            });
        }
    }
    Ok(())
}

/// The core's one plugin set: its protocol and its plugins.
pub(crate) fn build_plugin_host(
    protocol_factory: Option<&Arc<dyn PluginFactory>>,
    plugin_factories: &[Arc<dyn PluginFactory>],
    core: &RuntimeHostConfig,
) -> Result<PluginHost> {
    let mut factories =
        Vec::with_capacity(usize::from(protocol_factory.is_some()) + plugin_factories.len());
    if let Some(protocol_factory) = protocol_factory {
        factories.push(Arc::clone(protocol_factory));
    }
    factories.extend(plugin_factories.iter().cloned());
    let mut host = PluginHost::new(
        factories,
        core.control.execution_budgets.clone(),
        core.tracing.clone(),
    )
    .with_prompt_render_pool(core.control.prompt_render_pool.clone());
    if let Some(protocol) = protocol_factory {
        host = host.with_protocol_plugin(Arc::clone(protocol));
    }
    Ok(host)
}

impl LashCore {
    /// The live-replay cursor a reader should attach at after taking a durable
    /// snapshot of `session_id` at `revision`.
    ///
    /// A snapshot reader needs a cursor, and the only honest one names the
    /// replay incarnation that will serve it: a cursor naming any other
    /// incarnation is fenced at attach and answered with
    /// `replay_gap(unavailable)`, which sends the reader back for another
    /// snapshot, and round it goes (FIG-3162). This query reads the live-replay
    /// store only. Like [`Self::sessions`], it never opens a session or
    /// hydrates a checkpoint, so a host can pair it with a durable read.
    pub fn observation_cursor(
        &self,
        session_id: &SessionId,
        revision: lash_core::SessionRevision,
    ) -> lash_core::SessionCursor {
        self.live_replay_store.current_cursor(session_id, revision)
    }

    /// Enumerate every durable session catalog entry.
    ///
    /// This is a read-only catalog query. It does not open sessions, hydrate
    /// checkpoints, or mutate catalog generations.
    /// Results are ordered by creation time and then session id, and include
    /// permanent deletion tombstones.
    pub async fn sessions(&self) -> Result<Vec<SessionView>> {
        self.sessions_filtered(SessionListFilter::default()).await
    }

    /// Enumerate durable session catalog entries matching `filter`.
    ///
    /// Like [`Self::sessions`], this query never opens a session or acquires
    /// execution authority.
    pub async fn sessions_filtered(&self, filter: SessionListFilter) -> Result<Vec<SessionView>> {
        self.store_factory
            .list_sessions(&filter)
            .await
            .map_err(Into::into)
    }
}

/// Explicit host selection for a fork: the new session's id, its declared
/// lineage and the process runs it observes. What it forks is the
/// [`Target`](lash_core::Target) [`fork_at`](LashCore::fork_at) takes.
///
/// Lineage is independent of the forked session. Observers are the exact
/// runs the host selected; an empty list creates a history-only fork.
#[derive(Clone, Debug)]
pub struct ForkRequest {
    pub session_id: SessionId,
    pub relation: lash_core::SessionRelation,
    pub observed_processes: Vec<lash_core::ProcessId>,
}

#[cfg(all(test, feature = "otel-trace"))]
mod capacity_laws;
