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

#[tokio::test]
async fn lo_root_prompt_retirement_when_configured() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptWrite};
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore};

    let mut counts = Vec::new();
    for depth in [10, 100, 1000] {
        let database_url = crate::testing::required_database_url();
        let database = IsolatedDatabase::create(&database_url).await;
        let storage = crate::testing::connect(database.url())
            .await
            .expect("open the isolated store");
        let set = crate::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        );
        let store = storage.durable_store();
        let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
        let actor = ActorKey::session("sql-work-owner").unwrap();
        let session = lash_protocol_types::SessionId::from("sql-work-owner");
        let budget = lash_protocol_types::allocation::AllocationBudget::unowned();
        let segments: Vec<_> = (0..depth)
            .map(|index| {
                lash_protocol_types::llm::types::RequestSegment::item_json(
                    &serde_json::json!({"body":format!("leaf {index:04}")}),
                )
                .unwrap()
            })
            .collect();
        let items = lash_protocol_types::llm::types::RequestItems::default()
            .append(&segments, &budget)
            .unwrap();
        let template = lash_protocol_types::llm::types::RecordedRequestTemplate::from_item_run(
            lash_protocol_types::llm::types::ProviderRouteIdentity::new(
                "fixture", "fixture", "fixture",
            ),
            true,
            None,
            "{\"messages\":[".into(),
            items,
            "]}".into(),
            &budget,
        )
        .unwrap();
        let (_, request_texts) = lash_core_store::prompt_sections::OwnerPromptState::default()
            .chunk(&template, &budget)
            .unwrap();
        let texts: Vec<_> = request_texts.iter().cloned().collect();
        let root = texts
            .iter()
            .find(|text| Some(&text.hash) == request_texts.roots().first())
            .unwrap();
        let held = texts
            .iter()
            .find(|text| text.hash == root.children[0])
            .unwrap()
            .clone();
        let mut survivors = std::collections::BTreeSet::new();
        let mut pending = vec![held.hash.clone()];
        while let Some(hash) = pending.pop() {
            let text = texts.iter().find(|text| text.hash == hash).unwrap();
            pending.extend(text.children.iter().cloned());
            survivors.insert(hash);
        }
        let call = |run: &'static str| PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: lash_protocol_types::TurnId::from(run),
                ordinal: 1,
            },
        };
        let mut tx = store
            .begin(&actor, lash_durable::Epoch(identity.epoch))
            .await
            .unwrap();
        for (run, request_texts) in [("whole", request_texts), ("held", vec![held].into())] {
            tx.write(DomainWrite::Prompt(PromptWrite::Record {
                call: call(run),
                snapshot: None,
                request: run.to_owned(),
                snapshot_texts: Vec::new(),
                request_texts,
                prefix: None,
            }));
        }
        let (answer, published) = lash_core_ids::operational_metrics::sql::collect(
            "prompt.publish",
            "postgres",
            store.commit(tx, CommitLabel::new("model.start")),
        )
        .await;
        answer.unwrap();
        let mut total: u64 = published
            .shapes
            .values()
            .map(|shape| shape.statements)
            .sum();
        let hashes: Vec<_> = texts.iter().map(|text| text.hash.clone()).collect();
        for run in ["whole", "held"] {
            let mut tx = store
                .begin(&actor, lash_durable::Epoch(identity.epoch))
                .await
                .unwrap();
            tx.write(DomainWrite::Prompt(PromptWrite::Release {
                session: session.clone(),
                run: Some(lash_protocol_types::TurnId::from(run)),
            }));
            let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
                "prompt.release",
                "postgres",
                store.commit(tx, CommitLabel::new("prompt.release")),
            )
            .await;
            answer.unwrap();
            total += receipt
                .shapes
                .values()
                .map(|shape| shape.statements)
                .sum::<u64>();
            eprintln!(
                "LO-ROOT prompt retirement PostgreSQL {depth}/{run}: {}",
                serde_json::to_string(&receipt).unwrap()
            );
            use lash_durable::domain::pages::PageStore as _;
            assert_eq!(
                store.retirement_backlog().await.unwrap().holds,
                0,
                "attributable cleanup is drained"
            );
            let remaining: std::collections::BTreeSet<_> = store
                .prompt_texts(&hashes)
                .await
                .unwrap()
                .into_iter()
                .map(|text| text.hash)
                .collect();
            if run == "whole" {
                assert_eq!(remaining, survivors);
            } else {
                assert!(remaining.is_empty());
            }
        }
        counts.push(total);
    }
    assert!(
        counts.windows(2).all(|pair| pair[0] == pair[1]),
        "publication and drained cleanup SQL is flat: {counts:?}"
    );
}
