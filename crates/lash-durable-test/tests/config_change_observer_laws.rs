//! The session config-change laws (FIG-5333, FIG-5397): a config
//! transaction a host commands on a durable session reaches the plugins
//! that observe session config changes after its commit. Transactions still
//! apply when plugins cannot build; owed changes coalesce to the latest
//! transition until a successful build (FIG-5317). A node lost between
//! its commit and its delivery leaves the change owed, so the session's next
//! plugin build delivers it.
//!
//! The first laws run a lash core serving its own node over one tier's
//! database. The host applies a transaction through the session's config
//! admin; the core's node applies it in a command run, and a later `send()`
//! runs a turn after it in the same session actor, so the command run,
//! delivery included, has finished once the turn answers.
//!
//! The crash law runs the session on simulated nodes A and B over the
//! production durable store, and cuts every `session.command` write.
// Test code: the PostgreSQL leg reads its database URL from the environment.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/dialect.rs"]
mod dialect;
#[path = "support/matrix.rs"]
mod matrix;
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::config::{ConfigTransaction, ConfigTransactionOutcome, ConfigWrite, SetMaxToolCalls};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core_execution::StoreSet;
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{Fault, Matrix, Scenario, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::sync::MutexExt as _;
use matrix::MatrixTestExt as _;
use served::{Tier, World};

/// The observing plugin's id.
const PLUGIN: &str = "config-change-observer";
/// The tool-call limit a session is created with.
const BEFORE: usize = 8;
/// The tool-call limit the transaction sets.
const AFTER: usize = 16;

/// One `SessionConfigChanged` the plugin saw: the change's identity, the
/// tool-call limits of its previous and current policy, and the one the
/// session's read service answered while the observer ran.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    session: String,
    revision: u64,
    previous: usize,
    current: usize,
    served: usize,
}

/// A plugin that records every session config change it observes. While
/// `failing` is set a session it is part of does not build.
#[derive(Clone, Default)]
struct Observer {
    seen: Arc<Mutex<Vec<Seen>>>,
    failing: Arc<AtomicBool>,
}

impl Observer {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock_recover().clone()
    }
}

impl lash::plugins::PluginDefinition for Observer {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PLUGIN)
    }
}

impl lash::plugins::PluginFactory for Observer {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    fn build(
        &self,
        _: &lash::plugins::PluginSessionContext,
    ) -> Result<Arc<dyn lash::plugins::SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash::plugins::SessionPlugin for Observer {
    fn id(&self) -> &'static str {
        PLUGIN
    }

    /// A session's build ends here; inspecting the plugin's registrations,
    /// which a config transaction's resolution does, never reaches it.
    fn session_ready(
        &self,
        _: lash::plugins::SessionReadyContext,
    ) -> Result<(), lash::plugins::PluginError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(lash::plugins::PluginError::Registration(
                "the observer's session does not build".to_owned(),
            ));
        }
        Ok(())
    }

    fn register(
        &self,
        reg: &mut lash::plugins::PluginRegistrar,
    ) -> Result<(), lash::plugins::PluginError> {
        let seen = Arc::clone(&self.seen);
        reg.session().on_event(
            lash::hook_key!("observe"),
            Arc::new(move |event| {
                let seen = Arc::clone(&seen);
                Box::pin(async move {
                    if let lash::plugins::PluginLifecycleEvent::SessionConfigChanged(ctx) = event {
                        let snapshot = ctx.sessions.snapshot_current().await?;
                        seen.lock_recover().push(Seen {
                            session: ctx.session_id.to_string(),
                            revision: ctx.revision,
                            previous: ctx.previous.max_tool_calls.get(),
                            current: ctx.current.max_tool_calls.get(),
                            served: snapshot.policy.max_tool_calls.get(),
                        });
                    }
                    Ok(())
                })
            }),
        )
    }
}

/// The live session `name` on `world`'s core, whose admin applies config.
async fn open(world: &World, name: &str) -> lash::LashSession {
    world
        .core
        .session(lash::SessionId::try_from(name.to_owned()).unwrap())
        .open()
        .await
        .expect("the session opens")
}

