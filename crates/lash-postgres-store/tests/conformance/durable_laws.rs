//! The engine-neutral laws that need an effect context, run on
//! the durable backend over PostgreSQL (ADR 0132 §14, FIG-5185).
//!
//! Each law's runtime runs in process over the production durable backend
//! assembled over an isolated database's store set, on an [`ActorContext`]
//! over that backend.

use std::sync::Arc;

use lash_conformance::ReopenableRuntimeStore;
use lash_core_execution::ActorContext;
use lash_core_execution::store::ConformanceDeployment;
use lash_core_store::store::SessionCatalogStore as _;
use lash_execution_contracts::backend::StoreSet;

use super::*;

/// The effect context a law's turns run on: the durable backend over
/// `stores`, owning no actor.
fn durable_host(stores: &Arc<dyn StoreSet>) -> ActorContext {
    ActorContext::detached(lash_conformance::backend_over(Arc::clone(stores)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingress_plugin_callbacks_publish_state_that_survives_a_checkpoint() {
    let (_database_fixture, storage) = storage()
        .await
        .expect("PostgreSQL law requires its isolated database");
    reset(storage.pool()).await;
    let (_attachments, stores) = pg_law_stores(&storage);
    let store = Arc::new(storage.store());
    store
        .admit_session(&root_session_request("ingress-callback-state"))
        .await
        .expect("admit the callback's session");
    lash_conformance::ingress_plugin_callbacks_publish_state_that_survives_a_checkpoint(
        store,
        durable_host(&stores),
    )
    .await;
}

fn root_session_request(
    session_id: &str,
) -> lash_core_store::session_store_factory_types::SessionStoreCreateRequest {
    lash_core_store::session_store_factory_types::SessionStoreCreateRequest {
        owning_process_id: None,
        pending_observer_intents: Vec::new(),
        session_id: SessionId::fixture(session_id.to_string()),
        relation: lash_core_store::session_identity::SessionRelation::Root,
        config: lash_core_store::session_graph::PersistedSessionConfig::new(
            lash_protocol_types::session_model::TurnBudget::Unbounded,
            lash_protocol_types::session_model::MaxToolCalls::new(1024),
            lash_core::NoProgressBudget::bounded(12),
        ),
        head: lash_core_store::session_store_factory_types::SessionCreationHead::Config,
        retention: lash_core_store::store::Retention::UntilGc,
    }
}

// The settlement laws run a facade runtime over a fresh durable backend per
// law, over this database's store set.
lash_conformance::session_config_settlement_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    let make = move || {
        let stores = Arc::clone(&stores);
        async move { lash_conformance::backend_over(stores) }
    };
    ((database_fixture, attachments), make)
});

/// The deployment-store fixture: one isolated database, reset for each store
/// a law asks for, and the durable host over it.
macro_rules! session_store_factory_fixture {
    () => {{
        let Some((database_fixture, storage)) = storage().await else {
            return;
        };
        let (host_attachments, host_stores) = pg_law_stores(&storage);
        let host = durable_host(&host_stores);
        let storage = Arc::new(storage);
        let make_storage = Arc::clone(&storage);
        let make = move || {
            let storage = Arc::clone(&make_storage);
            sync_await(async move {
                reset(storage.pool()).await;
                Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>
            })
        };
        let attachments = Arc::new(tempfile::tempdir().expect("attachment directory"));
        let attached_storage = Arc::clone(&storage);
        let attached_root = Arc::clone(&attachments);
        let make_attached = move || {
            let storage = Arc::clone(&attached_storage);
            let root = attached_root.path().join(uuid::Uuid::new_v4().to_string());
            sync_await(async move {
                reset(storage.pool()).await;
                (
                    Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>,
                    lash_sqlite_store::SqliteStoreSet::open(
                        (root).join("attachments.db"),
                        lash_sqlite_store::SqliteSynchronous::Normal,
                    )
                    .await
                    .expect("SQLite attachment store")
                    .attachment_store()
                        as Arc<dyn lash_core_store::attachments::AttachmentStore>,
                )
            })
        };
        (
            (database_fixture, attachments, host_attachments),
            make,
            make_attached,
            host,
        )
    }};
}

lash_conformance::session_store_factory_tests!(@catalogue { session_store_factory_fixture!() }; [
    (session_store_factory, "session-store-factory"),
]);
lash_conformance::session_store_factory_tests!(@turn_cancel { session_store_factory_fixture!() }; [
    (session_meta_records_the_process_that_owns_it, "session-meta-owning-process"),
    (a_closing_session_lists_as_closing_never_as_live, "catalog-closing-entry"),
    (concurrent_session_admissions_preserve_one_relation, "concurrent-session-relation"),
    (a_session_that_never_ran_a_turn_forks_at_its_creation_revision, "fork-empty-session"),
]);

/// A fork inherits its source's history and none of its execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fork_inherits_history_without_execution_queues_waits_or_journals() {
    let (_database_fixture, storage) = storage()
        .await
        .expect("PostgreSQL law requires its isolated database");
    reset(storage.pool()).await;
    let (_attachments, stores) = pg_law_stores(&storage);
    lash_conformance::registration_macro_support::fork_inherits_history_without_execution_queues_waits_or_journals(
        Arc::new(storage.session_store_factory()) as Arc<dyn ConformanceDeployment>,
        durable_host(&stores),
    )
    .await;
}

