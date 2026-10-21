//! The durability engine's fencing laws (`lash_durable::laws`) over SQLite,
//! in memory and in a file.

use lash_core_store::store::BlobRef;
use std::sync::Arc;
use std::time::Duration;

use lash_core_ids::test_clock::TestClock;
use lash_durable::laws;

use crate::SqliteStoreSet;

/// PROCESS-ROW-WORK: a single-statement row read needs no explicit read
/// transaction; SQLite supplies its snapshot for the statement itself.
#[tokio::test]
async fn a_process_row_read_uses_only_its_select() {
    use lash_durable::DurableReads as _;
    use lash_execution_contracts::process::ProcessRegistrar as _;

    let set = SqliteStoreSet::memory().await.unwrap();
    lash_execution_contracts::testing::process_execution_env_fixture(
        set.process_env_store().as_ref(),
    )
    .await;
    let registered = set
        .process_registry()
        .register_process(lash_execution_contracts::testing::held_engine_registration(
            serde_json::Value::Null,
            lash_execution_contracts::process::ProcessProvenance::host(),
            lash_execution_contracts::process::Lifetime::Detached,
        ))
        .await
        .unwrap();
    let store = set.durable_store();
    let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "process.row",
        "sqlite",
        store.process(&registered.id),
    )
    .await;
    let row = answer.unwrap().unwrap();
    assert_eq!(row.process, registered.id);
    assert!(!row.terminal);
    eprintln!(
        "PROCESS-ROW SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(receipt.statements, 1, "the SELECT owns its snapshot");
}

/// DURABLE-METADATA-READ-WORK: a single SELECT already owns an atomic
/// SQLite snapshot; reading metadata opens no redundant transaction.
#[tokio::test]
async fn single_statement_durable_reads_use_their_implicit_snapshots() {
    use lash_durable::DurableReads as _;
    use lash_durable::domain::ExecKey;

    let set = SqliteStoreSet::memory().await.unwrap();
    let store = set.durable_store();
    let session = lash_protocol_types::SessionId::fixture("metadata-read-work");
    let run = lash_protocol_types::TurnId::fixture("metadata-read-run");
    let exec = ExecKey::Cell(
        session.clone(),
        run.clone(),
        lash_durable::domain::CellId::new("metadata"),
    );
    let ((), receipt) =
        lash_core_ids::operational_metrics::sql::collect("durable.metadata", "sqlite", async {
            assert!(store.turn_end(&session, &run).await.unwrap().is_none());
            assert!(store.exec_root(&exec).await.unwrap().is_none());
            assert!(store.cell_roots(&session, &run).await.unwrap().is_empty());
            assert!(
                store
                    .cell_roots_within(&session, &run, 1)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(store.session_close(&session).await.unwrap().is_none());
            assert!(store.ending_scopes(&session).await.unwrap().is_empty());
        })
        .await;
    eprintln!(
        "DURABLE_METADATA_SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(
        receipt.statements, 6,
        "each metadata SELECT owns its snapshot"
    );
}

/// A fresh store set on a clock the law moves, and the directory a file set
/// lives in.
async fn world(file: bool) -> (Option<tempfile::TempDir>, SqliteStoreSet, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000_000));
    if file {
        let root = tempfile::tempdir().expect("store root");
        let set = SqliteStoreSet::open_with_clock(
            root.path().join("lash.db"),
            crate::SqliteSynchronous::Normal,
            clock.clone(),
        )
        .await
        .expect("open the file store set");
        (Some(root), set, clock)
    } else {
        let set = SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .expect("open the memory store set");
        (None, set, clock)
    }
}

fn advancing(clock: &Arc<TestClock>) -> impl Fn(Duration) + Sync + '_ {
    move |by| clock.advance(u64::try_from(by.as_millis()).expect("a law advances by millis"))
}