/// A core over `tier` with `observer` installed.
async fn world(tier: Tier, observer: &Observer) -> Option<World> {
    let factory: Arc<dyn lash::plugins::PluginFactory> = Arc::new(observer.clone());
    World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(factory)
    })
    .await
}

/// Apply a transaction setting the tool-call limit to `limit` on `session`,
/// written against `revision`.
async fn set_limit(
    session: &lash::LashSession,
    id: &str,
    revision: u64,
    limit: usize,
) -> ConfigTransactionOutcome {
    session
        .admin()
        .config()
        .apply(
            ConfigWrite::new(id, revision),
            ConfigTransaction::of(SetMaxToolCalls {
                max_tool_calls: lash::MaxToolCalls::new(limit),
            }),
        )
        .await
        .expect("the transaction is accepted")
        .await_outcome(&session.admin().config())
        .await
        .expect("the transaction settles")
}

/// The change `name`'s head still owes its observers.
async fn owed(
    backend: &lash::Backend,
    name: &str,
) -> Option<Box<lash::persistence::UndeliveredConfigChange>> {
    backend
        .stores()
        .session_store_factory()
        .load_session_head_meta(&lash::SessionId::try_from(name.to_owned()).unwrap())
        .await
        .expect("read the session's head")
        .expect("the session has a head")
        .config
        .undelivered_change
}

async fn revision(session: &lash::LashSession) -> u64 {
    session
        .admin()
        .config()
        .revision()
        .await
        .expect("read the config revision")
}

