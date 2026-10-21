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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lo_idle_writer_when_configured() {
    use lash_core_execution::testing::store_fixtures::root_session_request;
    use lash_core_store::session_state::RuntimeSessionState;
    use lash_core_store::store::{RuntimeCommit, SessionCatalogStore, SessionCommitStore};
    use lash_durable::{ActorKey, DurableStore, FormatSet, MailTx, NodeId, NodeSpec};
    use lash_protocol_types::SessionId;

    let database = IsolatedDatabase::create(&crate::testing::required_database_url()).await;
    let storage = crate::testing::connect(database.url()).await.unwrap();
    let catalog = storage.store();
    let durable = storage.durable_store();
    let session = SessionId::fixture("lo-idle-writer");
    let request = root_session_request(&session);
    catalog.admit_session(&request).await.unwrap();
    let actor = ActorKey::session(session.as_str()).unwrap();
    let initial = durable.actor(&actor).await.unwrap();
    assert!(initial.is_some(), "initial head creates an idle actor");
    let initial = initial.unwrap();
    assert_eq!(initial.revision.0, 1, "initial head advances revision");
    catalog.admit_session(&request).await.unwrap();
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision,
        initial.revision
    );
    let state = RuntimeSessionState {
        session_id: session.clone(),
        ..RuntimeSessionState::fixture(lash_core_store::session_policy::SessionPolicy::new(
            request.config.turn_budget,
            request.config.max_tool_calls,
            request.config.no_progress_budget,
        ))
    };
    let commit = RuntimeCommit::persisted_state_for_test(&state);
    catalog.commit_runtime_state(commit.clone()).await.unwrap();
    assert_eq!(durable.actor(&actor).await.unwrap().unwrap().revision.0, 2);
    let formats = FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS);
    let node = durable
        .register_node(&NodeSpec {
            node: NodeId::new("idle-writer"),
            decodes: vec![formats],
            serves: vec![],
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    durable
        .commit_mail(wake, lash_durable::CommitLabel::new("law.wake"))
        .await
        .unwrap();
    let grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    assert_eq!(
        durable
            .begin(&actor, grant.epoch)
            .await
            .unwrap()
            .revision()
            .0,
        2
    );
    // Exact receipt replay is answer-only, even under the claim.
    assert!(
        catalog
            .commit_runtime_state(commit)
            .await
            .unwrap()
            .receipt_replayed
    );
    assert_eq!(durable.actor(&actor).await.unwrap().unwrap().revision.0, 2);
    // A separately created target is claimed before its first materialized
    // head. Naming a run is no authority over that claim either.
    let first = SessionId::fixture("lo-first-head");
    let first_request = root_session_request(&first);
    catalog.admit_session(&first_request).await.unwrap();
    let first_actor = ActorKey::session(first.as_str()).unwrap();
    let mut wake = MailTx::new();
    wake.wake(first_actor.clone());
    durable
        .commit_mail(wake, lash_durable::CommitLabel::new("law.wake-first"))
        .await
        .unwrap();
    let first_grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    assert_eq!(first_grant.actor, first_actor);
    let first_state = RuntimeSessionState {
        session_id: first.clone(),
        ..state.clone()
    };
    assert!(matches!(
        catalog
            .commit_runtime_state(RuntimeCommit::persisted_state_for_test(&first_state))
            .await,
        Err(lash_core_store::store::StoreError::SessionHeadOwned {
            owner: lash_core_store::store::SessionHeadOwner::Actor { .. },
            ..
        })
    ));
    assert_eq!(
        durable
            .actor(&first_actor)
            .await
            .unwrap()
            .unwrap()
            .revision
            .0,
        1
    );
    let target = SessionId::fixture("lo-fork-target");
    catalog
        .fork_session(
            &lash_core_store::session_store_factory_types::ForkSessionRequest {
                session_id: target.clone(),
                source_session_id: session.clone(),
                head_revision: 0,
                config: request.config.clone(),
                relation: lash_core_store::session_identity::SessionRelation::Root,
                pending_observer_intents: Vec::new(),
                retention: lash_core_store::store::Retention::UntilGc,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        durable
            .actor(&ActorKey::session(target.as_str()).unwrap())
            .await
            .unwrap()
            .unwrap()
            .revision
            .0,
        1
    );
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision.0,
        2,
        "fork only advances target"
    );
    let mut fresh_state = state;
    fresh_state.head_revision = 1;
    let fresh = RuntimeCommit::persisted_state_for_test(&fresh_state);
    let mut spoofed = fresh.clone();
    spoofed.turn_commit.operation = lash_core_store::store::OperationId::turn(
        &session,
        lash_protocol_types::TurnId::fixture("spoofed-owner"),
        "commit",
    );
    assert!(spoofed.is_sessions_own_head_write());
    assert!(matches!(
        catalog.commit_runtime_state(spoofed).await,
        Err(lash_core_store::store::StoreError::SessionHeadOwned {
            owner: lash_core_store::store::SessionHeadOwner::Actor { .. },
            ..
        })
    ));

    assert!(matches!(
        catalog.commit_runtime_state(fresh).await,
        Err(lash_core_store::store::StoreError::SessionHeadOwned {
            owner: lash_core_store::store::SessionHeadOwner::Actor { .. },
            ..
        })
    ));
    assert_eq!(durable.actor(&actor).await.unwrap().unwrap().revision.0, 2);
    // The host's dirty flush has no grant. Refusal preserves the draft;
    // after relinquishing the actual grant it can flush, then cleanly no-op.
    use lash_core::testing::runtime_helpers::{
        EmptyTools, plugin_session_with_tools, standard_test_policy, test_host_config,
    };
    use lash_execution_contracts::backend::{Backend, BackendParts};
    let backend = Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: Arc::new(crate::PostgresStoreSet::new(
            &storage,
            Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
        )),
        settings: lash_core_execution::runtime::actor::wait_laws::settings(),
        engines: Vec::new(),
        providers: Arc::new(lash_execution_contracts::projection::NoProjectionProviders),
    })
    .unwrap();
    let dirty = SessionId::fixture("lo-dirty-flush");
    let policy = standard_test_policy();
    let factory = backend.session_store_factory();
    let mut request = root_session_request(&dirty);
    request.config = lash_core_store::session_graph::PersistedSessionConfig::from_policy(&policy);
    let view = lash_core::testing::runtime_helpers::create_session_store(&factory, &request)
        .await
        .unwrap();
    let host = test_host_config(&backend);
    let services = lash_core::facade_support::PersistentRuntimeServices::new(
        plugin_session_with_tools(&dirty, Arc::new(EmptyTools)),
        view,
        Arc::clone(&host.core.durability.attachment_store),
        Arc::clone(&host.core.durability.process_env_store),
    );
    let mut runtime = lash_core::facade_support::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        host,
        services,
        RuntimeSessionState {
            session_id: dirty.clone(),
            policy,
            ..RuntimeSessionState::fixture(standard_test_policy())
        },
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    let dirty_actor = ActorKey::session(dirty.as_str()).unwrap();
    let mut wake = MailTx::new();
    wake.wake(dirty_actor.clone());
    durable
        .commit_mail(wake, lash_durable::CommitLabel::new("law.dirty-wake"))
        .await
        .unwrap();
    let dirty_grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    assert_eq!(dirty_grant.actor, dirty_actor);
    let pending = runtime.state().pending_graph_commit().nodes().len();
    let error = runtime
        .flush_for_park()
        .await
        .expect_err("dirty host flush needs unclaimed authority");
    assert!(matches!(
        error,
        lash_core::SessionError::Store {
            source: lash_core_store::store::StoreError::SessionHeadOwned { .. },
            ..
        }
    ));
    assert_eq!(
        runtime.state().pending_graph_commit().nodes().len(),
        pending
    );
    assert!(runtime.state().checkpoint_ref.is_none());
    assert_eq!(
        durable
            .actor(&dirty_actor)
            .await
            .unwrap()
            .unwrap()
            .revision
            .0,
        1
    );
    let mut release = durable
        .begin(&dirty_actor, dirty_grant.epoch)
        .await
        .unwrap();
    release.ack_seen().give_up(lash_durable::Release::Idle);
    durable
        .commit(release, lash_durable::CommitLabel::new("law.dirty-release"))
        .await
        .unwrap();
    runtime.flush_for_park().await.unwrap();
    assert!(runtime.state().checkpoint_ref.is_some());
    let revision = durable.actor(&dirty_actor).await.unwrap().unwrap().revision;
    runtime.flush_for_park().await.unwrap();
    assert_eq!(
        durable.actor(&dirty_actor).await.unwrap().unwrap().revision,
        revision,
        "clean flush writes nothing"
    );

    // Admission planned under the old grant cannot publish after a direct
    // write advanced the relinquished actor. Its graph/head stays untouched.
    let mut stale = durable.begin(&actor, grant.epoch).await.unwrap();
    let mut release = durable.begin(&actor, grant.epoch).await.unwrap();
    release.ack_seen().give_up(lash_durable::Release::Idle);
    durable
        .commit(release, lash_durable::CommitLabel::new("law.release-idle"))
        .await
        .unwrap();
    let direct = RuntimeCommit::persisted_state_for_test(&fresh_state);
    catalog.commit_runtime_state(direct.clone()).await.unwrap();
    let revision = durable.actor(&actor).await.unwrap().unwrap().revision;
    stale.write(lash_durable::domain::DomainWrite::SessionCommit(
        lash_durable::domain::SessionCommitWrite {
            session: session.clone(),
            expected_head: direct.expected_head_revision,
            commit_json: lash_core_store::store::encode_session_commit(&direct)
                .unwrap()
                .json,
            execution: None,
        },
    ));
    assert!(matches!(
        durable
            .commit(stale, lash_durable::CommitLabel::new("law.stale-admission"))
            .await,
        Err(lash_durable::DurableError::OwnershipLost(_))
    ));
    assert_eq!(
        catalog
            .load_session_head_meta(&session)
            .await
            .unwrap()
            .unwrap()
            .head_revision,
        2
    );
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision,
        revision
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lo_idle_writer_close_and_redrive_when_configured() {
    use lash_core_execution::testing::store_fixtures::root_session_request;
    use lash_core_store::store::{SessionCatalogStore, SessionLookup};
    use lash_durable::domain::{
        DomainWrite, MailAnswer, MailDomainWrite, RedriveAnswer, RedriveRequest, SessionCloseStep,
        SessionCloseWrite,
    };
    use lash_durable::{
        ActorKey, CommitLabel, DurableReads, DurableStore, FormatSet, MailTx, NodeId, NodeSpec,
        Release,
    };
    use lash_protocol_types::SessionId;

    let database = IsolatedDatabase::create(&crate::testing::required_database_url()).await;
    let storage = crate::testing::connect(database.url()).await.unwrap();
    let catalog = storage.store();
    let durable = storage.durable_store();
    let session = SessionId::fixture("lo-close");
    catalog
        .admit_session(&root_session_request(&session))
        .await
        .unwrap();
    let actor = ActorKey::session(session.as_str()).unwrap();
    let formats = FormatSet::new(lash_durable::domain::SESSION_ACTOR_FORMATS);
    let node = durable
        .register_node(&NodeSpec {
            node: NodeId::new("close-writer"),
            decodes: vec![formats.clone()],
            serves: vec![],
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    durable
        .commit_mail(wake, CommitLabel::new("law.wake"))
        .await
        .unwrap();
    let grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    let before = durable.actor(&actor).await.unwrap().unwrap();
    let refusal = catalog
        .delete_session(&session)
        .await
        .expect_err("public deletion cannot delete a claimed session");
    assert!(matches!(
        refusal.stop,
        lash_core_store::store::MaintenanceStop::Failed(
            lash_core_store::store::StoreError::SessionHeadOwned { .. }
        )
    ));
    assert_eq!(
        refusal.partial,
        Default::default(),
        "no partial maintenance work"
    );
    assert!(matches!(
        catalog.lookup_session(&session).await.unwrap(),
        SessionLookup::Live(_)
    ));
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision,
        before.revision
    );
    let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
    tx.write(DomainWrite::ActorPark {
        reason_json: Some("diagnostic park".into()),
    });
    durable
        .commit(tx, CommitLabel::new("law.diagnostic"))
        .await
        .unwrap();
    let before = durable.actor(&actor).await.unwrap().unwrap();
    let redrive = || {
        let mut tx = MailTx::new();
        tx.write(MailDomainWrite::Redrive(RedriveRequest {
            actor: actor.clone(),
            requester: "law".into(),
        }));
        tx
    };
    let answer = durable
        .commit_mail(redrive(), CommitLabel::new("law.redrive-owned"))
        .await
        .unwrap();
    assert!(matches!(
        answer.answers.as_slice(),
        [MailAnswer::Redrive(RedriveAnswer::NotParked)]
    ));
    assert!(answer.woken.is_empty());
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision,
        before.revision
    );
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().park,
        before.park
    );
    let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
    let seen = tx.seen();
    tx.ack_seen().give_up(Release::Parked);
    durable
        .commit(tx, CommitLabel::new("law.park"))
        .await
        .unwrap();
    let parked_revision = durable.actor(&actor).await.unwrap().unwrap().revision;
    let answer = durable
        .commit_mail(redrive(), CommitLabel::new("law.redrive-parked"))
        .await
        .unwrap();
    assert!(matches!(
        answer.answers.as_slice(),
        [MailAnswer::Redrive(RedriveAnswer::Redriven)]
    ));
    assert_eq!(answer.woken.len(), 1);
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision.0,
        parked_revision.0 + 1
    );
    let grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
    assert_eq!(
        tx.seen().0,
        seen.0 + 1,
        "redrive takes one mailbox position"
    );
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Begin {
        session: session.clone(),
    }));
    durable
        .commit(tx, CommitLabel::new("law.close-begin"))
        .await
        .unwrap();
    for step in [
        SessionCloseStep::Cancel,
        SessionCloseStep::Revoke,
        SessionCloseStep::EndScope,
    ] {
        let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
        tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
            session: session.clone(),
            step,
        }));
        durable.commit(tx, step.label()).await.unwrap();
    }
    // A draft whose grant was released cannot reach Artifacts either.
    let mut stale = durable.begin(&actor, grant.epoch).await.unwrap();
    stale.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step: SessionCloseStep::Artifacts,
    }));
    let mut release = durable.begin(&actor, grant.epoch).await.unwrap();
    release.ack_seen().give_up(Release::Idle);
    durable
        .commit(release, CommitLabel::new("law.release-close"))
        .await
        .unwrap();
    assert!(matches!(
        durable
            .commit(stale, CommitLabel::new("law.stale-artifacts"))
            .await,
        Err(lash_durable::DurableError::OwnershipLost(_))
    ));
    assert!(matches!(
        catalog.lookup_session(&session).await.unwrap(),
        SessionLookup::Live(_)
    ));
    let mut wake = MailTx::new();
    wake.wake(actor.clone());
    durable
        .commit_mail(wake, CommitLabel::new("law.wake-close"))
        .await
        .unwrap();
    let grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    // Another actor's valid fence cannot delete this session.
    let foreign = ActorKey::process("lo-foreign-close").unwrap();
    let mut create = MailTx::new();
    create.create_actor(foreign.clone(), formats);
    durable
        .commit_mail(create, CommitLabel::new("law.foreign"))
        .await
        .unwrap();
    let foreign_grant = durable.claim(&node, 1).await.unwrap().pop().unwrap();
    let mut tx = durable.begin(&foreign, foreign_grant.epoch).await.unwrap();
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step: SessionCloseStep::Artifacts,
    }));
    assert!(matches!(
        durable
            .commit(tx, CommitLabel::new("law.foreign-artifacts"))
            .await,
        Err(lash_durable::DurableError::Store(
            lash_durable::StoreFailure {
                kind: lash_durable::StoreFailureKind::Corrupt,
                ..
            }
        ))
    ));
    assert!(matches!(
        catalog.lookup_session(&session).await.unwrap(),
        SessionLookup::Live(_)
    ));
    // Failure after deletion but before the transaction commits rolls all
    // artifacts and the recorded step back together.
    let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step: SessionCloseStep::Artifacts,
    }));
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step: SessionCloseStep::Cancel,
    }));
    assert!(
        durable
            .commit(tx, CommitLabel::new("law.rollback-artifacts"))
            .await
            .is_err()
    );
    assert!(matches!(
        catalog.lookup_session(&session).await.unwrap(),
        SessionLookup::Live(_)
    ));
    assert_eq!(
        durable.session_close(&session).await.unwrap().unwrap().done,
        Some(SessionCloseStep::EndScope)
    );
    let mut tx = durable.begin(&actor, grant.epoch).await.unwrap();
    tx.write(DomainWrite::SessionClose(SessionCloseWrite::Step {
        session: session.clone(),
        step: SessionCloseStep::Artifacts,
    }));
    durable
        .commit(tx, CommitLabel::new("law.artifacts"))
        .await
        .unwrap();
    assert!(matches!(
        catalog.lookup_session(&session).await.unwrap(),
        SessionLookup::Deleted
    ));
    assert_eq!(
        durable.session_close(&session).await.unwrap().unwrap().done,
        Some(SessionCloseStep::Artifacts)
    );
    // Repeated public deletion is answer-only even with the actor kept
    // for the later Tombstone transition.
    let revision = durable.actor(&actor).await.unwrap().unwrap().revision;
    catalog.delete_session(&session).await.unwrap();
    assert_eq!(
        durable.actor(&actor).await.unwrap().unwrap().revision,
        revision
    );
}
