//! LO-EVICT observes production activations and their actual projection lifetimes.
use super::*;
use lash_core::runtime::durable::session::SessionActivation;
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{ActorKey, DurableError};
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_protocol_types::sync::MutexExt as _;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

type Contexts = Arc<Mutex<BTreeMap<String, lash_core::ActorContext>>>;
struct ObservedActivation {
    inner: SessionActivation,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
struct Live(Arc<AtomicUsize>);
impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[async_trait::async_trait]
impl Activation for ObservedActivation {
    async fn capabilities(&self) -> Result<lash_durable::NodeCapabilities, DurableError> {
        self.inner.capabilities().await
    }
    async fn activate(&self, owned: Owned) -> Exit {
        let _live = Live(self.live.clone());
        let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(live, Ordering::SeqCst);
        self.inner.activate(owned).await
    }
}
async fn source(contexts: &Contexts, id: &str) -> std::sync::Weak<lash_core::RuntimeSessionState> {
    let cx = contexts.lock_recover().get(id).unwrap().clone();
    let projection = cx.session_projection().await.unwrap().unwrap();
    Arc::downgrade(&projection.head)
}

async fn send(nodes: &SimNodes, session: &lash::DurableSession, text: &str) {
    let input = session.send(lash::TurnInput::text(text)).await.unwrap();
    nodes.quiesce().await;
    let output = tokio::time::timeout(served::WATCHDOG, input.output())
        .await
        .expect("event-driven completion with the clock stationary")
        .unwrap();
    served::assert_answered(text, &output);
    assert_eq!(output.assistant_message(), Some("done"));
    nodes.quiesce().await;
}
async fn lo_evict(tier: Tier) {
    let (dialect, url) = dialect_of(tier).expect("required backend service");
    let clock = SimClock::new();
    let keep = Mutex::new(Vec::new());
    let (stores, database) = dialect::open(dialect, url.as_deref(), clock.clone(), &keep).await;
    let mut settings = sim::settings();
    settings.owner_retention = Some(lash_durable::OwnerRetentionSettings {
        idle: Duration::from_secs(60),
        max_resident_owners: std::num::NonZeroUsize::new(2).unwrap(),
    });
    let backend = served::configured_backend(stores, settings, Vec::new());
    let gates = Arc::new([Semaphore::new(0), Semaphore::new(0)]);
    let entered = Arc::new(Semaphore::new(0));
    let provider_gates = gates.clone();
    let provider_entered = entered.clone();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("owner-retention")
        .requires_streaming(true)
        .complete(move |request: lash_core::llm::types::LlmRequest| {
            let gates = provider_gates.clone();
            let entered = provider_entered.clone();
            async move {
                let text = serde_json::to_string(&request.messages).unwrap();
                if text.contains("b-cold") {
                    assert!(
                        text.contains("b-first"),
                        "cold claim preserves committed history"
                    );
                }
                for (index, marker) in ["hold-one", "hold-two"].iter().enumerate() {
                    if text.contains(marker) {
                        entered.add_permits(1);
                        gates[index].acquire().await.unwrap().forget();
                    }
                }
                Ok(served::text(&request, "done"))
            }
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .serve_test_llm_profile(provider, served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lo-evict"),
            lash::persistence::LeaseIncarnationId::new("boot"),
        ))
        .unwrap();
    let mut sessions = Vec::new();
    for id in [
        "evict-a", "evict-b", "evict-c", "busy-one", "busy-two", "deferred",
    ] {
        sessions.push(
            core.session(lash::SessionId::from(id))
                .create(lash::SessionCreation::root(served::spec(8)))
                .await
                .unwrap(),
        );
    }
    let contexts: Contexts = Arc::default();
    let seen = contexts.clone();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let activation = SessionActivation::new(
        backend.clone(),
        lash::testing::session_turn_services(&core),
        Arc::new(Tripwire::default()),
    )
    .observe_context_for_testing(move |cx| {
        seen.lock_recover()
            .insert(cx.actor().id().to_owned(), cx.clone());
    });
    let nodes = SimNodes::new(
        database,
        clock.clone(),
        Script::new(),
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 8,
        },
        Arc::new(ObservedActivation {
            inner: activation,
            live: live.clone(),
            peak: peak.clone(),
        }),
    )
    .with_retention(backend.owner_retention());
    nodes.start("owner");
    let _wakes = backend
        .stores()
        .wake_sink()
        .attach(&nodes.hints("owner").unwrap());
    nodes.quiesce().await;
    send(&nodes, &sessions[0], "a-first").await;
    clock.advance_by(1).await;
    send(&nodes, &sessions[1], "b-first").await;
    let b = source(&contexts, "evict-b").await;
    clock.advance_by(1).await;
    send(&nodes, &sessions[0], "a-recent").await;
    let a = source(&contexts, "evict-a").await;
    clock.advance_by(1).await;
    send(&nodes, &sessions[2], "c-first").await;
    let c = source(&contexts, "evict-c").await;
    assert!(
        b.upgrade().is_none(),
        "exact LRU idle owner's head must drop"
    );
    assert!(a.upgrade().is_some(), "recently used owner stays resident");
    assert!(c.upgrade().is_some());
    assert_eq!(live.load(Ordering::SeqCst), 2);
    assert_eq!(
        peak.load(Ordering::SeqCst),
        2,
        "replacement waits for release"
    );
    send(&nodes, &sessions[1], "b-cold").await;
    let b = source(&contexts, "evict-b").await;
    let idle_by = clock.logical_ms() + 60_001;
    while clock.logical_ms() < idle_by {
        nodes
            .step()
            .await
            .expect("the serving node has a next timer");
    }
    nodes.quiesce().await;
    assert!(
        a.upgrade().is_none() && b.upgrade().is_none() && c.upgrade().is_none(),
        "idle eviction releases actual acknowledged heads"
    );
    assert_eq!(live.load(Ordering::SeqCst), 0);
    assert!(
        nodes.stopped("owner").await.is_none(),
        "idle eviction must keep the runner serving, rather than losing its lease"
    );
    let one = sessions[3]
        .send(lash::TurnInput::text("hold-one"))
        .await
        .unwrap();
    nodes.quiesce().await;
    let two = sessions[4]
        .send(lash::TurnInput::text("hold-two"))
        .await
        .unwrap();
    nodes.quiesce().await;
    tokio::time::timeout(served::WATCHDOG, entered.acquire_many(2))
        .await
        .expect("both admitted owners enter their provider calls")
        .unwrap()
        .forget();
    let mut before = Vec::new();
    for id in ["busy-one", "busy-two"] {
        before.push(
            nodes
                .database()
                .actor(&ActorKey::session(id).unwrap())
                .await
                .unwrap(),
        );
    }
    let at = clock.logical_ms();
    let deferred = sessions[5]
        .send(lash::TurnInput::text("after-slot"))
        .await
        .unwrap();
    nodes.quiesce().await;
    assert!(
        !contexts.lock_recover().contains_key("deferred"),
        "activation defers"
    );
    for (index, id) in ["busy-one", "busy-two"].iter().enumerate() {
        assert_eq!(
            nodes
                .database()
                .actor(&ActorKey::session(id).unwrap())
                .await
                .unwrap(),
            before[index],
            "deferral changes no resident state"
        );
    }
    assert_eq!(live.load(Ordering::SeqCst), 2);
    gates[0].add_permits(1);
    nodes.quiesce().await;
    served::assert_answered("first busy", &one.output().await.unwrap());
    served::assert_answered(
        "deferred",
        &tokio::time::timeout(served::WATCHDOG, deferred.output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(
        clock.logical_ms(),
        at,
        "drop/yield admits work without a poll"
    );
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    gates[1].add_permits(1);
    nodes.quiesce().await;
    served::assert_answered("second busy", &two.output().await.unwrap());
    nodes.kill("owner");
    nodes.quiesce().await;
    core.shutdown().await.unwrap();
}
#[tokio::test]
async fn lo_evict_on_sqlite_memory() {
    lo_evict(Tier::SqliteMemory).await;
}
#[tokio::test]
#[ignore = "requires a private PostgreSQL service"]
async fn lo_evict_on_postgres_when_configured() {
    let _ = lash_postgres_store::testing::required_database_url();
    lo_evict(Tier::Postgres).await;
}

/// LO-INGRESS construction: hosts state a nonzero limit, and one admitted
/// item can never alone exceed the independently configured commit budget.
#[tokio::test]
async fn ingress_policy_is_required_and_fits_one_commit() {
    use lash::durable::{DurableBuildError, DurableConfigError};
    let stores = Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let mut settings = sim::settings();
    settings.max_ingress_bytes = None;
    assert!(matches!(
        lash::durable::DurableBackendBuilder::new(stores.clone())
            .config(settings)
            .build(),
        Err(DurableBuildError::InvalidConfig(
            DurableConfigError::MissingIngressLimit
        ))
    ));
    settings.max_ingress_bytes = std::num::NonZeroU64::new(1024);
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(settings)
        .build()
        .unwrap();
    let host = |bytes| {
        lash_core::facade_support::RuntimeHostConfig::new(
            backend.clone(),
            lash::CommitBudget::bounded(bytes, 512),
            lash::QueuedWorkBatchingConfig::new(1),
            lash::ExecutionBudgets::recommended(),
            lash::DeltaCoalescing::recommended(),
            lash_core::facade_support::DataRetentionConfig::standard(),
        )
    };
    assert!(matches!(
        host(1023),
        Err(DurableConfigError::IngressAboveCommitBudget {
            ingress: 1024,
            commit: 1023
        })
    ));
    assert!(
        host(1024).is_ok(),
        "equality admits one item, without changing aggregate admission"
    );
}