/// CLAIM-BATCH-WORK: a claim mutates bounded identity sets, preserving the
/// selected age order, Ready/Due causes, purposes, requirements and epochs.
#[tokio::test]
async fn a_claim_batches_mutations_without_losing_selected_facts() {
    use lash_durable::{
        ActorKey, ClaimCause, ClaimPurpose, CommitLabel, DurableInstant, DurableStore, Epoch,
        FormatSet, MailKind, MailTx, NodeId, NodeSpec, Release,
    };

    for count in [16_usize, 257] {
        let (_root, set, clock) = world(false).await;
        let store = set.durable_store();
        let formats = FormatSet::new("claim-batch");
        let spec = |name: &str, serves| NodeSpec {
            node: NodeId::new(name),
            decodes: vec![formats.clone()],
            serves,
            capabilities: Default::default(),
            ttl_millis: 15_000,
        };
        let setup = store
            .register_node(&spec("setup", vec!["served".into(), "unserved".into()]))
            .await
            .unwrap();
        // Reverse key order so SQLite's RETURNING order cannot stand in
        // for the selected eligibility order.
        let actors = (0..count)
            .map(|index| ActorKey::process(&format!("batch-{:04}", count - index)).unwrap())
            .collect::<Vec<_>>();
        let mut create = MailTx::new();
        for actor in &actors {
            create.create_actor(actor.clone(), formats.clone());
        }
        store
            .commit_mail(create, CommitLabel::new("law.create"))
            .await
            .unwrap();
        let initial = store.claim(&setup, count).await.unwrap();
        assert_eq!(initial.len(), count);
        for (index, actor) in actors.iter().enumerate() {
            let grant = initial.iter().find(|claim| &claim.actor == actor).unwrap();
            let mut tx = store.begin(actor, grant.epoch).await.unwrap();
            match index % 4 {
                0 => {
                    tx.give_up(Release::Waiting {
                        next_due: Some(DurableInstant(1_000_000 + i64::try_from(index).unwrap())),
                    });
                }
                2 => {
                    tx.set_requires(vec!["unserved".into()])
                        .give_up(Release::Ready);
                }
                3 => {
                    tx.set_requires(vec!["served".into()])
                        .give_up(Release::Ready);
                }
                _ => {
                    tx.give_up(Release::Ready);
                }
            }
            store
                .commit(tx, CommitLabel::new("law.release"))
                .await
                .unwrap();
            if index % 4 == 2 {
                let mut cancel = MailTx::new();
                cancel.append(
                    actor.clone(),
                    MailKind::new(lash_durable::domain::CANCEL_MAIL),
                    "{}",
                );
                store
                    .commit_mail(cancel, CommitLabel::new("law.cancel"))
                    .await
                    .unwrap();
            }
            clock.advance(1);
        }
        let chooser = store
            .register_node(&spec("chooser", vec!["served".into()]))
            .await
            .unwrap();
        let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
            "claim.batch",
            "sqlite",
            store.claim(&chooser, count),
        )
        .await;
        let claimed = answer.unwrap();
        assert_eq!(claimed.len(), count);
        for (index, (claim, actor)) in claimed.iter().zip(&actors).enumerate() {
            assert_eq!(&claim.actor, actor, "selected age order");
            assert_eq!(claim.epoch, Epoch(3), "one claim increment after release");
            assert_eq!(
                claim.cause,
                if index % 4 == 0 {
                    ClaimCause::Due
                } else {
                    ClaimCause::Ready
                }
            );
            assert_eq!(
                claim.purpose,
                if index % 4 == 2 {
                    ClaimPurpose::CancelOnly
                } else {
                    ClaimPurpose::Run
                }
            );
            let requires: Vec<String> = match index % 4 {
                2 => vec!["unserved".into()],
                3 => vec!["served".into()],
                _ => Vec::new(),
            };
            assert_eq!(claim.requires, requires);
            let stored = store.actor(actor).await.unwrap().unwrap();
            assert_eq!(stored.epoch, claim.epoch);
            assert_eq!(stored.owner.as_ref(), Some(&chooser.owner));
        }
        let updates: u64 = receipt
            .shapes
            .iter()
            .filter(|(shape, _)| shape.starts_with("update main.actors "))
            .map(|(_, work)| work.statements)
            .sum();
        eprintln!(
            "CLAIM_BATCH count={count} identities={} updates={updates} SQL {}",
            claimed.len(),
            serde_json::to_string(&receipt).unwrap()
        );
        let chunks = u64::try_from(count.div_ceil(256)).unwrap();
        assert!(
            receipt.statements <= 5 + chunks,
            "{count} actors used {} statements; budget {}",
            receipt.statements,
            5 + chunks
        );
        assert_eq!(updates, chunks, "one mutation per bounded identity set");
    }
}

