//! The durability engine's fencing laws (`lash_durable::laws`) over
//! PostgreSQL, each on its own isolated database.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use lash_core_ids::test_clock::TestClock;
use lash_durable::laws;

use crate::testing::IsolatedDatabase;

macro_rules! law {
    ($name:ident, |$store:ident, $advance:ident| $law:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let Some(database_url) = crate::postgres_test_support::database_url() else {
                eprintln!("skipping {}: database URL is not set", stringify!($name));
                return;
            };
            let database = IsolatedDatabase::create(&database_url).await;
            let storage = crate::testing::connect(database.url())
                .await
                .expect("open the isolated store");
            let clock = Arc::new(TestClock::new(1_000_000));
            let $store = &storage
                .durable_store()
                .with_clock_for_testing(clock.clone());
            let $advance = &|by: Duration| {
                clock.advance(u64::try_from(by.as_millis()).expect("a law advances by millis"));
            };
            $law.await.unwrap_or_else(|broken| panic!("{broken}"));
        }
    };
}

// The hermetic PostgreSQL unit target supplies its own server. These mounts
// also refuse an absent service when invoked directly.
macro_rules! claim_law {
    ($name:ident, |$store:ident, $advance:ident| $law:expr) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() {
            let database_url = crate::testing::required_database_url();
            let database = IsolatedDatabase::create(&database_url).await;
            let storage = crate::testing::connect(database.url())
                .await
                .expect("open the isolated store");
            let clock = Arc::new(TestClock::new(1_000_000));
            let $store = &storage
                .durable_store()
                .with_clock_for_testing(clock.clone());
            let $advance = &|by: Duration| {
                clock.advance(u64::try_from(by.as_millis()).expect("a law advances by millis"));
            };
            $law.await.unwrap_or_else(|broken| panic!("{broken}"));
        }
    };
}

claim_law!(
    a_replaced_boot_adopts_none_of_its_successors_actors,
    |store, _advance| { laws::a_replaced_boot_adopts_none_of_its_successors_actors(store) }
);
claim_law!(
    a_boot_redrives_only_the_session_parks_it_names,
    |store, _advance| { laws::a_boot_redrives_only_the_session_parks_it_names(store) }
);
claim_law!(
    progress_before_an_empty_ready_release_resets_failed_activations,
    |store, _advance| {
        laws::progress_before_an_empty_ready_release_resets_failed_activations(store)
    }
);
claim_law!(
    claims_take_the_oldest_eligible_actors_first,
    |store, advance| { laws::claims_take_the_oldest_eligible_actors_first(store, advance) }
);
claim_law!(
    an_abandoned_activation_hands_back_only_its_original_grant,
    |store, _advance| {
        laws::an_abandoned_activation_hands_back_only_its_original_grant(Arc::new(store.clone()))
    }
);

