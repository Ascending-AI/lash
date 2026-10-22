//! FIG-5932: a send to a session its node owns is admitted by the wake its
//! acceptance commits, handed to the owner in process after the commit, not
//! by the owner's claim poll. The poll stays the backstop: a wake that never
//! reaches the owner is still found by it.

use super::*;

/// A claim poll no law here waits out: under it, only a wake admits a send
/// within [`BOUND`].
const NO_POLL: std::time::Duration = std::time::Duration::from_secs(3600);

/// How long a send may take to answer here: far below [`NO_POLL`], and far
/// above a send its wake admits.
const BOUND: std::time::Duration = std::time::Duration::from_secs(30);

/// A serving core over `stores` whose claim poll is `claim_poll`.
fn serving(
    stores: Arc<dyn lash_execution_contracts::backend::StoreSet>,
    claim_poll: std::time::Duration,
) -> LashCore {
    let mut settings = lash::durable::DurableSettings {
        max_ingress_bytes: Some(const { std::num::NonZeroU64::new(64 * 1024).unwrap() }),
        owner_retention: Some(lash::durable::OwnerRetentionSettings {
            idle: std::time::Duration::from_secs(60),
            max_resident_owners: const { std::num::NonZeroUsize::new(256).unwrap() },
        }),
        ..lash::durable::DurableSettings::default()
    };
    settings.lease.claim_poll = claim_poll;
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .config(settings)
        .build()
        .expect("durable backend");
    explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .serve_sessions(true)
        .build(lash_facade_tests::runtime_lease_owner())
        .expect("serving core")
}

/// Send `texts` to a new session of `core` one after another, each answered
/// within [`BOUND`].
async fn send_in_turn(core: &LashCore, session: &str, texts: &[&str]) {
    let session = core
        .session(lash_vocabulary::identity::SessionId::parse(session).expect("session id"))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await
        .expect("create the session");
    for text in texts {
        tokio::time::timeout(BOUND, session.send(lash::TurnInput::text(*text)).output())
            .await
            .unwrap_or_else(|_| panic!("`{text}` was not admitted by its wake"))
            .expect("the turn answers");
    }
}

/// The first send readies the unowned session and rings its node's claim;
/// each later one reaches the idle owner through its acceptance's wake. With
/// the claim poll an hour long, a send the wake missed never answers in time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_send_is_admitted_by_its_acceptance_wake_not_the_claim_poll() {
    let core = serving(sqlite_memory_store_set().await, NO_POLL);
    send_in_turn(&core, "warm-admission", &["cold", "warm", "warm again"]).await;
    core.shutdown().await.expect("shutdown");
}

/// A node that hears none of its store set's wakes, as when a wake is lost,
/// still admits every send at its owner's claim poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_whose_wake_is_lost_is_admitted_at_the_claim_poll() {
    let stores =
        lash_core::testing::runtime_helpers::LayeredStores::over(sqlite_memory_store_set().await)
            .lose_wakes()
            .into_store_set();
    let core = serving(stores, std::time::Duration::from_millis(200));
    send_in_turn(&core, "lost-wake", &["cold", "warm"]).await;
    core.shutdown().await.expect("shutdown");
}
