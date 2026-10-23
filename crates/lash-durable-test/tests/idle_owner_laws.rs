//! FIG-6890: a node's idle owners neither hold its slots against waiting
//! work nor spin their mail polls.
//!
//! A session with nothing to do stays hot on its node for `idle_evict`.
//! Each law runs a lash core serving its own node over a SQLite store set on
//! the virtual clock, so a poll fires only when the law moves time:
//!
//! - a burst of new sessions sent to a node whose every slot an idle owner
//!   holds is admitted with the clock standing still, so no claim poll, mail
//!   poll or eviction ran it;
//! - a node's idle owners wake for their mail polls a bounded number of
//!   times a minute: each owner's poll backs off with its idle time.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;
use std::time::Duration;

use lash_core::llm::types::LlmRequest;
use lash_durable_test::{SimClock, settle};

/// A deadlock watchdog on the wall clock, no part of any law.
const WATCHDOG: Duration = Duration::from_secs(60);

/// Owners stay hot far past anything a law waits out.
const IDLE_EVICT: Duration = Duration::from_secs(3600);

/// A model that answers every request at once.
fn model() -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("idle-owner-model")
        .requires_streaming(true)
        .complete(|request: LlmRequest| async move { Ok(served::text(&request, "done")) })
        .build()
        .into_handle()
}

/// A core serving its node over a SQLite store set on `clock`, running at
/// most `max_active` sessions.
async fn core(clock: &Arc<SimClock>, max_active: usize) -> lash::LashCore {
    let stores = sim::memory(Arc::clone(clock)).await;
    let defaults = lash_core_execution::DurableSettings::default();
    let settings = lash_core_execution::DurableSettings {
        max_active,
        claim_batch: defaults.claim_batch.min(max_active),
        owner_retention: lash_durable::OwnerRetentionSettings {
            idle: IDLE_EVICT,
            ..lash_durable::OwnerRetentionSettings::default()
        },
        ..defaults
    };
    let backend = served::configured_backend(Arc::new(stores), settings, Vec::new());
    lash::LashCore::standard_builder(backend)
        .commit_budget(lash::CommitBudget {
            nodes: lash::CommitBudgetLimit::bounded(4096),
            ..lash::CommitBudget::default()
        })
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model(), served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("idle-owner-deployment"),
            lash::persistence::LeaseIncarnationId::new("idle-owner-boot"),
        ))
        .expect("the core builds")
}

/// Create session `name` and send it its first input, answered.
async fn first_send(core: &lash::LashCore, name: &str) {
    let session = core
        .session(
            lash_vocabulary::identity::SessionId::try_from(name.to_owned()).expect("a session id"),
        )
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .expect("the session is created");
    session
        .send(lash::TurnInput::text(name))
        .output()
        .await
        .unwrap_or_else(|error| panic!("`{name}`'s first send answers: {error}"));
}

/// Create sessions `prefix-0..count` at once, each with its first send.
async fn burst(core: &lash::LashCore, prefix: &str, count: usize) {
    let mut sends = tokio::task::JoinSet::new();
    for index in 0..count {
        let core = core.clone();
        let name = format!("{prefix}-{index}");
        sends.spawn(async move { first_send(&core, &name).await });
    }
    tokio::time::timeout(WATCHDOG, sends.join_all())
        .await
        .unwrap_or_else(|_| panic!("deadlock watchdog: a first send of `{prefix}` never answered"));
}

/// A node whose every slot an idle owner holds admits a burst of new
/// sessions, twice its capacity, each with a first send, on the wake their
/// sends commit: idle owners give their slots up to readied work. The clock
/// never moves, so no claim poll, mail poll or idle eviction admitted any
/// of them.
#[tokio::test]
async fn a_burst_of_new_sessions_on_a_node_its_idle_owners_fill_is_admitted_without_a_poll() {
    const CAPACITY: usize = 4;
    let clock = SimClock::new();
    let core = core(&clock, CAPACITY).await;
    burst(&core, "held", CAPACITY).await;
    settle().await;
    burst(&core, "new", 2 * CAPACITY).await;
    assert_eq!(
        clock.logical_ms(),
        0,
        "the burst was admitted with time standing still"
    );
    core.shutdown().await.expect("shutdown");
}

/// Idle owners wake for their mail polls at most this many times each in
/// [`MINUTE`]: the poll backs off with the owner's idle time, from one
/// claim poll up to sixteen, so a minute holds the four doublings, the
/// first poll and one poll at the ceiling per sixteen claim polls, and one
/// for the minute's edge.
fn idle_wakes_bound(claim_poll: Duration) -> u64 {
    let ceiling = claim_poll.as_millis() as u64 * 16;
    4 + 1 + MINUTE.as_millis() as u64 / ceiling + 1
}

/// The virtual interval the idle law counts wakes over.
const MINUTE: Duration = Duration::from_secs(60);

/// A node whose owners all sit idle wakes a bounded number of timers a
/// minute for each of them: the minute's wakes with eight idle owners, less
/// the node's own wakes in a minute with none, are at most
/// [`idle_wakes_bound`] for each owner.
#[tokio::test]
async fn an_idle_nodes_owners_poll_their_mail_a_bounded_number_of_times_a_minute() {
    const OWNERS: usize = 8;
    let clock = SimClock::new();
    let core = core(&clock, 256).await;
    let minute = MINUTE.as_millis() as u64;
    settle().await;
    let before = clock.woken();
    clock.advance_by(minute).await;
    let node_alone = clock.woken() - before;
    burst(&core, "idle", OWNERS).await;
    settle().await;
    let before = clock.woken();
    clock.advance_by(minute).await;
    let with_owners = clock.woken() - before;
    let per_owner = with_owners.saturating_sub(node_alone) as u64 / OWNERS as u64;
    let bound = idle_wakes_bound(
        lash_core_execution::DurableSettings::default()
            .lease
            .claim_poll,
    );
    assert!(
        per_owner <= bound,
        "each idle owner woke {per_owner} times in a minute, past {bound} \
         (node alone {node_alone}, with owners {with_owners})"
    );
    core.shutdown().await.expect("shutdown");
}
