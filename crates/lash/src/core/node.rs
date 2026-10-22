//! The core's node (ADR 0132 §3): the session and process actors of the
//! core's backend run on it, their turns in runtimes this core opens.
//!
//! Its processes run on the core's durable process worker: a `SessionTurn`
//! process mails its turn to its child session, and an engine process's
//! steps run the process's catalog tools and its engine's own bodies
//! (`ProcessSteps`).
//! The node advances every engine the core registers, a plugin's as a
//! host's.
//!
//! The node starts with the core, on the runtime the core is built in (or on
//! the first session the core opens, when it was built outside one), and
//! stops when the core shuts down or its last clone is dropped. Its name is
//! the core's stable owner id: a new boot of the same owner fences the old.
//!
//! While the core lives, so does its node (FIG-6887). A node that stopped
//! itself unrenewed (the process stalled past the lease's `self_stop_after`)
//! serves again as a new boot, which fences the old one and resumes every
//! actor it held, its listener holding the liveness lock again; so does a
//! node whose start a transient store failure refused. It stays stopped
//! only when a newer boot of its owner replaced it or the store refused it
//! for good.
//!
//! The host drains it by release (ADR 0106 §1) with [`LashCore::drain`]: the
//! node claims nothing more, hands each actor it owns to the next build at a
//! committed phase, and stops. A drained core never starts a node again.

use std::sync::{Arc, Mutex};

use lash_core::ExecutionBudgets;
use lash_core::LiveReplayStore;
use lash_core::durable_port::DurableError;
use lash_core::durable_port::DurableProbe;
use lash_core::durable_port::runner::{Activation, Drain, Stopped};
use lash_core::facade_support::LashRuntime;
use lash_core::runtime::durable::ProcessActivation;
use lash_core::runtime::durable::node::{NodeServe, serve};
use lash_core::runtime::durable::services::{RuntimeTurnServices, SessionRuntimes};
use lash_core::runtime::durable::session::{SessionActivation, TurnError, TurnServices};
use lash_vocabulary::identity::SessionId;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::LashCore;
use super::drain::{NodeDrainError, NodeDrainReport};

/// How the node's serving ended.
type Served = Result<Stopped, DurableError>;

/// The core's one node, shared by its clones.
pub(crate) struct NodeSlot {
    pub(crate) registration: lash_core::durable_port::runner::NodeRegistration,
    stop: CancellationToken,
    /// The node's drain switch; started, the node never starts again.
    drain: Drain,
    /// Whether the node started.
    started: Mutex<bool>,
    /// How the node's serving ended, once it has.
    served: watch::Sender<Option<Served>>,
}

impl NodeSlot {
    pub(crate) fn new() -> Self {
        Self {
            registration: Default::default(),
            stop: CancellationToken::new(),
            drain: Drain::default(),
            started: Mutex::new(false),
            served: watch::Sender::new(None),
        }
    }

    /// A slot that never starts a node: a core's that serves no sessions,
    /// and the one the node's own core clone holds.
    pub(crate) fn detached() -> Self {
        let slot = Self::new();
        slot.stop.cancel();
        slot
    }

