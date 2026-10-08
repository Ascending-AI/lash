//! The durability engine's fencing laws (`lash_durable::laws`) over
//! PostgreSQL, each on its own isolated database.

// FIG-2971: this file is test code; ambient env access is sanctioned here
// (the workspace clippy ban targets production library code).
#![allow(clippy::disallowed_methods)]

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::testing::TestClock;
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

law!(one_writer_under_a_claim_race, |store, _advance| {
    laws::one_writer_under_a_claim_race(store)
});
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
    a_turn_cancel_is_a_first_winner_row_with_a_wake,
    |store, _advance| { laws::a_turn_cancel_is_a_first_winner_row_with_a_wake(store) }
);
law!(
    a_turn_counts_the_model_calls_it_admitted,
    |store, _advance| { laws::a_turn_counts_the_model_calls_it_admitted(store) }
);
law!(
    a_runs_namespace_rows_hold_its_bodies_and_end_with_it,
    |store, _advance| { laws::a_runs_namespace_rows_hold_its_bodies_and_end_with_it(store) }
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
    a_node_claims_only_actors_whose_formats_it_decodes,
    |store, _advance| { laws::a_node_claims_only_actors_whose_formats_it_decodes(store) }
);
law!(
    a_newer_format_is_not_written_while_an_older_node_is_live,
    |store, _advance| { laws::a_newer_format_is_not_written_while_an_older_node_is_live(store) }
);
law!(
    a_draining_node_claims_nothing_and_releases_ready,
    |store, _advance| { laws::a_draining_node_claims_nothing_and_releases_ready(store) }
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
