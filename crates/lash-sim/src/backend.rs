//! The backends the simulator executes its runtimes over.
//!
//! A simulated turn runs where a deployment runs one: on lash's durable
//! engine ([`SimEngine`]), over a SQLite memory store set. The engine is
//! built through [`lash::durable::DurableBackendBuilder`], and the core a
//! world builds over it serves its own node, which runs every turn the
//! world sends (FIG-5172). When the simulator records the
//! checkpoint writes a run commits, it wraps the engine backend's session
//! factory in an observer and has the engine's durable port observe the
//! session commits its owners make; every other port stays the backend's.

use std::sync::Arc;

use lash_core::Backend;
use lash_core::sync::MutexExt as _;

use crate::runner::FixedScriptRunnerError;
use crate::store::{CheckpointWriteCollector, ObservedDeploymentStore};

/// Where the simulator runs turns: lash's durable engine over a SQLite
/// memory store set.
///
/// A turn is sent to the session and the engine runs it
/// ([`run_turn`](Self::run_turn)).
#[derive(Clone)]
pub struct SimEngine {
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    holds: Arc<crate::session_hold::SessionHolds>,
    observers: Arc<crate::session_hold::CommitObservers>,
    backend: Backend,
}

impl std::fmt::Debug for SimEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SimEngine").finish_non_exhaustive()
    }
}

/// Builds the send a turn starts from. The input is accepted on the
/// session and the engine runs it.
pub type SimTurnBuild =
    Arc<dyn Fn(&lash::LashSession) -> lash::Result<lash::SendBuilder> + Send + Sync>;

impl SimEngine {
    /// A fresh engine over a fresh SQLite memory store set. `_seed` names
    /// the world; the durable engine takes no seed of its own.
    pub async fn new(_seed: u64) -> Result<Self, FixedScriptRunnerError> {
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        let stores = Arc::new(stores);
        let holds = Arc::new(crate::session_hold::SessionHolds::default());
        let observers = Arc::new(crate::session_hold::CommitObservers::default());
        let backend = lash::durable::DurableBackendBuilder::new(Arc::new(
            crate::session_hold::HoldingStoreSet::new(
                stores.clone(),
                holds.clone(),
                observers.clone(),
            ),
        ))
        .build()
        .map_err(|err| FixedScriptRunnerError::Runtime(err.to_string()))?;
        Ok(Self {
            stores,
            holds,
            observers,
            backend,
        })
    }

    /// Hold `session`'s actor until the hold drops: what is sent to the
    /// session meanwhile stays pending, and no run of it goes on
    /// ([`crate::session_hold`]).
    pub fn hold_session(
        &self,
        session: &lash::SessionId,
    ) -> Result<crate::session_hold::SessionHold, FixedScriptRunnerError> {
        self.holds
            .hold(session)
            .map_err(FixedScriptRunnerError::Runtime)
    }

    /// The store set the engine runs over.
    pub fn stores(&self) -> &lash_sqlite_store::SqliteStoreSet {
        &self.stores
    }

    /// The backend a core runs on.
    pub fn backend(&self) -> Backend {
        DecoratedBackend::over_engine(self).into()
    }

    /// Run one turn of `session` on `prompt`, named `turn_id`, keeping only
    /// its output.
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
                Ok(session.send(lash::TurnInput::text(prompt.clone())))
            }),
        )
        .await
    }

    /// Send one turn of `session`, named `turn_id`, and wait for the engine
    /// to settle it, streaming its activity to `events`. The host never
    /// executes the run (D5): it accepts the input and waits. The outer
    /// result is the harness's; the inner one is the turn's own.
    pub async fn run_turn(
        &self,
        session: &lash::LashSession,
        turn_id: impl Into<lash::TurnId>,
        events: Arc<dyn lash::TurnActivitySink>,
        build: SimTurnBuild,
    ) -> Result<lash::Result<lash::TurnOutput>, FixedScriptRunnerError> {
        let collected = CollectedTurnActivity {
            live: Some(events),
            activities: std::sync::Mutex::new(Vec::new()),
        };
        let report = match build(session) {
            Ok(send) => match send.id(turn_id.into()).await {
                Ok(handle) => handle.output_into(&collected).await,
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        let activities = std::mem::take(&mut *collected.activities.lock_recover());
        Ok(report.map(|result| lash::TurnOutput { result, activities }))
    }
}

/// Wait for the engine to settle the input `handle` accepted, streaming its
/// activity to `events`, and keep that activity with the turn's output. The
/// host never executes the run (D5); it waits.
pub async fn settle_handle(
    handle: lash::SendHandle,
    events: Arc<dyn lash::TurnActivitySink>,
) -> lash::Result<lash::TurnOutput> {
    let collected = CollectedTurnActivity {
        live: Some(events),
        activities: std::sync::Mutex::new(Vec::new()),
    };
    let report = handle.output_into(&collected).await;
    let activities = std::mem::take(&mut *collected.activities.lock_recover());
    report.map(|result| lash::TurnOutput { result, activities })
}

/// The activity a turn streamed, kept for its output and forwarded live.
struct CollectedTurnActivity {
    live: Option<Arc<dyn lash::TurnActivitySink>>,
    activities: std::sync::Mutex<Vec<lash::TurnActivity>>,
}

#[async_trait::async_trait]
impl lash::TurnActivitySink for CollectedTurnActivity {
    async fn emit(&self, activity: lash::TurnActivity) {
        self.activities.lock_recover().push(activity.clone());
        if let Some(live) = &self.live {
            live.emit(activity).await;
        }
    }

    async fn emit_for_turn(&self, turn_id: &lash::TurnId, activity: lash::TurnActivity) {
        self.activities.lock_recover().push(activity.clone());
        if let Some(live) = &self.live {
            live.emit_for_turn(turn_id, activity).await;
        }
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
/// keeps the inner backend's Lash VM artifacts.
pub struct DecoratedBackend {
    layered: lash_core::testing::runtime_helpers::LayeredBackend,
    /// The engine's session-commit observers, for a backend over a
    /// [`SimEngine`].
    engine_observers: Option<Arc<crate::session_hold::CommitObservers>>,
}

impl DecoratedBackend {
    /// `inner`, undecorated.
    pub fn over(inner: Backend) -> Self {
        Self {
            layered: lash_core::testing::runtime_helpers::LayeredBackend::over(inner),
            engine_observers: None,
        }
    }

    /// `engine`'s backend, undecorated.
    pub fn over_engine(engine: &SimEngine) -> Self {
        Self {
            engine_observers: Some(engine.observers.clone()),
            ..Self::over(engine.backend.clone())
        }
    }

    /// Observe the commits made through the session factory into
    /// `collector`, and, over a [`SimEngine`], the session commits its
    /// owners make.
    pub fn observing(self, collector: CheckpointWriteCollector) -> Self {
        if let Some(observers) = &self.engine_observers {
            observers.observe(collector.clone());
        }
        Self {
            layered: self.layered.map_session_store_factory(|factory| {
                Arc::new(ObservedDeploymentStore::new(factory, collector))
            }),
            engine_observers: self.engine_observers,
        }
    }
}

impl From<DecoratedBackend> for Backend {
    fn from(backend: DecoratedBackend) -> Self {
        backend.layered.into_backend()
    }
}
