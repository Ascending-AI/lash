//! The durability engine's fencing laws (`lash_durable::laws`) over SQLite,
//! in memory and in a file.

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

/// BYTES (FIG-5946): a `model.start` binds the bodies of only the texts
/// the store does not already hold. A session's second call, admitted while
/// the first is open, binds only its changed section text and its own body
/// tail: the unchanged section text, the shared body prefix and the
/// response contract are held. Once the turn's `model.done` settled both
/// requests, the next call binds its body chunks and contract again, but
/// not the section texts the settled calls' snapshots still hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_model_start_binds_only_the_text_bodies_the_store_lacks() {
    use lash_durable::domain::{ModelCallId, PromptCallKey, PromptText, PromptWrite};
    use lash_durable::{
        ActorKey, CommitLabel, DomainWrite, DurableStore, FormatSet, MailTx, NodeId, NodeSpec,
    };

    let (_root, set, _clock) = world(false).await;
    let store = set.durable_store();
    let formats = FormatSet::new("prompt-bytes-formats");
    let actor = ActorKey::session("prompt-bytes").expect("a session actor key");
    let mut create = MailTx::new();
    create.create_actor(actor.clone(), formats.clone());
    store
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("create the session actor");
    let node = store
        .register_node(&NodeSpec {
            node: NodeId::new("prompt-bytes-owner"),
            decodes: vec![formats],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .expect("register the owner");
    let epoch = store.claim(&node, 1).await.expect("claim")[0].epoch;
    let run = lash_protocol_types::TurnId::from("prompt-bytes-turn");
    let text = |name: &str| PromptText {
        hash: format!("hash-{name}"),
        text: format!("text {name}").into(),
        children: Vec::new(),
    };
    let record = |ordinal: u32, snapshot: &[&str], request: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            call: PromptCallKey {
                session: lash_protocol_types::SessionId::from("prompt-bytes"),
                call: ModelCallId::Turn {
                    run: run.clone(),
                    ordinal,
                },
            },
            snapshot: Some(format!("snapshot of call {ordinal}")),
            request: format!("request of call {ordinal}"),
            snapshot_texts: snapshot.iter().map(|name| text(name)).collect(),
            request_texts: request
                .iter()
                .map(|name| text(name))
                .collect::<Vec<_>>()
                .into(),
            prefix: None,
        })
    };
    let settle = |open: Option<u32>| {
        DomainWrite::Prompt(PromptWrite::Settle {
            session: lash_protocol_types::SessionId::from("prompt-bytes"),
            run: run.clone(),
            open,
        })
    };
    // Measure every INSERT binding, including bodies submitted again that
    // INSERT would ignore. SQLite reports SQL literal bytes: each fixture
    // pair ["hash-N", "text N"] costs 17 + 2*len(N), plus array commas,
    // brackets and the SQL string's quotes. The expected names below are
    // the fixture's missing bodies, independent of RETURNING rows.
    let bound = |writes: Vec<DomainWrite>, label: &'static str| {
        let (store, actor) = (&store, &actor);
        async move {
            let ((), receipt) =
                lash_core_ids::operational_metrics::sql::collect(label, "sqlite", async {
                    let mut tx = store.begin(actor, epoch).await.expect("begin");
                    for write in writes {
                        tx.write(write);
                    }
                    store
                        .commit(tx, CommitLabel::new(label))
                        .await
                        .expect("commit");
                })
                .await;
            assert_eq!(
                receipt.unmeasured_bound_statements, 0,
                "every binding is measured"
            );
            receipt
                .shapes
                .iter()
                .filter(|(shape, _)| {
                    shape.starts_with("insert into") && shape.contains("prompt_texts ")
                })
                .map(|(_, shape)| shape.bound_bytes)
                .sum::<u64>()
        }
    };

    assert_eq!(
        bound(
            vec![record(
                1,
                &["intro", "memo-1"],
                &["prefix", "tail-1", "contract"]
            )],
            "model.start"
        )
        .await,
        155,
        "the first call binds every body"
    );
    assert_eq!(
        bound(
            vec![
                record(2, &["intro", "memo-2"], &["prefix", "tail-2", "contract"]),
                settle(Some(2)),
            ],
            "model.start"
        )
        .await,
        63,
        "the second call binds only its changed section text and its body tail"
    );
    assert_eq!(bound(vec![settle(None)], "model.done").await, 0);
    assert_eq!(
        bound(
            vec![record(
                97,
                &["intro", "memo-2"],
                &["prefix", "tail-3", "contract"]
            )],
            "model.start"
        )
        .await,
        97,
        "after the settlement the next call binds its body chunks and contract, \
         not the section texts the snapshots hold"
    );
    let names = ["intro", "memo-1", "memo-2", "prefix", "tail-3", "contract"];
    let hashes: Vec<String> = names.iter().map(|name| text(name).hash).collect();
    assert_eq!(
        lash_durable::DurableReads::prompt_texts(&store, &hashes)
            .await
            .expect("read the texts"),
        names.iter().map(|name| text(name)).collect::<Vec<_>>(),
        "every held text reads back"
    );
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
    let stale = store
        .begin(&lost, epoch(&lost))
        .await
        .expect("open the soon-stale owner");
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

/// PROMPT-RECLAIM-WORK: releasing an index reads its outgoing edges once
/// per level, rather than once per node. A subtree another call roots stays
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
            hash: format!("leaf-{leaf}"),
            text: format!("leaf {leaf}").into(),
            children: Vec::new(),
        })
        .collect();
    let mut frontier: Vec<_> = texts.iter().map(|text| text.hash.clone()).collect();
    let mut level = 0;
    while frontier.len() > 1 {
        let mut next = Vec::new();
        for (index, pair) in frontier.as_chunks::<2>().0.iter().enumerate() {
            let hash = format!("branch-{level}-{index}");
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
    for (run, levels) in [("whole", 5), ("held", 4)] {
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
        let edge_reads: u64 = receipt
            .shapes
            .iter()
            .filter(|(sql, _)| {
                sql.starts_with("select child from") || sql.starts_with("select parent, child from")
            })
            .map(|(_, shape)| shape.statements)
            .sum();
        eprintln!(
            "PROMPT-RECLAIM SQL {run}: {}",
            serde_json::to_string(&receipt).unwrap()
        );
        assert_eq!(
            edge_reads, levels,
            "edge reads must follow levels, not nodes"
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
            hash: format!("text-{index}"),
            text: format!("text {index}").into(),
            children: Vec::new(),
        })
        .collect();
    texts.push(PromptText {
        hash: "branch".to_owned(),
        text: "branch".to_owned().into(),
        children: vec!["text-0".to_owned(), "text-1".to_owned()],
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
    hashes.push("branch".to_owned());
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