lash_conformance::process_prune_session_store_tests!({
    let Some((database_fixture, storage)) = storage().await else {
        return;
    };
    reset(storage.pool()).await;
    let (attachments, stores) = pg_law_stores(&storage);
    let factory = Arc::new(storage.store()) as Arc<dyn DeploymentStore>;
    let registry = Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>;
    (
        (database_fixture, attachments),
        factory,
        registry,
        durable_host(&stores),
    )
});

mod runtime_persistence {
    use super::*;

    lash_conformance::__runtime_persistence_register! {
            reopenable {
        let Some((database_fixture, storage)) = storage().await else {
            return;
        };
        // One reset per law: a law that opens several sessions (the factory
        // laws) admits each through `make`, and the catalog must keep them all.
        reset(storage.pool()).await;
        let (attachments, host_stores) = pg_law_stores(&storage);
        let effect_host = durable_host(&host_stores);
        drop(storage);
        let database_url = database_fixture.url().to_owned();
        let clock = Arc::new(lash_core_ids::test_clock::TestClock::new(10_000));
        let lease_clock = Arc::clone(&clock);
        (
            (database_fixture, attachments),
            move |session_id: &str| {
                let effect_host = effect_host.clone();
                let database_url = database_url.clone();
                let clock = Arc::clone(&clock);
                let request = root_session_request(session_id);
                sync_await(async move {
                    let open_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open first Postgres conformance pool");
                    let reopen_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open independent Postgres conformance pool");
                    let open_factory = open_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>)
                        .with_lease_clock_for_testing(
                            Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>
                        );
                    let reopen_factory = reopen_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>)
                        .with_lease_clock_for_testing(
                            clock as Arc<dyn lash_core_ids::clock::Clock>,
                        );
                    open_factory
                        .admit_session(&request)
                        .await
                        .expect("admit Postgres conformance session");
                    ReopenableRuntimeStore {
                        open: Arc::new(open_factory) as Arc<dyn RuntimeStore>,
                        reopen: Arc::new(reopen_factory) as Arc<dyn RuntimeStore>,
                        effect_host,
                    }
                })
            },
            lash_conformance::RuntimePersistenceLeaseTiming::controlled(move |ms| {
                lease_clock.advance(ms)
            }),
        )
    };
            stores [
            (commit_increments_head_and_round_trips_agent_frames, "root"),
            (receipt_replay_rehydrates_recorded_node_clocks_and_ids, "root"),

            (serves_each_admitted_session_and_refuses_an_unknown_one, "alpha"),
            (commit_rejects_carried_nondefault_node_budget, "root"),
            (commit_rejects_carried_nondefault_byte_budget, "root"),
            (commit_rejects_turn_result_bytes_over_budget, "root"),

            (checkpoint_restore_rejects_turn_index_without_increment_headroom, "root"),
            (checkpoint_restore_rejects_token_usage_whose_prompt_subtotal_overflows, "root"),

            (session_read_loads_persisted_history, "branchy"),
            (session_plugin_config_round_trips_through_the_committed_head, "session-plugin-config"),

            (head_and_window_reads_agree_for_each_named_session, "read-agreement"),
            (observer_settlement_requires_admission, "root"),
            (observer_settlement_preserves_creation_facts, "root"),
            (attachment_writes_keep_independent_referrers, "root"),
            (attachment_acquisition_preserves_receiving_referrer, "root"),
            (append_request_receipt_replays_after_head_advance, "root"),
            (append_request_receipt_rejects_changed_content, "root"),
            (append_request_exact_hash_rejects_changed_ancestor, "root"),
            (append_request_receipt_rejects_corrupt_node_count, "root"),
            (semantic_boundary_receipt_replays_after_head_advance, "root"),
            (semantic_boundary_receipt_rejects_changed_content, "root"),
            (semantic_boundary_receipt_rejects_mislabeled_identity, "root"),
            (concurrent_same_append_operation_applies_exactly_once, "root"),
            (legacy_append_receipt_keeps_exact_hash_semantics, "root"),
            (append_receipt_encoding_version_mismatch_keeps_exact_hash_semantics, "root"),
            (append_receipt_and_graph_append_are_atomic, "root"),
            (fresh_append_receipt_enforces_ancestor_precondition, "root"),
            (commit_rejects_non_derived_append_node_ids, "root"),
            (append_rejects_duplicate_batch_node_ids, "root"),
            (append_rejects_existing_node_id_collision, "root"),

            (committed_leaf_is_derived_from_the_terminal_appended_node, "root"),
            (preserve_head_commit_reports_the_resident_leaf, "root"),

            (commit_rejects_leaf_without_frame_open_ancestor, "missing-frame-run"),
            (queued_work_source_keys_are_idempotent_and_list_ordered, "queued-work-source-keys"),
            (concurrent_queued_work_source_key_enqueues_report_one_inserted_and_one_existing, "concurrent-queued-work-source-key"),
            (decorated_queued_work_source_key_replay_reports_absorbed, "decorated-queued-work-source-key"),
            (pending_session_work_ordering_agrees_across_ingress_families, "pending-work-ordering"),
            (delete_then_enqueue_never_reuses_ingress_sequences, "root"),
            (pending_turn_inputs_source_keys_order_cancel_and_cross_session, "root"),
            (pending_turn_input_duplicate_input_id, "root"),
            (run_specs_join_the_submission_digest_and_intern_once, "run-specs"),
            (run_spec_bytes_are_reclaimed_once_no_unsettled_input_names_them, "run-spec-reclaim"),
            (a_turn_input_batch_enqueues_new_ids_contiguously_in_request_order, "turn-input-batches"),
            (a_resent_turn_input_batch_answers_its_existing_ids_and_enqueues_the_rest, "turn-input-batch-retries"),
            (a_conflict_or_a_repeated_id_refuses_the_whole_turn_input_batch, "turn-input-batch-refusals"),
            (a_changed_resubmission_is_a_typed_conflict_for_every_kind, "ingress-content-conflict"),

            (a_settled_command_resubmitted_under_its_key_is_not_a_new_command, "ingress-command-resubmission"),
            ]
            hosted_stores [
            ]
            store_refs [
            ]
            factories [
                (concurrent_head_revision_cas_applies_exactly_once, "concurrent-head-cas"),
            (plugin_state_boundary, "plugin-state"),
            ]
            timed_stores [
            ]
            timed_factories [
            ]
        }

    lash_conformance::runtime_persistence_reopenable_tests!(@reopen_laws {
        let Some((database_fixture, storage)) = storage().await else {
            return;
        };
        // One reset per law: a law that opens several sessions (the factory
        // laws) admits each through `make`, and the catalog must keep them all.
        reset(storage.pool()).await;
        let (attachments, host_stores) = pg_law_stores(&storage);
        let effect_host = durable_host(&host_stores);
        drop(storage);
        let database_url = database_fixture.url().to_owned();
        let clock = Arc::new(lash_core_ids::test_clock::TestClock::new(10_000));
        let lease_clock = Arc::clone(&clock);
        (
            (database_fixture, attachments),
            move |session_id: &str| {
                let effect_host = effect_host.clone();
                let database_url = database_url.clone();
                let clock = Arc::clone(&clock);
                let request = root_session_request(session_id);
                sync_await(async move {
                    let open_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open first Postgres conformance pool");
                    let reopen_storage = lash_postgres_store::testing::connect(&database_url)
                        .await
                        .expect("open independent Postgres conformance pool");
                    let open_factory = open_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>)
                        .with_lease_clock_for_testing(
                            Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>
                        );
                    let reopen_factory = reopen_storage
                        .session_store_factory()
                        .with_clock(Arc::clone(&clock) as Arc<dyn lash_core_ids::clock::Clock>)
                        .with_lease_clock_for_testing(
                            clock as Arc<dyn lash_core_ids::clock::Clock>,
                        );
                    open_factory
                        .admit_session(&request)
                        .await
                        .expect("admit Postgres conformance session");
                    ReopenableRuntimeStore {
                        open: Arc::new(open_factory) as Arc<dyn RuntimeStore>,
                        reopen: Arc::new(reopen_factory) as Arc<dyn RuntimeStore>,
                        effect_host,
                    }
                })
            },
            lash_conformance::RuntimePersistenceLeaseTiming::controlled(move |ms| {
                lease_clock.advance(ms)
            }),
        )
    }; [(reopen_mint_identity, "pending-turn-input-multi-store-mint"), (gc_blobs, "gc-blobs"), (append_receipt_reopen, "root")]);
}