macro_rules! law {
    ($name:ident, |$store:ident, $advance:ident| $law:expr) => {
        mod $name {
            use super::*;

            async fn run(file: bool) {
                let (_root, set, clock) = world(file).await;
                let $store = &set.durable_store();
                let $advance = &advancing(&clock);
                $law.await.unwrap_or_else(|broken| panic!("{broken}"));
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn memory() {
                run(false).await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn file() {
                run(true).await;
            }
        }
    };
}

law!(
    a_replaced_boot_adopts_none_of_its_successors_actors,
    |store, _advance| { laws::a_replaced_boot_adopts_none_of_its_successors_actors(store) }
);
law!(
    a_boot_redrives_only_the_session_parks_it_names,
    |store, _advance| { laws::a_boot_redrives_only_the_session_parks_it_names(store) }
);
law!(
    progress_before_an_empty_ready_release_resets_failed_activations,
    |store, _advance| {
        laws::progress_before_an_empty_ready_release_resets_failed_activations(store)
    }
);
law!(
    claims_take_the_oldest_eligible_actors_first,
    |store, advance| { laws::claims_take_the_oldest_eligible_actors_first(store, advance) }
);
law!(
    an_abandoned_activation_hands_back_only_its_original_grant,
    |store, _advance| {
        laws::an_abandoned_activation_hands_back_only_its_original_grant(Arc::new(store.clone()))
    }
);

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

mod deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text {
    use super::*;

    async fn run(file: bool) {
        let (_root, set, _clock) = world(file).await;
        laws::deleting_a_session_releases_its_prompt_roots_and_keeps_shared_text(
            &set.durable_store(),
            &*set.session_store_factory(),
        )
        .await
        .unwrap_or_else(|broken| panic!("{broken}"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn memory() {
        run(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn file() {
        run(true).await;
    }
}

/// FIG-5958: an owner commit that lost its fence (a stale epoch) shares a
/// write batch with a live owner's commit. Only the stale commit is
/// refused, with `OwnershipLost`, by undoing its own savepoint: the live
/// commit lands in the same transaction, which no rollback ends, and the
/// lost actor's state does not move.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_fence_in_a_write_batch_refuses_only_its_own_commit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use lash_durable::{
        ActorKey, CommitLabel, DurableError, DurableStore, FormatSet, MailKind, MailTx, NodeId,
        NodeSpec, Release,
    };

    let root = tempfile::tempdir().expect("store root");
    let pauses = crate::testing::SqlitePauses::default();
    let set = SqliteStoreSet::open_with_options_and_clock(
        root.path().join("lash.db"),
        crate::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..crate::SqliteStoreSetOptions::standard(crate::SqliteSynchronous::Full)
        },
        Arc::new(TestClock::new(1_000_000)),
    )
    .await
    .expect("open the store set");
    let store = set.durable_store();
    let label = CommitLabel::new("law.batch");
    let formats = FormatSet::new("batch-fence-formats");
    let live = ActorKey::session("batch-live").expect("a session actor key");
    let lost = ActorKey::session("batch-lost").expect("a session actor key");
    let mut create = MailTx::new();
    create.create_actor(live.clone(), formats.clone());
    create.create_actor(lost.clone(), formats.clone());
    store
        .commit_mail(create, label)
        .await
        .expect("create the actors");
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("batch-owner"),
            decodes: vec![formats],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register the owner");
    let claimed = store.claim(&node, 2).await.expect("claim both actors");
    let epoch = |actor: &ActorKey| {
        claimed
            .iter()
            .find(|claim| &claim.actor == actor)
            .map(|claim| claim.epoch)
            .expect("the actor was claimed")
    };
    let mut stale = store
        .begin(&lost, epoch(&lost))
        .await
        .expect("open the soon-stale owner");
    // OWNER-REFUSAL-ORDER: request preparation may find malformed content,
    // but the actor fence still refuses first when this attempt is stale.
    stale.write(lash_durable::domain::DomainWrite::SessionCommit(
        lash_durable::domain::SessionCommitWrite {
            session: lash_protocol_types::SessionId::parse("batch-lost").unwrap(),
            expected_head: 0,
            commit_json: "malformed session commit".to_owned(),
            execution: None,
        },
    ));
    let mut give_up = store
        .begin(&lost, epoch(&lost))
        .await
        .expect("open the owner");
    give_up.give_up(Release::Idle);
    store
        .commit(give_up, label)
        .await
        .expect("release the actor");
    let mut wake = MailTx::new();
    wake.append(lost.clone(), MailKind::new("law.note"), "wake");
    store
        .commit_mail(wake, label)
        .await
        .expect("wake the actor");
    let reclaimed = store.claim(&node, 1).await.expect("claim it again");
    assert_eq!(
        reclaimed.len(),
        1,
        "the woken actor is claimed under a new epoch"
    );
    let current = store
        .begin(&live, epoch(&live))
        .await
        .expect("open the live owner");
    let revision = |actor: &ActorKey| {
        let store = &store;
        let actor = actor.clone();
        async move {
            store
                .actor(&actor)
                .await
                .expect("read the actor")
                .expect("the actor exists")
                .revision
        }
    };
    let (live_before, lost_before) = (revision(&live).await, revision(&lost).await);

    let commits = Arc::new(AtomicUsize::new(0));
    let rollbacks = Arc::new(AtomicUsize::new(0));
    let (counted_commits, counted_rollbacks) = (Arc::clone(&commits), Arc::clone(&rollbacks));
    set.process_env_store()
        .conn
        .call(move |c| {
            c.commit_hook(Some(move || {
                counted_commits.fetch_add(1, Ordering::SeqCst);
                false
            }));
            c.rollback_hook(Some(move || {
                counted_rollbacks.fetch_add(1, Ordering::SeqCst);
            }));
            Ok(())
        })
        .await
        .expect("count the writer's transaction ends");

    // Hold the writer after a heartbeat's fence, so both owner commits
    // queue behind it and form the next batch, the stale one first.
    let pause = pauses.pause_after_fence();
    let holding = tokio::spawn({
        let store = set.durable_store();
        let node = node.clone();
        async move { store.heartbeat(&node).await }
    });
    pause.wait_until_reached().await;
    let mut refused = Box::pin(store.commit(stale, label));
    let mut landed = Box::pin(store.commit(current, label));
    std::future::poll_fn(|cx| {
        use std::future::Future;
        assert!(refused.as_mut().poll(cx).is_pending());
        assert!(landed.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    pause.release();
    holding
        .await
        .expect("heartbeat task")
        .expect("the held heartbeat commits");
    let refused = refused.await;
    assert!(
        matches!(refused, Err(DurableError::OwnershipLost(_))),
        "the stale commit answered {refused:?}"
    );
    landed
        .await
        .expect("the live commit lands beside the refusal");
    assert_eq!(
        rollbacks.load(Ordering::SeqCst),
        0,
        "the refusal ended the shared transaction instead of its own savepoint"
    );
    assert_eq!(
        commits.load(Ordering::SeqCst),
        2,
        "the heartbeat and the shared batch commit once each"
    );
    assert!(
        revision(&live).await > live_before,
        "the live commit moved its actor"
    );
    assert_eq!(
        revision(&lost).await,
        lost_before,
        "the stale commit moved nothing"
    );
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

    let (_root, set, _clock) = world(false).await;
    let store = set.durable_store();
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
    for ordinal in 1..=128u32 {
        if ordinal == 65 {
            messages[0] =
                "{\"role\":\"user\",\"content\":\"changed prefix: 🦀\\\"\\\\\"}".to_owned();
        }
        messages.push(serde_json::json!({
            "blocks": [{"ToolResult": {
                "call_id": format!("call-{ordinal}"),
                "tool_name": "fixture_lookup",
                "content": [{"type": "text", "text": format!(
                    "{{\n  \"key\": \"item-{ordinal}\",\n  \"tags\": [\"replay\", \"fixture\"],\n  \"record\": \"{}\"\n}}",
                    "x".repeat(2048)
                )}]
            }}],
            "role": "User"
        }).to_string());
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
            lash_core_ids::operational_metrics::sql::collect("model.start", "sqlite", async {
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
            .map(|(_, shape)| shape.rows_returned)
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
        if ordinal == 1 || ordinal == 128 {
            eprintln!(
                "PREFIX-DELTA SQL call {ordinal}: {}",
                serde_json::to_string(&receipt).unwrap()
            );
        }
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
        let reopened = set.reopen().await.unwrap().durable_store();
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
        "PREFIX-DELTA: 128 sequential calls, max new nodes={maximum_nodes}, max new edges={maximum_edges}, byte-exact after every settlement and changed prefix"
    );
}

/// PROMPT-RECLAIM-WORK: releasing an index deletes its outgoing edges once
/// per level, rather than once per node, and reads none: each reclaimed text
/// answers its own children (FIG-6939). A subtree another call roots stays
/// stored until that call releases it.
#[tokio::test]
async fn prompt_reclamation_reads_one_edge_batch_per_level() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptText, PromptWrite};
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore};

    let (_root, set, _clock) = world(false).await;
    let store = set.durable_store();
    let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
    let actor = ActorKey::session("sql-work-owner").unwrap();
    let session = lash_protocol_types::SessionId::from("sql-work-owner");
    let mut texts: Vec<_> = (0..16)
        .map(|leaf| PromptText {
            hash: BlobRef::for_content(format!("leaf-{leaf}").as_bytes()).0,
            text: format!("leaf {leaf}").into(),
            children: Vec::new(),
        })
        .collect();
    let mut frontier: Vec<_> = texts.iter().map(|text| text.hash.clone()).collect();
    let mut level = 0;
    while frontier.len() > 1 {
        let mut next = Vec::new();
        for (index, pair) in frontier.as_chunks::<2>().0.iter().enumerate() {
            let hash = BlobRef::for_content(format!("branch-{level}-{index}").as_bytes()).0;
            texts.push(PromptText {
                hash: hash.clone(),
                text: format!("branch {level} {index}").into(),
                children: pair.to_vec(),
            });
            next.push(hash);
        }
        frontier = next;
        level += 1;
    }
    let root = texts.last().unwrap();
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
    for (run, request_texts) in [("whole", texts.clone()), ("held", vec![held])] {
        tx.write(DomainWrite::Prompt(PromptWrite::Record {
            call: call(run),
            snapshot: None,
            request: run.to_owned(),
            snapshot_texts: Vec::new(),
            request_texts: request_texts.into(),
            prefix: None,
        }));
    }
    store
        .commit(tx, CommitLabel::new("model.start"))
        .await
        .unwrap();
    let hashes: Vec<_> = texts.iter().map(|text| text.hash.clone()).collect();
    for (run, levels) in [("whole", 4), ("held", 3)] {
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
            "sqlite",
            store.commit(tx, CommitLabel::new("prompt.release")),
        )
        .await;
        answer.unwrap();
        // A level's reclaimed texts answer their own children; their
        // edges go in one statement per level above the leaves.
        let edge_batches: u64 = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| sql.starts_with("delete from main.prompt_text_edges"))
            .map(|(_, shape)| shape.statements)
            .sum();
        eprintln!(
            "PROMPT-RECLAIM SQL {run}: {}",
            serde_json::to_string(&receipt).unwrap()
        );
        assert_eq!(
            edge_batches, levels,
            "edge deletes must follow levels, not nodes"
        );
        assert!(
            !receipt
                .shapes
                .keys()
                .any(|sql| sql.starts_with("select parent") || sql.starts_with("select child")),
            "no level reads its edges"
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
}

/// PROMPT-READ-WORK: an admitted request reads its text and index metadata
/// in one statement, retaining caller order, duplicate hashes and missing-hash
/// omission.
#[tokio::test]
async fn prompt_text_reads_are_batched_in_request_order() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptText, PromptWrite};
    use lash_durable::{ActorKey, CommitLabel, DomainWrite, DurableReads, DurableStore, Epoch};

    let (_root, set, _clock) = world(false).await;
    let store = set.durable_store();
    let identity = lash_core_execution::testing::sql_work::owner_commit(&set).await;
    let actor = ActorKey::session("sql-work-owner").unwrap();
    let mut texts: Vec<_> = (0..16)
        .map(|index| PromptText {
            hash: BlobRef::for_content(format!("text-{index}").as_bytes()).0,
            text: format!("text {index}").into(),
            children: Vec::new(),
        })
        .collect();
    texts.push(PromptText {
        hash: BlobRef::for_content(b"branch").0,
        text: "branch".to_owned().into(),
        children: [b"text-0", b"text-1"]
            .map(|text| BlobRef::for_content(text).0)
            .to_vec(),
    });
    let mut tx = store.begin(&actor, Epoch(identity.epoch)).await.unwrap();
    tx.write(DomainWrite::Prompt(PromptWrite::Record {
        call: PromptCallKey {
            session: lash_protocol_types::SessionId::from("sql-work-owner"),
            call: ModelCallId::Turn {
                run: lash_protocol_types::TurnId::from("batch-read"),
                ordinal: 1,
            },
        },
        snapshot: None,
        request: "batch read".to_owned(),
        snapshot_texts: Vec::new(),
        request_texts: texts.clone().into(),
        prefix: None,
    }));
    store
        .commit(tx, CommitLabel::new("model.start"))
        .await
        .unwrap();
    let mut hashes: Vec<_> = texts.iter().rev().map(|text| text.hash.clone()).collect();
    hashes.insert(2, "missing".to_owned());
    hashes.push(BlobRef::for_content(b"branch").0);
    let expected: Vec<_> = hashes
        .iter()
        .filter_map(|hash| texts.iter().find(|text| text.hash == *hash).cloned())
        .collect();
    let (answer, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "prompt.texts",
        "sqlite",
        store.prompt_texts(&hashes),
    )
    .await;
    assert_eq!(answer.unwrap(), expected);
    let reads: u64 = receipt
        .shapes
        .iter()
        .filter(|(sql, _)| {
            sql.starts_with("select ")
                && (sql.contains("prompt_texts") || sql.contains("prompt_text_edges"))
        })
        .map(|(_, shape)| shape.statements)
        .sum();
    eprintln!(
        "PROMPT-READ SQL {}",
        serde_json::to_string(&receipt).unwrap()
    );
    assert_eq!(reads, 1, "text and edge reads must share their snapshot");
}

law!(
    a_retired_owner_claims_only_its_registry_callbacks,
    |store, _advance| { laws::a_retired_owner_claims_only_its_registry_callbacks(store) }
);

law!(
    a_turn_counts_its_tool_admissions_atomically,
    |store, _advance| { laws::a_turn_counts_its_tool_admissions_atomically(store) }
);

law!(
    lo_fence_rejects_stale_epoch_and_revision,
    |store, advance| {
        async {
            laws::lo_fence_rejects_stale_epoch_and_revision(store, advance).await?;
            let nodes = store
                .connection_for_testing()
                .call(|conn| {
                    conn.query_row("SELECT count(*) FROM blobs WHERE encoding = 1", [], |row| {
                        row.get::<_, i64>(0)
                    })
                })
                .await
                .unwrap();
            assert_eq!(nodes, 0, "a stale draft left no graph blobs");
            Ok::<(), laws::LawBroken>(())
        }
    }
);

/// LO-IDLE-WRITER: the writer transaction orders a direct head change with
/// a claim, even when no unfinished run or command owns the idle head.
mod lo_idle_writer_orders_direct_mutation_and_claim {
    use super::*;
    use lash_core_execution::testing::store_fixtures::root_session_request;
    use lash_core_store::session_state::RuntimeSessionState;
    use lash_core_store::store::{RuntimeCommit, SessionCatalogStore, SessionCommitStore};
    use lash_durable::{ActorKey, DurableStore, FormatSet, MailTx, NodeId, NodeSpec};
    use lash_protocol_types::SessionId;

    async fn run(file: bool) {
        let (_root, set, _clock) = world(file).await;
        let catalog = set.session_store_factory();
        let durable = set.durable_store();
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
            stores: Arc::new(set.clone()),
            settings: lash_core_execution::runtime::actor::wait_laws::settings(),
            engines: Vec::new(),
            providers: Arc::new(lash_execution_contracts::projection::NoProjectionProviders),
        })
        .unwrap();
        let dirty = SessionId::fixture("lo-dirty-flush");
        let policy = standard_test_policy();
        let factory = backend.session_store_factory();
        let mut request = root_session_request(&dirty);
        request.config =
            lash_core_store::session_graph::PersistedSessionConfig::from_policy(&policy);
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

    #[tokio::test]
    async fn memory() {
        Box::pin(run(false)).await;
    }
    #[tokio::test]
    async fn file() {
        Box::pin(run(true)).await;
    }
}

/// LO-IDLE-WRITER: public deletion and operator redrive never borrow a
/// claimed actor's authority. Artifacts deletion is part of its own commit.
mod lo_idle_writer_keeps_close_and_redrive_under_authority {
    use super::*;
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

    async fn run(file: bool) {
        let (_root, set, _clock) = world(file).await;
        let catalog = set.session_store_factory();
        let durable = set.durable_store();
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
    #[tokio::test]
    async fn memory() {
        Box::pin(run(false)).await;
    }
    #[tokio::test]
    async fn file() {
        Box::pin(run(true)).await;
    }
}

law!(
    lo_fence_preserves_a_composed_drafts_basis,
    |store, _advance| { laws::lo_fence_preserves_a_composed_drafts_basis(store) }
);