/// The law's four claimers must race in PostgreSQL even when each statement
/// takes time: queueing them behind one connection can exhaust the 500 ms
/// acquire budget before a later claimer reaches the database (FIG-6314).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_writer_under_a_claim_race() {
    let database_url = crate::testing::required_database_url();
    let database = IsolatedDatabase::create(&database_url).await;
    let mut config = crate::testing::fixture_config();
    // This law races four nodes, matching the fixture's served-node count.
    // Each needs a scheduler connection without waiting behind its peers.
    config.roles.scheduler.max_connections = config.roles.served_nodes;
    let storage = crate::testing::connect_with(database.url(), &config)
        .await
        .expect("open the isolated store");
    crate::testing::delay_node_statements(storage.pool(), Duration::from_millis(300))
        .await
        .expect("model host load during the claim race");
    let store = storage
        .durable_store()
        .with_clock_for_testing(Arc::new(TestClock::new(1_000_000)));
    laws::one_writer_under_a_claim_race(&store)
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
}
law!(a_zombie_owner_past_reap_cannot_commit, |store, advance| {
    laws::a_zombie_owner_past_reap_cannot_commit(store, advance)
});
law!(
    a_lapsed_heartbeat_is_reaped_with_an_epoch_bump,
    |store, advance| { laws::a_lapsed_heartbeat_is_reaped_with_an_epoch_bump(store, advance) }
);
law!(mail_from_non_owners_wakes_the_actor, |store, advance| {
    laws::mail_from_non_owners_wakes_the_actor(store, advance)
});
law!(no_write_through_a_stale_epoch, |store, advance| {
    laws::no_write_through_a_stale_epoch(store, advance)
});
law!(
    an_owner_commit_reports_a_turn_cancel_accepted_after_its_open,
    |store, _advance| {
        laws::an_owner_commit_reports_a_turn_cancel_accepted_after_its_open(store)
    }
);
law!(
    a_turn_cancel_is_a_first_winner_row_with_a_wake,
    |store, _advance| { laws::a_turn_cancel_is_a_first_winner_row_with_a_wake(store) }
);
law!(
    a_turn_counts_the_model_calls_it_admitted,
    |store, _advance| { laws::a_turn_counts_the_model_calls_it_admitted(store) }
);
law!(
    a_receipt_stands_beside_its_model_phase_until_the_next_phase,
    |store, _advance| { laws::a_receipt_stands_beside_its_model_phase_until_the_next_phase(store) }
);
law!(
    a_runs_namespace_write_modes_keep_only_current_values_and_end_with_it,
    |store, _advance| {
        laws::a_runs_namespace_write_modes_keep_only_current_values_and_end_with_it(store)
    }
);
law!(
    a_session_close_moves_one_step_at_a_time,
    |store, _advance| { laws::a_session_close_moves_one_step_at_a_time(store) }
);
law!(
    prompt_snapshot_roots_survive_phase_pruning_until_released,
    |store, _advance| { laws::prompt_snapshot_roots_survive_phase_pruning_until_released(store) }
);
law!(
    settling_a_turns_calls_releases_their_requests_and_keeps_their_snapshots,
    |store, _advance| {
        laws::settling_a_turns_calls_releases_their_requests_and_keeps_their_snapshots(store)
    }
);
law!(
    a_node_claims_only_actors_whose_formats_it_decodes,
    |store, _advance| { laws::a_node_claims_only_actors_whose_formats_it_decodes(store) }
);
law!(
    a_hard_record_lands_with_its_owner_commit_or_not_at_all,
    |store, _advance| { laws::a_hard_record_lands_with_its_owner_commit_or_not_at_all(store) }
);
law!(
    a_node_claims_only_actors_whose_hard_requirements_it_meets,
    |store, _advance| { laws::a_node_claims_only_actors_whose_hard_requirements_it_meets(store) }
);
law!(
    a_store_keeps_each_sets_first_surface_and_indexes_its_definitions,
    |store, _advance| {
        laws::a_store_keeps_each_sets_first_surface_and_indexes_its_definitions(store)
    }
);
law!(
    a_store_keeps_artifacts_and_sets_by_digest,
    |store, _advance| { laws::a_store_keeps_artifacts_and_sets_by_digest(store) }
);
law!(
    a_newer_format_is_not_written_while_an_older_node_is_live,
    |store, _advance| { laws::a_newer_format_is_not_written_while_an_older_node_is_live(store) }
);
law!(
    a_draining_node_claims_nothing_and_releases_ready,
    |store, _advance| { laws::a_draining_node_claims_nothing_and_releases_ready(store) }
);
law!(
    actors_in_a_format_set_and_a_turns_cells_are_listed,
    |store, _advance| { laws::actors_in_a_format_set_and_a_turns_cells_are_listed(store) }
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text() {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!(
            "skipping deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text: \
             database URL is not set"
        );
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    laws::deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text(
        &storage.durable_store(),
        &storage.session_store_factory(),
    )
    .await
    .unwrap_or_else(|broken| panic!("{broken}"));
}

claim_law!(
    a_retired_owner_claims_only_its_registry_callbacks,
    |store, _advance| { laws::a_retired_owner_claims_only_its_registry_callbacks(store) }
);

law!(
    a_turn_counts_its_tool_admissions_atomically,
    |store, _advance| { laws::a_turn_counts_its_tool_admissions_atomically(store) }
);

/// LO-FENCE exercises the function itself as well as its same-draft fallback.
async fn lo_fence_path(explicit: bool) {
    let url = crate::testing::required_database_url();
    let database = IsolatedDatabase::create(&url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("isolated store");
    let clock = Arc::new(TestClock::new(1_000_000));
    let store = storage
        .durable_store()
        .with_clock_for_testing(clock.clone());
    let advance = |by: Duration| clock.advance(u64::try_from(by.as_millis()).expect("millis"));
    let (outcome, refused) = super::executor::observe_fence_refusals(laws::lo_fence_with_path(
        &store, &advance, explicit,
    ))
    .await;
    outcome.unwrap_or_else(|broken| panic!("{broken}"));
    if explicit {
        // Takeover reaches the function; park and root-lease work select
        // the explicit path.
        assert_eq!(refused, 1, "only takeover reaches the function fence");
    } else {
        assert_eq!(refused, 2, "stale graph and takeover each raised LX003");
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM lash_blobs WHERE hash = $1")
        .bind(lo_fence_staged_hash())
        .fetch_one(storage.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0, "stale staged nodes were rolled back");
}

fn lo_fence_staged_hash() -> Vec<u8> {
    use lash_core_store::checkpoint_graph::{
        EncodeBudget, ExplicitLimit, GraphStaging, NodeKind, TokenWriter, TypedBody,
    };
    let mut out = TokenWriter::new();
    out.text("lo-fence-stale");
    let mut staging = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    staging
        .encode(NodeKind::FROZEN_VALUE, TypedBody::logical(out.as_bytes()))
        .unwrap()
        .0
        .to_vec()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn lo_fence_explicit_when_configured() {
    lo_fence_path(true).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn lo_fence_function_when_configured() {
    lo_fence_path(false).await;
}
