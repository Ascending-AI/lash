//! FIG-7055: a send followed from a process that serves no node, while a node
//! of another process runs its session over the same database. The follower
//! learns of the run's commits from the session's change hints, which every
//! owner commit of the session names to every listener, and reads the store
//! once per hint; a timed read at the follow poll's ceiling stays for a hint
//! that was lost.

use super::*;

use lash_core::durable_port::{DurableError, NodeLease, NodeWakeFeed, NodeWakes};
use lash_execution_contracts::backend::StoreSet;

/// A worker core that serves the sessions, and a host core over its own
/// store set on the same database that only sends and follows, as two
/// processes would.
struct Topology {
    worker: LashCore,
    host: LashCore,
}

fn owner(name: &str) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(
        lash_core::LeaseOwnerId::new(name),
        lash_core::LeaseIncarnationId::new("boot-1"),
    )
}

async fn topology(
    worker_stores: Arc<dyn StoreSet>,
    host_stores: Arc<dyn StoreSet>,
    host_pacing: lash::ObserverPacing,
) -> Result<Topology> {
    let worker = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(worker_stores),
    ))
    .serve_test_llm_profile(
        scripted_provider(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))),
        mock_llm_profile_spec(),
    )
    .build(owner("worker"))?;
    let host = explicit_ephemeral_facets(LashCore::standard_builder(
        lash_conformance::backend_over(host_stores),
    ))
    .serve_test_llm_profile(
        scripted_provider(Arc::new(Notify::new()), Arc::new(AtomicUsize::new(0))),
        mock_llm_profile_spec(),
    )
    .serve_sessions(false)
    .observer_pacing(host_pacing)
    .build(owner("host"))?;
    Ok(Topology { worker, host })
}

/// Observer pacing whose follow and terminal reads come at `follow` and
/// `terminal`.
fn pacing(follow: lash::PollPacing, terminal: lash::PollPacing) -> lash::ObserverPacing {
    lash::ObserverPacing {
        follow,
        terminal,
        ..lash::ObserverPacing::standard()
    }
}

/// Every timed read an hour away.
fn untimed() -> Result<lash::ObserverPacing> {
    let hour = std::time::Duration::from_secs(3600);
    let never = lash::PollPacing::new(hour, hour).expect("valid pacing");
    Ok(pacing(never, never))
}

/// Send `text` from the host, and follow it to its answer.
async fn sent_from_the_host(topology: &Topology, session: &str, text: &str) -> Result<()> {
    let session = created_session(&topology.host, SessionId::fixture(session))
        .await
        .durable()
        .await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session.send(TurnInput::text(text)).output(),
    )
    .await
    .expect("the host's follower answers")?;
    assert_eq!(
        output.assistant_message(),
        Some(format!("echo: {text}").as_str())
    );
    topology.host.shutdown().await?;
    topology.worker.shutdown().await?;
    Ok(())
}

async fn sqlite_file(directory: &tempfile::TempDir) -> Arc<dyn StoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            directory.path().join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("open the SQLite store set"),
    )
}

/// With every timed read an hour away, a follower in a process that serves
/// no node still answers: the worker's owner commits of the session reach
/// it through the store set's observer. Before, it learned its run's binding
/// at the poll floor and its terminal on the terminal schedule, and answered
/// only when one of them fired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_less_follower_answers_from_change_hints() -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let topology = topology(
        sqlite_file(&directory).await,
        sqlite_file(&directory).await,
        untimed()?,
    )
    .await?;
    sent_from_the_host(&topology, "cross-node-hints", "over there").await
}

/// Node wakes whose observer hears nothing: every hint to it is lost.
struct DeafObserver(Arc<dyn NodeWakes>);

struct Deaf;

#[async_trait]
impl NodeWakeFeed for Deaf {
    async fn next(&mut self) -> lash_core::durable_port::NodeWakeEvent {
        std::future::pending().await
    }

    fn session(&self) -> u64 {
        0
    }
}

#[async_trait]
impl NodeWakes for DeafObserver {
    async fn publish(
        &self,
        batch: &lash_core::durable_port::WakeBatch,
    ) -> std::result::Result<(), DurableError> {
        self.0.publish(batch).await
    }

    async fn committed(
        &self,
        batch: &lash_core::durable_port::WakeBatch,
    ) -> std::result::Result<(), DurableError> {
        self.0.committed(batch).await
    }

    async fn listen(
        &self,
        lease: &NodeLease,
    ) -> std::result::Result<Box<dyn NodeWakeFeed>, DurableError> {
        self.0.listen(lease).await
    }

    async fn observe(&self) -> std::result::Result<Box<dyn NodeWakeFeed>, DurableError> {
        Ok(Box::new(Deaf))
    }

    async fn liveness(
        &self,
    ) -> std::result::Result<Vec<lash_core::durable_port::BootLiveness>, DurableError> {
        self.0.liveness().await
    }

    async fn reap_released(
        &self,
        reaper: &NodeLease,
        boot: &lash_core::durable_port::Owner,
    ) -> std::result::Result<Vec<lash_core::durable_port::Reaped>, DurableError> {
        self.0.reap_released(reaper, boot).await
    }
}

/// A follower whose observer hears none of its session's hints still
/// answers, from its timed read at the follow poll's ceiling: the hints are
/// a latency aid, and the read they replace stays for their loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_follower_whose_hints_are_lost_answers_from_its_timed_read() -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let host =
        lash_core::testing::runtime_helpers::LayeredStores::over(sqlite_file(&directory).await)
            .map_node_wakes(|inner| Arc::new(DeafObserver(inner)))
            .into_store_set();
    let ceiling = lash::PollPacing::new(
        std::time::Duration::from_millis(25),
        std::time::Duration::from_millis(200),
    )
    .expect("valid pacing");
    let topology = topology(
        sqlite_file(&directory).await,
        host,
        pacing(ceiling, untimed()?.terminal),
    )
    .await?;
    sent_from_the_host(&topology, "cross-node-lost-hints", "found anyway").await
}
