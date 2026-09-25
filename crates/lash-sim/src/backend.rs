//! The backends the simulator drives its runtimes over.
//!
//! A simulated turn runs where a deployment runs one: inside a handler of
//! lash-restate's engine, on the in-process Restate server double
//! ([`SimEngine`]), over a SQLite memory store set; SQLite is storage only.
//! When the simulator records the checkpoint writes a run commits, it wraps
//! the engine backend's session factory in an observer, and every other port
//! stays the backend's.

use std::sync::Arc;

use lash_core::sync::MutexExt as _;
use lash_core::{Backend, SessionStoreFactory};

use crate::runner::FixedScriptRunnerError;
use crate::store::{CheckpointWriteCollector, ObservedSessionStoreFactory};

/// Where the simulator runs turns: lash-restate's engine on a fresh
/// in-process Restate server double under the scenario's seed, with serial
/// scheduling, over a SQLite memory store set.
///
/// The engine refuses an effect outside a handler, so a turn enters one
/// through [`run_turn`](Self::run_turn), and a core that starts processes
/// serves their segments through [`serve_processes`](Self::serve_processes).
#[derive(Clone, Debug)]
pub struct SimEngine {
    restate: lash_restate_test::RestateTestBackend,
}

/// Builds the turn a handler attempt runs. Restate re-runs a handler from the
/// top on every replay, so the turn is built afresh on each attempt.
pub type SimTurnBuild =
    Arc<dyn Fn(&lash::LashSession) -> lash::Result<lash::TurnBuilder> + Send + Sync>;

/// Builds the queued drain a handler attempt runs, afresh on each attempt.
pub type SimQueuedTurnBuild =
    Arc<dyn Fn(&lash::LashSession) -> lash::QueuedTurnBuilder + Send + Sync>;

impl SimEngine {
    /// A fresh engine on a server double under `seed`, scheduled serially:
    /// one attempt runs at a time, and one seed grants the turn in one order
    /// on a current-thread runtime.
    pub async fn new(seed: u64) -> Result<Self, FixedScriptRunnerError> {
        Self::scheduled(seed, lash_restate_test::Scheduling::Serial).await
    }

    /// A fresh engine on a server double under `seed` whose live attempts
    /// run whenever Tokio polls them, as `restate-server` does: the
    /// generated search lane's cross-session concurrency. A scenario that
    /// stops a running turn from outside it needs this too: a host-local stop
    /// is a durable request on the turn's cancellation gate, an ingress call,
    /// and serial scheduling lands ingress only between attempts, so it would
    /// wait on the very attempt it stops (FIG-3672 P9).
    pub async fn concurrent(seed: u64) -> Result<Self, FixedScriptRunnerError> {
        Self::scheduled(seed, lash_restate_test::Scheduling::Concurrent).await
    }

    async fn scheduled(
        seed: u64,
        scheduling: lash_restate_test::Scheduling,
    ) -> Result<Self, FixedScriptRunnerError> {
        let mut config = lash_restate_test::ServerConfig::default().scheduling(scheduling);
        // A crashed attempt is retried at once: retry timing is no contract,
        // and a simulated world crashes an attempt on every durable effect.
        config.retry.initial_interval = std::time::Duration::from_millis(1);
        config.retry.max_interval = std::time::Duration::from_millis(10);
        lash_restate_test::backend(seed, config)
            .await
            .map(|restate| Self { restate })
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))
    }

    /// The server double and the engine wired to it.
    pub fn restate(&self) -> &lash_restate_test::RestateTestBackend {
        &self.restate
    }

    /// The backend a core runs on: the engine's own backend, with its
    /// Lashlang artifacts in the engine's store set.
    pub fn backend(&self) -> Arc<DecoratedBackend> {
        Arc::new(DecoratedBackend::over_engine(self))
    }

    /// Serve process segments with `core`'s durable worker, as a Restate
    /// deployment does.
    pub fn serve_processes(&self, core: &lash::LashCore) -> Result<(), FixedScriptRunnerError> {
        let config = core
            .durable_process_worker_config()
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        self.restate.install_process_worker(
            lash::durability::DurableProcessWorker::new(config)
                .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?,
        );
        Ok(())
    }

    /// Drain `session`'s next claimable queued work as one turn inside a
    /// handler on the server, under the idempotent drain id `drain_id`. The
    /// outer result is the handler's; the inner one is the drain's own.
    pub async fn run_queued_turn(
        &self,
        session: &lash::LashSession,
        drain_id: impl Into<String>,
        build: SimQueuedTurnBuild,
    ) -> Result<lash::Result<lash::QueuedTurnDrain<lash::TurnOutput>>, FixedScriptRunnerError> {
        let drain_id = drain_id.into();
        let admitted = lash_core::AdmittedScope::unpinned(lash_core::ExecutionScope::queue_drain(
            session.session_id(),
            drain_id.clone(),
        ))
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        type Drained = lash::Result<lash::QueuedTurnDrain<lash::TurnOutput>>;
        let slot: Arc<std::sync::Mutex<Option<Drained>>> = Arc::new(std::sync::Mutex::new(None));
        let attempt: lash_restate_test::HandlerAttempt = {
            let session = session.clone();
            let slot = Arc::clone(&slot);
            Arc::new(move |scoped| {
                let session = session.clone();
                let drain_id = drain_id.clone();
                let build = Arc::clone(&build);
                let slot = Arc::clone(&slot);
                Box::pin(async move {
                    let collected = CollectedTurnActivity::default();
                    let drained = build(&session)
                        .drain_id(drain_id)
                        .advanced()
                        .stream_to_with_scope(&collected, scoped)
                        .await;
                    let activities = std::mem::take(&mut *collected.activities.lock_recover());
                    *slot.lock_recover() =
                        Some(drained.map(|drain| {
                            drain.map(|result| lash::TurnOutput { result, activities })
                        }));
                })
            })
        };
        self.restate
            .run_in_handler(admitted, attempt)
            .await
            .map_err(FixedScriptRunnerError::Runtime)?;
        slot.lock_recover().take().ok_or_else(|| {
            FixedScriptRunnerError::Runtime("the drain's handler recorded no outcome".to_string())
        })
    }

    /// Run one turn of `session` on `prompt`, named `turn_id`, inside a
    /// handler on the server, keeping only its output.
    pub async fn run_text_turn(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        prompt: impl Into<String>,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        let prompt = prompt.into();
        self.run_turn(
            session,
            turn_id,
            Arc::new(DiscardedTurnActivity),
            Arc::new(move |session: &lash::LashSession| {
                Ok(session.turn(lash::TurnInput::text(prompt.clone())))
            }),
        )
        .await
    }

    /// Run one turn of `session`, named `turn_id`, inside a handler on the
    /// server, streaming its activity to `events`. The outer result is the
    /// handler's; the inner one is the turn's own.
    pub async fn run_turn(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        events: Arc<dyn lash::TurnActivitySink>,
        build: SimTurnBuild,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        let turn_id = turn_id.into();
        let admitted = lash_core::AdmittedScope::unpinned(session.turn_scope(turn_id.clone()))
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let slot: Arc<std::sync::Mutex<Option<lash::Result<lash::TurnOutput>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let attempt: lash_restate_test::HandlerAttempt = {
            let session = session.clone();
            let slot = Arc::clone(&slot);
            Arc::new(move |scoped| {
                let session = session.clone();
                let turn_id = turn_id.clone();
                let events = Arc::clone(&events);
                let build = Arc::clone(&build);
                let slot = Arc::clone(&slot);
                Box::pin(async move {
                    let result = async {
                        build(&session)?
                            .turn_id(turn_id)
                            .advanced()
                            .collect_with_scope(events.as_ref(), scoped)
                            .await
                    }
                    .await;
                    *slot.lock_recover() = Some(result);
                })
            })
        };
        self.restate
            .run_in_handler(admitted, attempt)
            .await
            .map_err(FixedScriptRunnerError::Runtime)?;
        slot.lock_recover().take().ok_or_else(|| {
            FixedScriptRunnerError::Runtime("the turn's handler recorded no report".to_string())
        })
    }
}