    /// Start `core`'s node once, on the current runtime; without one it
    /// starts on the next call made on a runtime.
    pub(crate) fn ensure(&self, core: &LashCore) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Ok(mut started) = self.started.lock() else {
            return;
        };
        if *started || self.stop.is_cancelled() || self.drain.started() {
            return;
        }
        let NodeActivations {
            backend,
            sessions,
            processes,
            serves,
            offers,
        } = core.node_activations(Arc::new(lash_core::durable_port::NoProbe));
        let processes = match processes {
            Ok(processes) => Some(processes),
            Err(error) => {
                tracing::error!(%error, "the core's node serves no processes");
                None
            }
        };
        let node = lash_core::durable_port::NodeId::new(core.runtime_owner.owner_id.clone());
        let drain = self.drain.clone();
        let stop = self.stop.clone();
        let served = self.served.clone();
        let registration = self.registration.clone();
        *started = true;
        runtime.spawn(async move {
            let clock = backend.clock();
            let retry_after = backend.config().lease().settings().claim_poll;
            let outcome = loop {
                let outcome = serve(
                    &backend,
                    NodeServe {
                        registration: registration.clone(),
                        node: node.clone(),
                        drain: drain.clone(),
                        sessions: Arc::clone(&sessions),
                        processes: processes.clone(),
                        serves: serves.clone(),
                        offers: offers.clone(),
                    },
                    stop.clone().cancelled_owned(),
                )
                .await;
                if stop.is_cancelled() || drain.started() {
                    break outcome;
                }
                match &outcome {
                    // The process stalled past `self_stop_after` (a starved
                    // host): the node stopped itself and dropped its actors.
                    // Its next boot fences this one and resumes them.
                    Ok(Stopped::Unrenewed) => {
                        tracing::warn!(
                            "the core's node went unrenewed; it serves again as a new boot"
                        );
                    }
                    Err(error) if crate::error::durable_error_is_retryable(error) => {
                        tracing::warn!(%error, "the core's node did not start; it starts again");
                        tokio::select! {
                            () = stop.cancelled() => break outcome,
                            () = clock.sleep(retry_after) => {}
                        }
                    }
                    _ => break outcome,
                }
            };
            match &outcome {
                Ok(Stopped::Requested | Stopped::Drained) => {
                    tracing::debug!(?outcome, "the core's node stopped");
                }
                Ok(stopped) => tracing::warn!(?stopped, "the core's node stopped on its own"),
                Err(error) => tracing::error!(%error, "the core's node stopped"),
            }
            // Restart retains these activations between boots. Release their
            // core clones before shutdown can observe the node as stopped.
            drop(sessions);
            drop(processes);
            drop(backend);
            served.send_replace(Some(outcome));
        });
    }

    /// How the node's serving ended, once it has; `None` when it never
    /// started.
    async fn ended(&self) -> Option<Served> {
        if !self.started.lock().is_ok_and(|started| *started) {
            return None;
        }
        let mut served = self.served.subscribe();
        let ended = served.wait_for(Option::is_some).await;
        ended.ok().and_then(|ended| ended.clone())
    }

    /// Drain `core`'s node by release and wait until it stops.
    pub(crate) async fn drain(&self, core: &LashCore) -> Result<NodeDrainReport, NodeDrainError> {
        // A core built outside a runtime starts its node now, so a drain
        // always meets a node that registered.
        self.ensure(core);
        self.drain.start();
        match self.ended().await {
            Some(Ok(Stopped::Drained)) => Ok(NodeDrainReport::of(&self.drain.released())),
            Some(Ok(stopped)) => Err(NodeDrainError::Stopped(stopped)),
            Some(Err(error)) => Err(NodeDrainError::Store(error)),
            None => Err(NodeDrainError::NotServing),
        }
    }

    /// How `core`'s node stopped, once it has; `None` when it runs none.
    pub(crate) async fn stopped(&self, core: &LashCore) -> Option<Served> {
        // A core built outside a runtime starts its node now.
        self.ensure(core);
        self.ended().await
    }

    /// Stop the node and wait for it.
    pub(crate) async fn stop(&self) {
        self.stop.cancel();
        let _ = self.ended().await;
    }
}

impl Drop for NodeSlot {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// The runtimes a core's node runs its sessions' turns in.
struct CoreRuntimes(LashCore);

#[async_trait::async_trait]
impl SessionRuntimes for CoreRuntimes {
    async fn open(&self, session: &SessionId) -> Result<LashRuntime, TurnError> {
        match self.0.session(session.clone()).open_runtime().await {
            Ok(runtime) => Ok(runtime),
            Err(crate::EmbedError::Runtime(error)) => Err(TurnError::Runtime(error)),
            // The store's refusal keeps its code and cause, so one no pass
            // would clear answers the run it blocks (FIG-5358).
            Err(
                crate::EmbedError::Store(error)
                | crate::EmbedError::Session(lash_core::SessionError::Store {
                    source: error, ..
                }),
            ) => Err(TurnError::Runtime(error.runtime_error())),
            Err(error) => Err(TurnError::Exec(format!(
                "session {session} did not open: {error}"
            ))),
        }
    }

    fn live_replay(&self) -> Arc<dyn LiveReplayStore> {
        Arc::clone(&self.0.live_replay_store)
    }

    fn execution_budgets(&self) -> ExecutionBudgets {
        self.0.env.core.control.execution_budgets.clone()
    }

