//! The durability engine's fencing laws (`lash_durable::laws`) over SQLite,
//! in memory and in a file.

use std::sync::Arc;
use std::time::Duration;

use lash_core_execution::testing::TestClock;
use lash_durable::laws;

use crate::SqliteStoreSet;

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
            ttl_millis: 15_000,
        })
        .await
        .expect("register the owner");
    let epoch = store.claim(&node, 1).await.expect("claim")[0].epoch;
    let run = lash_sansio::TurnId::from("prompt-bytes-turn");
    let text = |name: &str| PromptText {
        hash: format!("hash-{name}"),
        text: format!("text {name}"),
    };
    let record = |ordinal: u32, snapshot: &[&str], request: &[&str]| {
        DomainWrite::Prompt(PromptWrite::Record {
            call: PromptCallKey {
                session: lash_sansio::SessionId::from("prompt-bytes"),
                call: ModelCallId::Turn {
                    run: run.clone(),
                    ordinal,
                },
            },
            snapshot: Some(format!("snapshot of call {ordinal}")),
            request: format!("request of call {ordinal}"),
            snapshot_texts: snapshot.iter().map(|name| text(name)).collect(),
            request_texts: request.iter().map(|name| text(name)).collect(),
        })
    };
    let settle = |open: Option<u32>| {
        DomainWrite::Prompt(PromptWrite::Settle {
            session: lash_sansio::SessionId::from("prompt-bytes"),
            run: run.clone(),
            open,
        })
    };
    // The text bodies one commit bound: the rows its text insert stored,
    // each a bound pair, as the insert binds only unheld texts and skips
    // none.
    let bound = |writes: Vec<DomainWrite>, label: &'static str| {
        let (store, actor) = (&store, &actor);
        async move {
            let ((), receipt) =
                lash_core_execution::facade_support::sql::collect(label, "sqlite", async {
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
            receipt
                .shapes
                .iter()
                .filter(|(shape, _)| {
                    shape.starts_with("insert into") && shape.contains("prompt_texts ")
                })
                .map(|(_, shape)| shape.rows_returned)
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
        5,
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
        2,
        "the second call binds only its changed section text and its body tail"
    );
    assert_eq!(bound(vec![settle(None)], "model.done").await, 0);
    assert_eq!(
        bound(
            vec![record(
                3,
                &["intro", "memo-2"],
                &["prefix", "tail-3", "contract"]
            )],
            "model.start"
        )
        .await,
        3,
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