/// The activity a queued drain's turn streamed, kept for its output.
#[derive(Default)]
struct CollectedTurnActivity {
    activities: std::sync::Mutex<Vec<lash::TurnActivity>>,
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for CollectedTurnActivity {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.activities.lock_recover().push(activity);
    }
}

/// A turn's live activity nobody watches; its output keeps the activity.
pub struct DiscardedTurnActivity;

#[async_trait::async_trait]
impl lash::TurnActivitySink for DiscardedTurnActivity {
    async fn emit(&self, _activity: lash::TurnActivity) {}
}

/// The engine's backend with its session factory decorated.
///
/// A checkpoint-write observer over the session factory forwards every
/// binding to the factory it wraps, so the backend's ports still meet each
/// other exactly as the undecorated backend's do. The decorated backend
/// keeps the inner backend's Lashlang artifacts.
pub struct DecoratedBackend {
    inner: Arc<dyn Backend>,
    factory: Arc<dyn SessionStoreFactory>,
}

impl DecoratedBackend {
    /// `engine`'s backend, undecorated: it reaches the server double only
    /// through its connection, so a core over it never keeps the server
    /// alive.
    pub fn over_engine(engine: &SimEngine) -> Self {
        let inner = engine.restate.lash_backend();
        Self {
            factory: inner.session_store_factory(),
            inner,
        }
    }

    /// Observe the commits made through the session factory into
    /// `collector`.
    pub fn observing(mut self, collector: CheckpointWriteCollector) -> Self {
        self.factory = Arc::new(ObservedSessionStoreFactory::new(self.factory, collector));
        self
    }
}

impl Backend for DecoratedBackend {
    fn binding_identity(&self) -> &str {
        self.inner.binding_identity()
    }

    fn clock(&self) -> Arc<dyn lash_core::Clock> {
        self.inner.clock()
    }

    fn session_store_factory(&self) -> Arc<dyn SessionStoreFactory> {
        Arc::clone(&self.factory)
    }

    fn effect_host(&self) -> Arc<dyn lash_core::EffectHost> {
        self.inner.effect_host()
    }

    fn process_registry(&self) -> Arc<dyn lash_core::ProcessRegistry> {
        self.inner.process_registry()
    }

    fn trigger_store(&self) -> Arc<dyn lash_core::TriggerStore> {
        self.inner.trigger_store()
    }

    fn process_definition_registry(&self) -> Arc<dyn lash_core::ProcessDefinitionRegistry> {
        self.inner.process_definition_registry()
    }

    fn process_env_store(&self) -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
        self.inner.process_env_store()
    }

    fn attachment_store(&self) -> Arc<dyn lash_core::AttachmentStore> {
        self.inner.attachment_store()
    }

    fn module_artifacts(&self) -> Arc<dyn lash_core::ModuleArtifactStore> {
        self.inner.module_artifacts()
    }

    fn process_work(&self) -> Option<lash_core::ProcessWorkWiring> {
        self.inner.process_work()
    }

    fn queued_work(&self) -> lash_core::BackendQueuedWork {
        self.inner.queued_work()
    }
}