/// A committed config transaction reaches the session's config-change
/// observers once, after its commit: they see the change's revision, the
/// previous and the current policy, and the session they read already
/// serves the new one. The session's head no longer owes it, a later turn
/// does not deliver it again, and a transaction that does not apply
/// delivers nothing (FIG-5333, FIG-5397).
async fn a_committed_config_transaction_reaches_its_observers_once(tier: Tier) {
    let observer = Observer::default();
    let Some(world) = world(tier, &observer).await else {
        return;
    };
    let name = "config-change-observed";
    let durable = world.session(name, served::spec(BEFORE)).await;
    world.send(&durable, "before the change").await;
    let session = open(&world, name).await;
    assert_eq!(observer.seen(), Vec::new(), "no config changed yet");

    let base = revision(&session).await;
    let outcome = set_limit(&session, "raise-the-limit", base, AFTER).await;
    assert!(
        matches!(outcome, ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    // The session actor runs this turn after the command run that applied
    // the transaction: its answer means the command run has ended.
    world.send(&durable, "after the change").await;
    let once = vec![Seen {
        session: name.to_owned(),
        revision: base + 1,
        previous: BEFORE,
        current: AFTER,
        served: AFTER,
    }];
    assert_eq!(
        observer.seen(),
        once,
        "one delivery of the committed change"
    );
    assert_eq!(
        owed(&world.backend, name).await,
        None,
        "the delivery is retired"
    );

    let stale = set_limit(&session, "stale-write", base, BEFORE).await;
    assert!(
        matches!(stale, ConfigTransactionOutcome::Stale { .. }),
        "{stale:?}"
    );
    world.send(&durable, "after the stale write").await;
    assert_eq!(
        observer.seen(),
        once,
        "neither a later turn nor a transaction that did not apply delivers"
    );
    assert_eq!(session.policy_snapshot().max_tool_calls.get(), AFTER);
    world.shutdown().await;
}

/// A config transaction applies to a session whose plugins cannot build:
/// it answers `Applied` and the session records the new policy; its
/// observers, never built, see nothing, and the session's head owes them
/// the change. The session's next plugin build, its next turn's, delivers
/// it once and the turn's commit retires it (FIG-5245, FIG-5333, FIG-5397).
async fn a_config_transaction_applies_when_the_sessions_plugins_cannot_build(tier: Tier) {
    let observer = Observer::default();
    let Some(world) = world(tier, &observer).await else {
        return;
    };
    let name = "config-change-unbuilt";
    let durable = world.session(name, served::spec(BEFORE)).await;
    let session = open(&world, name).await;
    let base = revision(&session).await;
    observer.failing.store(true, Ordering::SeqCst);
    let outcome = set_limit(&session, "raise-unbuilt", base, AFTER).await;
    assert!(
        matches!(outcome, ConfigTransactionOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(session.policy_snapshot().max_tool_calls.get(), AFTER);
    assert_eq!(revision(&session).await, base + 1);
    assert_eq!(observer.seen(), Vec::new());
    let change = Seen {
        session: name.to_owned(),
        revision: base + 1,
        previous: BEFORE,
        current: AFTER,
        served: AFTER,
    };
    assert_eq!(
        owed(&world.backend, name).await.map(|owed| (
            owed.revision,
            owed.previous.max_tool_calls.get(),
            owed.current.max_tool_calls.get()
        )),
        Some((change.revision, BEFORE, AFTER)),
        "the head owes the observers the change"
    );

    observer.failing.store(false, Ordering::SeqCst);
    world.send(&durable, "once the plugins build").await;
    assert_eq!(
        observer.seen(),
        vec![change],
        "the next plugin build delivers the owed change"
    );
    assert_eq!(
        owed(&world.backend, name).await,
        None,
        "the turn's commit retires it"
    );
    world.shutdown().await;
}

/// Cold-session config changes coalesce to the latest committed transition
/// (FIG-5317, ruling 222777). Every transaction applies while plugins cannot
/// build; the first successful build observes only the final change, under
/// its own revision and with the committed policy already served. Delivery
/// retires the obligation, so a later turn does not announce it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn cold_config_changes_coalesce_until_the_next_successful_plugin_build_on_sqlite_memory() {
    let observer = Observer::default();
    let world = world(Tier::SqliteMemory, &observer)
        .await
        .expect("the SQLite memory world opens");
    let name = "config-changes-coalesced";
    let durable = world.session(name, served::spec(BEFORE)).await;
    let session = open(&world, name).await;
    let base = revision(&session).await;
    observer.failing.store(true, Ordering::SeqCst);

    let mut previous = BEFORE;
    for (index, current) in [AFTER, AFTER + 8, AFTER + 16].into_iter().enumerate() {
        let revision = base + index as u64 + 1;
        let outcome = set_limit(
            &session,
            &format!("change-unbuilt-{index}"),
            revision - 1,
            current,
        )
        .await;
        assert!(
            matches!(outcome, ConfigTransactionOutcome::Applied { .. }),
            "{outcome:?}"
        );
        assert_eq!(session.policy_snapshot().max_tool_calls.get(), current);
        assert_eq!(observer.seen(), Vec::new(), "failed builds deliver nothing");
        assert_eq!(
            owed(&world.backend, name).await.map(|owed| (
                owed.revision,
                owed.previous.max_tool_calls.get(),
                owed.current.max_tool_calls.get(),
            )),
            Some((revision, previous, current)),
            "each committed change supersedes the one still owed"
        );
        previous = current;
    }

    observer.failing.store(false, Ordering::SeqCst);
    world.send(&durable, "once the cold plugins build").await;
    let latest = vec![Seen {
        session: name.to_owned(),
        revision: base + 3,
        previous: AFTER + 8,
        current: AFTER + 16,
        served: AFTER + 16,
    }];
    assert_eq!(
        observer.seen(),
        latest,
        "one delivery of the latest committed transition"
    );
    assert_eq!(
        owed(&world.backend, name).await,
        None,
        "the successful turn retires the coalesced change"
    );
    world.send(&durable, "after the coalesced delivery").await;
    assert_eq!(observer.seen(), latest, "a later build does not redeliver");
    world.shutdown().await;
}

tiered_laws!(
    a_committed_config_transaction_reaches_its_observers_once,
    a_config_transaction_applies_when_the_sessions_plugins_cannot_build,
);

/// The crash law's session, and the input its turn runs after the change.
const CUT_SESSION: &str = "config-change-cut";
const CUT_INPUT: &str = "after the cut change";

fn cut_actor() -> ActorKey {
    ActorKey::session(CUT_SESSION).unwrap()
}

/// A session holding a config transaction and, after it, an input: its
/// actor applies the transaction in a command run, then runs the input's
/// turn, which builds the session's plugins. The scenario is fresh for every
/// matrix cell.
struct CutDelivery {
    dialect: dialect::Dialect,
    postgres_url: Option<String>,
    observer: Observer,
    tripwire: Arc<Tripwire>,
    backend: Mutex<Option<lash::Backend>>,
    core: Mutex<Option<lash::LashCore>>,
    keep: Mutex<Vec<Box<dyn std::any::Any + Send>>>,
}

impl CutDelivery {
    fn new(dialect: dialect::Dialect, postgres_url: Option<String>) -> Self {
        Self {
            dialect,
            postgres_url,
            observer: Observer::default(),
            tripwire: Arc::default(),
            backend: Mutex::default(),
            core: Mutex::default(),
            keep: Mutex::default(),
        }
    }

    fn backend(&self) -> lash::Backend {
        self.backend
            .lock_recover()
            .clone()
            .expect("the database is built first")
    }

    /// The deployment's core: it serves no node of its own, the simulated
    /// nodes run its session.
    fn core(&self) -> lash::LashCore {
        let backend = self.backend();
        let factory: Arc<dyn lash::plugins::PluginFactory> = Arc::new(self.observer.clone());
        self.core
            .lock_recover()
            .get_or_insert_with(|| {
                lash::LashCore::standard_builder(backend)
                    .serve_sessions(false)
                    .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                    .data_retention(lash::DataRetention::standard())
                    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
                    .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
                    .execution_budgets(lash::ExecutionBudgets::recommended())
                    .delta_coalescing(lash::DeltaCoalescing::recommended())
                    .serve_test_llm_profile(served::model(Arc::default()), served::metadata())
                    .plugin(factory)
                    .build(lash::persistence::LeaseOwnerIdentity::opaque(
                        lash::persistence::LeaseOwnerId::new("config-change-deployment"),
                        lash::persistence::LeaseIncarnationId::new("config-change-boot"),
                    ))
                    .expect("the core builds")
            })
            .clone()
    }

    /// The laws of one run, cut at `cut`.
    async fn laws(&self, nodes: &SimNodes, cut: Option<&lash_durable_test::Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let trace = nodes.script().trace();
        let turns = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::TURN_COMMIT && write.committed())
            .count();
        if turns != 1 {
            violations.push(format!("the turn committed {turns} times"));
        }
        // Uncut, the command run commits the change, delivers it, and
        // retires it with a commit of its own.
        let commands = trace
            .iter()
            .filter(|write| write.point.label == CommitLabel::SESSION_COMMAND && write.committed())
            .count();
        if cut.is_none() && commands != 2 {
            violations.push(format!(
                "{commands} session.command commits landed, not the change's and its retirement"
            ));
        }
        let id = lash::SessionId::try_from(CUT_SESSION.to_owned()).unwrap();
        match self
            .backend()
            .stores()
            .session_store_factory()
            .load_session_head_meta(&id)
            .await
        {
            Ok(Some(head))
                if head.config.max_tool_calls.get() == AFTER
                    && head.config.config_revision == 1
                    && head.config.undelivered_change.is_none() => {}
            other => violations.push(format!(
                "the change is not the session's, or is still owed: {other:?}"
            )),
        }
        // The change reached its observers: once uncut, at least once
        // however cut, and every delivery is that one change, under its
        // revision.
        let seen = self.observer.seen();
        let change = Seen {
            session: CUT_SESSION.to_owned(),
            revision: 1,
            previous: BEFORE,
            current: AFTER,
            served: AFTER,
        };
        let delivered = match cut {
            None => seen.len() == 1,
            Some(_) => !seen.is_empty(),
        };
        if !delivered || seen.iter().any(|seen| *seen != change) {
            violations.push(format!(
                "the committed change was not delivered as itself: {seen:?}"
            ));
        }
        violations
    }
}

#[async_trait::async_trait]
impl Scenario for CutDelivery {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let (stores, database): (Arc<dyn StoreSet>, Arc<dyn DurableStore>) = dialect::open(
            self.dialect,
            self.postgres_url.as_deref(),
            clock,
            &self.keep,
        )
        .await;
        *self.backend.lock_recover() = Some(served::configured_backend(
            stores,
            sim::settings(),
            Vec::new(),
        ));
        database
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: self.backend().formats().decodes(),
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(SessionActivation::new(
            self.backend(),
            lash::testing::session_turn_services(&self.core()),
            Arc::clone(&self.tripwire) as _,
        ))
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // The host is outside the deployment under test: its submission and
        // its send are uncut.
        let id = lash::SessionId::try_from(CUT_SESSION.to_owned()).unwrap();
        let session = self
            .core()
            .session(id.clone())
            .create(lash::SessionCreation::root(
                lash::plugins::SessionToolAccess::ambient(),
                served::spec(BEFORE),
            ))
            .await
            .map_err(|error| format!("create the session: {error}"))?;
        self.core()
            .session(id)
            .open()
            .await
            .map_err(|error| format!("open the session: {error}"))?
            .admin()
            .config()
            .submit(
                ConfigWrite::new("raise-the-cut-limit", 0),
                ConfigTransaction::of(SetMaxToolCalls {
                    max_tool_calls: lash::MaxToolCalls::new(AFTER),
                }),
            )
            .await
            .map_err(|error| format!("submit the transaction: {error}"))?;
        session
            .send(lash::TurnInput::text(CUT_INPUT))
            .await
            .map_err(|error| format!("send the turn's input: {error}"))?;
        // A starts and claims first, B once A is settled, so the matrix
        // cuts the uncut run's writes by node.
        nodes.start("a");
        nodes.quiesce().await;
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![cut_actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&cut_actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
        )
    }

    async fn check(&self, nodes: &SimNodes, cut: Option<&lash_durable_test::Cut>) -> Vec<String> {
        self.laws(nodes, cut).await
    }
}

/// Cut at every `session.command` write, under every fault: the commit
/// that applies the transaction, so a node lost between it and the
/// delivery, among them. However the command run is cut, the change is the
/// session's and reaches its observers, every delivery the one change
/// (FIG-5397).
async fn a_committed_config_change_reaches_its_observers_however_its_command_is_cut(
    dialect: dialect::Dialect,
    postgres_url: Option<String>,
) {
    let report = Matrix::new()
        .labels(&[CommitLabel::SESSION_COMMAND])
        .faults(&[
            Fault::FailBefore,
            Fault::AckHidden,
            Fault::Zombie,
            Fault::Abort,
            Fault::CommitThenAbort,
        ])
        .horizon(Duration::from_secs(600))
        .run_test(|| CutDelivery::new(dialect, postgres_url.clone()))
        .await;
    eprintln!(
        "config change on {dialect:?}: {} cells over {:?}",
        report.cells.len(),
        report.labels()
    );
    report.assert_held();
    assert!(
        report.labels().contains(&CommitLabel::SESSION_COMMAND),
        "the matrix never cut session.command"
    );
}

#[tokio::test]
async fn a_committed_config_change_reaches_its_observers_however_its_command_is_cut_on_sqlite_memory()
 {
    a_committed_config_change_reaches_its_observers_however_its_command_is_cut(
        dialect::Dialect::SqliteMemory,
        None,
    )
    .await;
}

#[tokio::test]
async fn a_committed_config_change_reaches_its_observers_however_its_command_is_cut_on_sqlite_file()
{
    a_committed_config_change_reaches_its_observers_however_its_command_is_cut(
        dialect::Dialect::SqliteFile,
        None,
    )
    .await;
}

#[tokio::test]
async fn a_committed_config_change_reaches_its_observers_however_its_command_is_cut_on_postgres() {
    let Some(url) = dialect::postgres_url() else {
        eprintln!("skipping: LASH_POSTGRES_DATABASE_URL is not set");
        return;
    };
    a_committed_config_change_reaches_its_observers_however_its_command_is_cut(
        dialect::Dialect::Postgres,
        Some(url),
    )
    .await;
}