    fn tracing(&self) -> &lash_core::runtime::TraceRuntime {
        &self.0.env.core.tracing
    }
}

/// What the core's node serves.
pub(crate) struct NodeActivations {
    /// The core's backend, advancing every engine the core registers.
    pub(crate) backend: lash_execution_contracts::backend::Backend,
    /// Runs the claimed sessions' turns.
    pub(crate) sessions: Arc<SessionActivation>,
    /// Runs the claimed processes on the core's process worker, or why the
    /// worker did not build.
    pub(crate) processes: crate::Result<Arc<dyn Activation>>,
    /// The claim tokens the core's node serves.
    pub(crate) serves: Vec<String>,
    /// What the core's registrations offer running code: its providers and
    /// the codec versions they read.
    pub(crate) offers: lash_core::durable_port::NodeCapabilities,
}

impl LashCore {
    /// The backend and activations the core's node serves, each reporting
    /// to `probe`. The node opens runtimes through a clone of the core that
    /// holds no node of its own, so the node never keeps its core alive.
    pub(crate) fn node_activations(&self, probe: Arc<dyn DurableProbe>) -> NodeActivations {
        let mut backend = self
            .backend
            .with_process_engines(self.host_process_engines.engines().cloned());
        // An engine that carries the previous build's state forward makes
        // this node a claimer of the processes that build parked.
        for (kind, migration) in self.host_process_engines.state_migrations() {
            backend = backend.with_state_migration(
                kind,
                Arc::clone(migration),
                &crate::formats::previous_actor_state_surfaces(),
            );
        }
        // An engine that projects a status has it recorded with each of its
        // processes' transitions.
        backend = backend.with_status_projections(self.host_process_engines.status_projections());
        let sessions = Arc::new(SessionActivation::new(
            backend.clone(),
            self.turn_services(),
            Arc::clone(&probe),
        ));
        let processes = self.process_worker().map(|worker| {
            let worker = Arc::new(worker);
            Arc::new(
                ProcessActivation::new(
                    backend.clone(),
                    lash_core_worker::process_steps(&worker),
                    probe,
                )
                .with_tracing(self.env.core.tracing.clone())
                .with_session_turns(worker)
                .with_process_events(self.substrate_slot.setup.process.watched().clone()),
            ) as Arc<dyn Activation>
        });
        NodeActivations {
            backend,
            sessions,
            processes,
            serves: self.node_serves(),
            offers: self.node_offers(),
        }
    }

    /// The claim tokens the core's node serves (S7): projected from its
    /// installed registrations when the core was built.
    pub(crate) fn node_serves(&self) -> Vec<String> {
        self.serves.to_vec()
    }

    /// What the core's node's registrations offer running code
    /// (FIG-6993): projected from its installed registrations when the
    /// core was built.
    pub(crate) fn node_offers(&self) -> lash_core::durable_port::NodeCapabilities {
        lash_core::durable_port::NodeCapabilities::clone(&self.offers)
    }

    /// The hub that ticks when a commit grew a process's log, on this
    /// core's node or, through its node wakes, on another.
    #[cfg(test)]
    pub(crate) fn process_changes(&self) -> &lash_core::runtime::ProcessChangeHub {
        self.substrate_slot.setup.process.watched().hub()
    }

    /// The watch over this core's process registry, which publishes what an
    /// owner's commit appended.
    #[cfg(test)]
    pub(crate) fn process_watch(&self) -> &lash_core::runtime::WatchedRegistry {
        self.substrate_slot.setup.process.watched()
    }

    /// The hub that ticks once this core's sinks, its observation
    /// dispatcher among them, were handed what a commit appended to a
    /// process's log, and when another node appended to it.
    pub(crate) fn process_emissions(&self) -> &lash_core::runtime::ProcessChangeHub {
        self.substrate_slot.setup.process.watched().emitted_hub()
    }

    /// The worker the core's node runs its processes on.
    fn process_worker(&self) -> crate::Result<lash_core_worker::DurableProcessWorker> {
        Ok(lash_core_worker::DurableProcessWorker::new(
            self.durable_process_worker_config()?,
        ))
    }

    /// The turn services a node runs this core's sessions' turns with.
    pub(crate) fn turn_services(&self) -> Arc<dyn TurnServices> {
        Arc::new(RuntimeTurnServices::new(
            Arc::new(CoreRuntimes(self.detached())),
            Arc::clone(&self.published_heads),
        ))
    }

    /// This core without its node: what the node opens runtimes through.
    fn detached(&self) -> Self {
        Self {
            node: Arc::new(NodeSlot::detached()),
            ..self.clone()
        }
    }
}

#[cfg(all(test, feature = "codemode"))]
mod tests {
    use super::*;
    use futures_util::StreamExt as _;

    /// Observe committed process events until the engine reaches its first sleep.
    async fn first_park(
        core: &LashCore,
        process: &lash_core::ProcessId,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let observable = core.processes().observe(process);
        let snapshot = observable.snapshot().await?;
        let mut events = observable.subscribe_and_recover(snapshot.cursor);
        loop {
            let record = core
                .processes()
                .get(process)
                .await?
                .ok_or("the process is retained")?;
            for wait in record.lifecycle.waits() {
                if let lash_execution_contracts::process::WaitKind::Sleep { until_ms } = wait.kind {
                    return Ok(until_ms);
                }
            }
            tokio::select! {
                event = events.next() => { event.ok_or("the process feed remains open")??; }
                stopped = core.node_stopped() => return Err(format!("the live node stopped before the process parked: {stopped:?}").into()),
            }
        }
    }

