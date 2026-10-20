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

/// PREFIX-DELTA and REQUEST-LIFETIME (FIG-5956): sequential calls keep one
/// history index across settlements, submit only new fragments and bounded
/// paths, and retire their request envelopes. Prefix invalidation retires
/// the shared history too. Changed earlier bytes remain exact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sequential_calls_submit_only_new_fragments_across_settlement() {
    use lash_core_execution::plugin::prompt::{admission_record, load_admitted_call};
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptWrite};
    use lash_durable::{
        ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore, FormatSet, MailTx, NodeId,
        NodeSpec,
    };
    use lash_protocol_types::{
        llm::types::LlmRequestScope, llm::types::ProviderRouteIdentity,
        llm::types::RecordedRequestTemplate, llm::types::ResponseContext,
        llm::types::ResponseContract,
    };

    let database_url = crate::postgres_test_support::database_url()
        .expect("the fragment-index law requires its hermetic PostgreSQL target");
    let database = crate::testing::IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url()).await.unwrap();
    let store = storage
        .durable_store()
        .with_clock_for_testing(Arc::new(TestClock::new(1_000_000)));
    let session = lash_protocol_types::SessionId::from("fragment-session");
    let actor = ActorKey::session(session.as_str()).unwrap();
    let formats = FormatSet::new("fragment-index-formats");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats.clone());
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .unwrap();
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("fragment-owner"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let epoch = store.claim(&node, 1).await.unwrap()[0].epoch;
    let route = ProviderRouteIdentity {
        provider: "fixture".into(),
        endpoint: "https://fixture.test".into(),
        model: "model".into(),
    };
    let response = ResponseContext::recorded(
        LlmRequestScope::new("fragment-session", "frame", "fragment-call"),
        Arc::new(ResponseContract {
            model: lash_core_execution::testing::test_llm_profile_config(
                "model",
                lash_core_execution::testing::test_llm_profile_metadata("model"),
            ),
            output_spec: None,
            tools: Vec::new(),
        }),
    );
    let mut messages = Vec::new();
    let mut all = std::collections::BTreeSet::new();
    let mut maximum_nodes = 0;
    let mut maximum_edges = 0;
    let mut maximum_settlement_bytes = 0;
    for ordinal in 1..=32u32 {
        if ordinal == 17 {
            messages[0] =
                "{\"role\":\"user\",\"content\":\"changed prefix: 🦀\\\"\\\\\"}".to_owned();
        }
        messages.push(format!(
            "{{\"role\":\"user\",\"content\":\"{ordinal}:{}\"}}",
            "x".repeat(2048)
        ));
        let wire = format!("{{\"messages\":[{}],\"stream\":true}}", messages.join(","));
        let template =
            RecordedRequestTemplate::literal(route.clone(), true, None, wire.as_str()).unwrap();
        let call = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Turn {
                run: lash_protocol_types::TurnId::parse(format!("fragment-turn-{ordinal}"))
                    .unwrap(),
                ordinal: 1,
            },
        };
        let write = admission_record(call.clone(), None, &template, &response, None).unwrap();
        let (prefix, request_root) = match &write {
            DomainWrite::Prompt(PromptWrite::Record {
                prefix, request, ..
            }) => {
                let record: lash_core_store::prompt_sections::AdmittedRequest =
                    serde_json::from_str(request).unwrap();
                (
                    prefix.clone().expect("a history prefix"),
                    record.body.root.blob.as_str().to_owned(),
                )
            }
            _ => unreachable!(),
        };
        let ((), receipt) =
            lash_core_ids::operational_metrics::sql::collect("model.start", "postgres", async {
                let mut tx = store.begin(&actor, epoch).await.unwrap();
                tx.write(write);
                store
                    .commit(tx, CommitLabel::new("model.start"))
                    .await
                    .unwrap();
            })
            .await;
        let nodes: u64 = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| sql.starts_with("insert into") && sql.contains("prompt_texts "))
            .map(|(_, shape)| shape.rows_affected)
            .sum();
        let edges: u64 = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| {
                sql.starts_with("insert into")
                    && (sql.contains("prompt_text_edges ")
                        || sql.contains("prompt_snapshot_texts "))
            })
            .map(|(_, shape)| shape.rows_affected)
            .sum();
        let bytes: u64 = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| {
                (sql.starts_with("insert into") && sql.contains("prompt_"))
                    || (sql.starts_with("select hash from") && sql.contains("prompt_texts "))
            })
            .map(|(_, shape)| shape.bound_bytes)
            .sum();
        let logarithm = u64::from(ordinal.ilog2() + 1);
        assert!(
            nodes > 0 && edges > 0,
            "the SQL witness counted no new nodes or edges"
        );
        assert!(
            nodes <= 32 + 16 * logarithm,
            "call {ordinal} installed {nodes} nodes"
        );
        assert!(
            edges <= 64 + 32 * logarithm,
            "call {ordinal} installed {edges} edges"
        );
        assert!(
            bytes <= 24_576 + 8192 * logarithm,
            "call {ordinal} bound {bytes} bytes"
        );
        maximum_nodes = maximum_nodes.max(nodes);
        maximum_edges = maximum_edges.max(edges);
        let reopened = storage.durable_store();
        assert_eq!(
            *load_admitted_call(&reopened, &call)
                .await
                .unwrap()
                .unwrap()
                .template,
            template
        );
        all.extend(
            store
                .prompt_snapshot(&call)
                .await
                .unwrap()
                .unwrap()
                .request_texts,
        );
        let ((), settlement) =
            lash_core_ids::operational_metrics::sql::collect("model.done", "postgres", async {
                let mut tx = store.begin(&actor, epoch).await.unwrap();
                let ModelCallId::Turn { run, .. } = &call.call else {
                    unreachable!()
                };
                tx.write(DomainWrite::Prompt(PromptWrite::Settle {
                    session: session.clone(),
                    run: run.clone(),
                    open: None,
                }));
                store
                    .commit(tx, CommitLabel::new("model.done"))
                    .await
                    .unwrap();
            })
            .await;
        let settlement_bytes: u64 = settlement
            .shapes
            .values()
            .map(|shape| shape.bound_bytes)
            .sum();
        maximum_settlement_bytes = maximum_settlement_bytes.max(settlement_bytes);
        assert!(
            settlement_bytes <= 4096 + 1024 * logarithm,
            "call {ordinal} settlement bound {settlement_bytes} bytes of retained history metadata"
        );
        assert!(load_admitted_call(&store, &call).await.unwrap().is_none());
        assert!(
            store
                .prompt_texts(&[request_root])
                .await
                .unwrap()
                .is_empty(),
            "settlement retains a request root"
        );
        assert_eq!(
            store.prompt_texts(&[prefix]).await.unwrap().len(),
            1,
            "settlement loses the reusable history prefix"
        );
    }
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::ResetPrefix {
        session: session.clone(),
    }));
    store
        .commit(tx, CommitLabel::new("pressure.frame"))
        .await
        .unwrap();
    assert!(
        store
            .prompt_texts(&all.into_iter().collect::<Vec<_>>())
            .await
            .unwrap()
            .is_empty(),
        "invalidated history fragments survived"
    );
    // Session deletion releases both the new call and the reusable index.
    let wire = format!("{{\"messages\":[{}],\"stream\":true}}", messages.join(","));
    let template = RecordedRequestTemplate::literal(route, true, None, wire.as_str()).unwrap();
    let call = PromptCallKey {
        session: session.clone(),
        call: ModelCallId::Turn {
            run: lash_protocol_types::TurnId::from("fragment-delete"),
            ordinal: 1,
        },
    };
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(admission_record(call.clone(), None, &template, &response, None).unwrap());
    store
        .commit(tx, CommitLabel::new("model.start"))
        .await
        .unwrap();
    let retained = store
        .prompt_snapshot(&call)
        .await
        .unwrap()
        .unwrap()
        .request_texts;
    let mut tx = store.begin(&actor, epoch).await.unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::Release {
        session,
        run: None,
    }));
    store
        .commit(tx, CommitLabel::new("session.delete"))
        .await
        .unwrap();
    assert!(
        store.prompt_texts(&retained).await.unwrap().is_empty(),
        "session deletion retained a prefix"
    );
    eprintln!(
        "PREFIX-DELTA: 32 sequential calls, max new nodes={maximum_nodes}, max new edges={maximum_edges}, max settlement bytes={maximum_settlement_bytes}, byte-exact after every settlement and changed prefix"
    );
}

claim_law!(
    a_retired_owner_claims_only_its_registry_callbacks,
    |store, _advance| { laws::a_retired_owner_claims_only_its_registry_callbacks(store) }
);

law!(
    a_turn_counts_its_tool_admissions_atomically,
    |store, _advance| { laws::a_turn_counts_its_tool_admissions_atomically(store) }
);