    /// FIG-7137: a serving node must survive its own expiry scan after a wall
    /// clock jump, then run a newly admitted process. Execute the ordinary
    /// reaper operation explicitly so the scan precedes the new start without
    /// a sleep or a race against the heartbeat timer.
    #[tokio::test]
    async fn a_node_starts_new_work_after_its_clock_jumps_past_its_lease() {
        let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
                .await
                .unwrap(),
        );
        let workers = crate::vm::WorkerService::default();
        let bounds = workers.config().run_bounds;
        let backend = lash_conformance::backend_over(stores);
        let kernel = Arc::new(crate::vm::KernelProcessPluginFactory::new(
            workers,
            crate::vm::TYPESCRIPT_DIALECT,
            bounds,
        ));
        let core = LashCore::standard_builder(backend)
            .process_component(
                Arc::new(crate::vm::KernelServicesComponent::new()),
                crate::vm::KERNEL_SERVICES_CONFIG,
            )
            .plugin(kernel)
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(crate::DataRetention::standard())
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(crate::tests::host::runtime_lease_owner())
            .unwrap();
        let artifacts = core.host_artifacts();
        let pin = crate::process::HostArtifactPin::mint();
        let base = crate::process::ProcessEnv::builder(crate::MaxToolCalls::new(16))
            .component(crate::vm::KERNEL_SERVICES_CONFIG, &())
            .build()
            .unwrap();
        let context = lash_vocabulary::PublicationContext {
            domain: lash_vocabulary::PublicationDomain(
                core.backend().binding_identity().to_string(),
            ),
            scope: lash_vocabulary::PublicationScope::Host,
            publisher: lash_vocabulary::PublisherInstance("node-clock-law".into()),
            root: lash_vocabulary::Segment::new("workflow").unwrap(),
        };
        let packages = artifacts
            .process_namespace(&pin, &base, context.clone())
            .await
            .unwrap();
        let environment = crate::vm::WorkflowEnvironmentRequest {
            snapshot: packages.snapshot.clone(),
            context,
            attachment: lash_vocabulary::Attachment {
                packages: [lash_vocabulary::PackagePath::parse("typescript").unwrap()].into(),
                members: Default::default(),
                deferred: Default::default(),
            },
        };
        let definition = lash_kernel_doc::parse_definition(
            "function nap() -> Null\nkernel 1\nmetering statements\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nbody { do perform 0(600000) as Null return null }",
        )
        .unwrap();
        let name = lash_vocabulary::Name::new("nap");
        let mut document =
            lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
        document.main = None;
        document.imports = definition.body().unwrap().imports.clone();
        document.exports.insert(
            name.clone(),
            lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
                function: definition.identity(),
                signature: definition.signature.clone(),
                convention: lash_kernel_doc::StartConvention::Direct,
                asynchronous: false,
                rest: false,
            }),
        );
        document.functions.insert(definition.identity(), definition);
        let linked = artifacts
            .publish_workflow(
                &pin,
                &document.into(),
                &lash_vocabulary::StartSelector::Export(name),
                &environment,
                None,
            )
            .await
            .unwrap();
        let process_env = artifacts
            .link_process_env(&base, &linked, &packages, &Default::default())
            .await
            .unwrap();
        let env_ref = artifacts
            .publish_process_env(&pin, &process_env)
            .await
            .unwrap();
        let draft = crate::vm::definition_draft(&crate::process::KernelProcessDefinition {
            code: crate::vm::KernelInlineRequest::Linked { start: linked },
        })
        .unwrap();
        let publication = artifacts.publish_definition(&pin, &draft).await.unwrap();
        let start = || async {
            core.processes()
                .start(
                    crate::process::ProcessStartRequest::definition(
                        publication.id.clone(),
                        Some(publication.signature.clone()),
                        serde_json::Map::new(),
                        crate::process::EngineLaunch::new(
                            env_ref.clone(),
                            Some(
                                core.processes()
                                    .kernel_engine_defaults()
                                    .unwrap()
                                    .into_engine_settings(),
                            ),
                        ),
                        crate::process::ProcessOriginator::host(),
                        crate::process::Lifetime::Detached,
                    ),
                    core.effect_host(),
                )
                .await
                .unwrap()
                .process_id
        };
        let before = start().await;
        assert_eq!(first_park(&core, &before).await.unwrap(), 1_700_000_600_000);
        let lease = core
            .node
            .registration
            .snapshot()
            .expect("the first park proves the node registered");
        let jumped = 1_700_000_060_000;
        assert!(jumped > lease.expires_at.0);
        clock.set(u64::try_from(jumped).unwrap());
        core.backend().durable().reap(&lease).await.unwrap();
        let after = start().await;
        let parked = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            first_park(&core, &after),
        )
        .await
        .expect("new work reaches its first park")
        .expect("the process observation reads");
        assert_eq!(parked, jumped + 600_000);
        assert!(matches!(
            core.backend().durable().heartbeat(&lease).await.unwrap(),
            lash_core::durable_port::HeartbeatOutcome::Renewed { expires_at }
                if expires_at.0 == jumped + lease.ttl_millis
        ));
        core.shutdown().await.unwrap();
    }
}
