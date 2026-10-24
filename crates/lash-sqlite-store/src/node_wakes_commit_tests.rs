//! Ready hints are selected by the actual producer transaction, before COMMIT.

use super::*;
use lash_durable::laws::node_wakes::{Claims, CountingStore};

/// FOLLOW-FACT-CHANGES: an owner revision that changes none of a
/// follower's facts needs no new snapshot; a park still wakes its readers.
#[tokio::test]
async fn unchanged_follow_facts_do_not_publish_another_read_hint() {
    let tier = SqliteTier::open().await;
    let store = tier.stores.durable_store();
    let key = actor("unchanged-follow");
    create(&store, &key).await;
    let node = store
        .register_node(&lash_durable::NodeSpec {
            node: NodeId::new("follow-owner"),
            decodes: vec![formats()],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = store.claim(&node, 1).await.unwrap().pop().unwrap();
    let clear = || {
        store.conn.write(|tx| {
            tx.execute("DELETE FROM node_wakes", [])?;
            Ok(())
        })
    };
    clear().await.unwrap();
    let tx = store.begin(&key, claimed.epoch).await.unwrap();
    store.commit(tx, CommitLabel::MODEL_START).await.unwrap();
    let hints = || {
        store.conn.read(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM node_wakes WHERE node_id IS NULL",
                [],
                |row| row.get::<_, u64>(0),
            )
        })
    };
    assert_eq!(
        hints().await.unwrap(),
        0,
        "an unchanged follow snapshot is not hinted again"
    );
    let mut tx = store.begin(&key, claimed.epoch).await.unwrap();
    tx.give_up(lash_durable::Release::Idle);
    store
        .commit(tx, CommitLabel::SESSION_RELEASE)
        .await
        .unwrap();
    assert_eq!(
        hints().await.unwrap(),
        0,
        "an idle release leaves follow facts unchanged"
    );
    let mut wake = MailTx::new();
    wake.wake(key.clone());
    store
        .commit_mail(wake, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let claimed = store.claim(&node, 1).await.unwrap().pop().unwrap();
    let mut tx = store.begin(&key, claimed.epoch).await.unwrap();
    tx.write(lash_durable::DomainWrite::ActorPark {
        reason_json: Some("{}".to_owned()),
    });
    tx.give_up(lash_durable::Release::Parked);
    store.commit(tx, CommitLabel::MODEL_START).await.unwrap();
    assert_eq!(
        hints().await.unwrap(),
        1,
        "the park's changed facts must wake readers"
    );
}

/// WAKE-FINAL-FACTS: routing uses the member's final owner, including an
/// owner change after the mail write, without capture-table reads or writes.
#[tokio::test]
async fn wake_capture_carries_final_facts_without_temporary_rows() {
    let tier = SqliteTier::open().await;
    let store = tier.stores.durable_store();
    let hot = actor("capture-final-owner");
    create(&store, &hot).await;
    let key = hot.clone();
    store
        .conn
        .write(move |tx| {
            tx.execute("DELETE FROM node_wakes", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let (changed, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "law.final_wake",
        "sqlite",
        store.conn.write(move |tx| {
            let before = tx.total_changes();
            tx.execute(crate::durable::NODE_WAKE_ROLLBACK_MAIL, [key.as_str()])?;
            tx.execute(crate::durable::NODE_WAKE_FINAL_OWNER, [key.as_str()])?;
            Ok(tx.total_changes() - before)
        }),
    )
    .await;
    assert_eq!(
        changed.unwrap(),
        2,
        "only the two actor writes changed rows"
    );
    assert!(
        receipt.shapes.keys().all(|sql| !sql.contains("temp.lash_")),
        "capture neither reads nor writes temporary rows: {receipt:?}"
    );
    let rows = store
        .conn
        .read(|c| {
            c.prepare("SELECT node_id, actors FROM node_wakes ORDER BY seq")?
                .query_map([], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "one coalesced hint: {rows:?}");
    assert_eq!(rows[0].0.as_deref(), Some("final-owner"));
    assert_eq!(
        node_wake_payload::decode(&rows[0].1),
        Some(NodeWakeEvent::Owned(vec![hot]))
    );
}

/// WAKE-MEMBER-ISOLATION: a refused batch member donates no captured actors
/// to a later successful member of the same outer transaction.
#[tokio::test]
async fn rolled_back_capture_cannot_reach_a_committed_sibling() {
    use std::future::Future as _;
    let dir = tempfile::tempdir().unwrap();
    let pauses = crate::testing::SqlitePauses::default();
    let stores = SqliteStoreSet::open_with_options_and_clock(
        dir.path().join("lash.db"),
        crate::SqliteStoreSetOptions {
            pauses: Some(pauses.clone()),
            ..crate::SqliteStoreSetOptions::standard(crate::SqliteSynchronous::Full)
        },
        Arc::new(lash_core::testing::TestClock::new(STORE_TIME)),
    )
    .await
    .unwrap();
    let store = stores.durable_store();
    let hot = actor("capture-rolled-back");
    create(&store, &hot).await;
    store
        .conn
        .write(|tx| {
            tx.execute(crate::durable::NODE_WAKE_OWNED_SETUP, [])?;
            tx.execute("DELETE FROM node_wakes", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let hold = pauses.pause_after_fence();
    let holding = tokio::spawn({
        let conn = store.conn.clone();
        async move { conn.write(|_| Ok(())).await }
    });
    hold.wait_until_reached().await;
    let mut refused = Box::pin(store.conn.write_flow(move |tx| {
        tx.execute(crate::durable::NODE_WAKE_ROLLBACK_MAIL, [hot.as_str()])?;
        Ok(crate::conn::TxOutcome::Rollback(()))
    }));
    let mut sibling = Box::pin(store.conn.write(|tx| {
        tx.execute(crate::durable::NODE_WAKE_SESSION_REVISION, [])?;
        Ok(())
    }));
    std::future::poll_fn(|cx| {
        assert!(refused.as_mut().poll(cx).is_pending());
        assert!(sibling.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    hold.release();
    holding.await.unwrap().unwrap();
    refused.await.unwrap();
    sibling.await.unwrap();
    let rows = store
        .conn
        .read(|c| {
            c.prepare("SELECT node_id, actors FROM node_wakes ORDER BY seq")?
                .query_map([], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only the sibling's appended hint: {rows:?}");
    assert_eq!(
        rows[0].0, None,
        "the refused mail must not produce an owned hint"
    );
    assert_eq!(
        node_wake_payload::decode(&rows[0].1),
        Some(NodeWakeEvent::Appended(vec![actor("capture-rolled-back")]))
    );
}

/// A node that claims every `poll` and holds `max_active` actors. Its
/// startup attempt gets the hang guard's bound, not production's: a
/// registration that misses its bound is retried after `claim_backoff`,
/// which a [`quiet`] node never reaches, and these laws assert no startup
/// latency.
fn holding(poll: Duration, max_active: usize) -> DurableSettings {
    let defaults = DurableSettings::default();
    DurableSettings {
        lease: LeaseSettings {
            claim_poll: poll,
            claim_backoff: poll,
            startup: WATCHDOG,
            ..defaults.lease
        },
        max_active,
        claim_batch: defaults.claim_batch.min(max_active),
        ..defaults
    }
}

fn quiet() -> Duration {
    Duration::from_secs(100 * 365 * 24 * 60 * 60)
}

fn serving(
    stores: &SqliteStoreSet,
    name: &str,
    settings: DurableSettings,
    claims: &Claims,
) -> (
    lash_durable::laws::node_wakes::LawNode,
    lash_durable::runner::WakeAttachment,
    mpsc::UnboundedReceiver<WakeEvidence>,
) {
    let (send, evidence) = mpsc::unbounded_channel();
    let store = CountingStore {
        inner: Arc::new(stores.durable_store()),
        node: name.to_owned(),
        claims: claims.clone(),
    };
    let node = serve_node(
        Arc::new(store),
        Some(Arc::new(ObservedWakes {
            inner: stores.node_wakes().unwrap(),
            evidence: send,
            drop_owned: false,
            report_owned: false,
        })),
        name,
        settings,
    );
    let attachment = stores.wake_sink().attach(&node.hints);
    (node, attachment, evidence)
}

async fn create(store: &SqliteDurableStore, actor: &ActorKey) -> lash_durable::MailCommit {
    let mut tx = MailTx::new();
    tx.create_actor(actor.clone(), formats());
    store
        .commit_mail(tx, CommitLabel::MAIL_SESSION)
        .await
        .unwrap()
}

async fn claimed(claims: &Claims, count: usize) {
    tokio::time::timeout(WATCHDOG, claims.wait_for(count))
        .await
        .expect("the expected claim attempts completed");
}

async fn probed(claims: &Claims, node: &str) {
    tokio::time::timeout(WATCHDOG, claims.wait_until_probed(node))
        .await
        .expect("the node made its first readiness read");
}

async fn listened(evidence: &mut mpsc::UnboundedReceiver<WakeEvidence>) -> NodeLease {
    let WakeEvidence::Listening(lease) = evidence_until(evidence, |event| {
        matches!(event, WakeEvidence::Listening(_))
    })
    .await
    else {
        unreachable!("the predicate selects a listening lease");
    };
    *lease
}

pub(super) async fn one_claim_per_readied_actor() {
    const PEERS: usize = 15;
    const READIED: usize = 8;
    let dir = tempfile::tempdir().unwrap();
    let stores = SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Full)
        .await
        .unwrap();
    let store = stores.durable_store();
    create(&store, &actor("held")).await;
    let claims = Claims::default();
    let (producer, _attachment, mut evidence) = serving(
        &stores,
        "producer",
        holding(Duration::from_millis(500), 1),
        &claims,
    );
    listened(&mut evidence).await;
    claimed(&claims, 1).await;
    assert_eq!(claims.take(), vec![("producer".to_owned(), 1)]);
    let mut peers = Vec::new();
    let mut leases = Vec::new();
    for index in 0..PEERS {
        let peer_stores = stores.reopen().await.unwrap();
        let (peer, attachment, mut evidence) = serving(
            &peer_stores,
            &format!("peer-{index:02}"),
            holding(quiet(), 256),
            &claims,
        );
        leases.push(listened(&mut evidence).await);
        probed(&claims, &format!("peer-{index:02}")).await;
        peers.push((peer, attachment, peer_stores));
    }
    assert_eq!(
        claims.take(),
        Vec::new(),
        "no peer claims with nothing ready"
    );
    evidence_until(&mut evidence, |event| {
        matches!(event, WakeEvidence::Watched(boots) if boots.iter().filter(|boot| boot.held).count() == PEERS + 1)
    })
    .await;
    // The probe reports what it saw before it returns, and the runner takes
    // that view as its peers only afterwards. Its next probe starts once the
    // view is in place, so the first readied actor's commit picks a peer.
    evidence_until(&mut evidence, |event| {
        matches!(event, WakeEvidence::Watched(_))
    })
    .await;
    for index in 0..READIED {
        let readied = actor(&format!("readied-{index}"));
        let commit = create(&store, &readied).await;
        producer.hints.woke(&commit);
        claimed(&claims, index + 1).await;
        let owner = store.actor(&readied).await.unwrap().unwrap().owner.unwrap();
        assert!(owner.node.as_str().starts_with("peer-"));
        // No unhinted contender can take the same ready actor. This also
        // makes the exclusivity mutant observable without broadcasting hints.
        let contender = leases.iter().find(|lease| lease.owner != owner).unwrap();
        assert!(store.claim(contender, 1).await.unwrap().is_empty());
    }
    producer.kill().await;
    for (peer, _, _) in peers {
        peer.kill().await;
    }
    let attempts = claims.take();
    assert_eq!(
        attempts.len(),
        READIED,
        "one hint causes one claim: {attempts:?}"
    );
    assert!(
        attempts.iter().all(|(_, taken)| *taken == 1),
        "no empty claim: {attempts:?}"
    );
}

pub(super) async fn lost_hint_reaches_the_claim_poll() {
    let dir = tempfile::tempdir().unwrap();
    let stores = SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Full)
        .await
        .unwrap();
    let store = stores.durable_store();
    create(&store, &actor("held")).await;
    // The doomed listener exists when the producer takes its initial peer
    // view, but never claims; only the producer can take the held actor.
    let doomed = store
        .register_node(&lash_durable::NodeSpec {
            node: NodeId::new("doomed"),
            decodes: vec![formats()],
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let doomed_feed = stores.node_wakes().unwrap().listen(&doomed).await.unwrap();
    let claims = Claims::default();
    let (producer, _attachment, mut evidence) =
        serving(&stores, "producer", holding(quiet(), 1), &claims);
    listened(&mut evidence).await;
    claimed(&claims, 1).await;
    evidence_until(&mut evidence, |event| {
        matches!(event, WakeEvidence::Watched(boots) if boots.iter().any(|boot| boot.boot == doomed.owner && boot.held))
    })
    .await;
    claims.take();
    let survivor_stores = stores.reopen().await.unwrap();
    let (survivor, _survivor_attachment, mut survivor_evidence) = serving(
        &survivor_stores,
        "survivor",
        holding(Duration::from_millis(500), 256),
        &claims,
    );
    listened(&mut survivor_evidence).await;
    probed(&claims, "survivor").await;
    assert_eq!(
        claims.take(),
        Vec::new(),
        "the survivor claims with nothing ready"
    );
    drop(doomed_feed); // releases its liveness lock; the producer's peer view is unchanged
    let readied = actor("readied");
    let commit = create(&store, &readied).await;
    // The transaction selected the producer's stale peer, not the new
    // survivor: its fallback poll is the only delivery path for this actor.
    let hints = store
        .conn
        .read(|c| {
            c.prepare("SELECT node_id FROM node_wakes WHERE actors = ?1")?
                .query_map([node_wake_payload::READY], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    assert_eq!(hints, vec!["doomed".to_owned()]);
    producer.hints.woke(&commit);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if store
                .actor(&readied)
                .await
                .unwrap()
                .unwrap()
                .owner
                .as_ref()
                .is_some_and(|owner| owner.node.as_str() == "survivor")
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the survivor's claim poll takes the actor whose hint was lost");
    producer.kill().await;
    survivor.kill().await;
}

pub(super) async fn lost_owned_hint_reaches_the_mail_poll() {
    let dir = tempfile::tempdir().unwrap();
    let stores = SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Full)
        .await
        .unwrap();
    let store = stores.durable_store();
    let hot = actor("hot");
    create(&store, &hot).await;
    let (send, mut evidence) = mpsc::unbounded_channel();
    let mut owner = serve_node(
        Arc::new(store.clone()),
        Some(Arc::new(ObservedWakes {
            inner: stores.node_wakes().unwrap(),
            evidence: send,
            drop_owned: true,
            report_owned: false,
        })),
        "owner",
        holding(Duration::from_millis(500), 256),
    );
    tokio::time::timeout(WATCHDOG, owner.waiting())
        .await
        .unwrap()
        .unwrap();
    let mut tx = MailTx::new();
    tx.append(hot.clone(), MailKind::new("law.note"), "note");
    store
        .commit_mail(tx, CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    // Drop the actual atomic envelope, rather than omitting the old
    // after-commit publish call, which no longer loses a same-file hint.
    evidence_until(
        &mut evidence,
        |event| matches!(event, WakeEvidence::Dropped(actors) if actors.contains(&hot)),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), owner.arrived())
        .await
        .expect("mail poll finds the mail whose atomic hint was dropped")
        .unwrap();
    owner.kill().await;
}

/// WAKE-MAINTENANCE (FIG-7083): producer commits retain durable hints
/// without running an expiry sweep. Liveness maintenance bounds each
/// sweep and preserves every hint still inside its retention window.
#[tokio::test]
async fn producers_leave_wake_expiry_to_bounded_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(lash_core::testing::TestClock::new(STORE_TIME));
    let stores = SqliteStoreSet::open_with_options_and_clock(
        dir.path().join("lash.db"),
        crate::SqliteStoreSetOptions::standard(crate::SqliteSynchronous::Normal),
        clock.clone(),
    )
    .await
    .unwrap();
    let store = stores.durable_store();
    let (_, receipt) = lash_core_ids::operational_metrics::sql::collect(
        "law.producer_wake",
        "sqlite",
        create(&store, &actor("maintenance")),
    )
    .await;
    assert!(
        receipt
            .shapes
            .keys()
            .all(|sql| !sql.starts_with("delete from node_wakes")),
        "a producer must not sweep disposable hints: {receipt:?}"
    );
    store.conn.write(|tx| {
        tx.execute("INSERT INTO node_wakes (node_id, actors, published_at_ms) VALUES (NULL, 'young', ?1)", [i64::try_from(STORE_TIME).unwrap()])?;
        for _ in 0..4200 {
            tx.execute("INSERT INTO node_wakes (node_id, actors, published_at_ms) VALUES (NULL, 'old', 0)", [])?;
        }
        Ok(())
    }).await.unwrap();
    let count = || {
        store.conn.read(|c| {
            c.query_row(
                "SELECT count(*) FROM node_wakes WHERE actors = 'old'",
                [],
                |row| row.get::<_, usize>(0),
            )
        })
    };
    stores.node_wakes().unwrap().liveness().await.unwrap();
    assert_eq!(
        count().await.unwrap(),
        105,
        "maintenance inspects at most 4096 hints, including the young one"
    );
    let young = store
        .conn
        .read(|c| {
            c.query_row(
                "SELECT count(*) FROM node_wakes WHERE published_at_ms = ?1",
                [i64::try_from(STORE_TIME).unwrap()],
                |row| row.get::<_, usize>(0),
            )
        })
        .await
        .unwrap();
    assert!(young > 0, "unexpired hints remain visible");
    clock.advance(11_000);
    stores.node_wakes().unwrap().liveness().await.unwrap();
    assert_eq!(count().await.unwrap(), 0);
}

/// WAKE-MAINTENANCE: an empty expiry pass must not commit a write. A
/// nonempty pass still commits deletion, even when no actor was reaped.
#[tokio::test]
async fn maintenance_commits_only_when_nodes_or_hints_expired() {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(lash_core::testing::TestClock::new(STORE_TIME));
    let stores = SqliteStoreSet::open_with_options_and_clock(
        dir.path().join("lash.db"),
        crate::SqliteStoreSetOptions::standard(crate::SqliteSynchronous::Normal),
        clock.clone(),
    )
    .await
    .unwrap();
    let store = stores.durable_store();
    let spec = |name, ttl_millis| lash_durable::NodeSpec {
        node: NodeId::new(name),
        decodes: vec![formats()],
        serves: Vec::new(),
        capabilities: Default::default(),
        ttl_millis,
    };
    let reaper = store.register_node(&spec("reaper", 15_000)).await.unwrap();
    let wakes = stores.node_wakes().unwrap();
    let commits = |receipt: &lash_core_ids::operational_metrics::sql::Receipt| {
        receipt.shapes.get("commit").map_or(0, |row| row.statements)
    };
    let (answer, empty) = lash_core_ids::operational_metrics::sql::collect(
        "law.empty_maintenance",
        "sqlite",
        async {
            wakes.liveness().await.unwrap();
            store.reap(&reaper).await
        },
    )
    .await;
    assert!(answer.unwrap().is_empty());
    assert_eq!(commits(&empty), 0, "empty expiry committed: {empty:?}");

    let expired_node = store.register_node(&spec("expired", 1)).await.unwrap();
    store.conn.write(|tx| {
        tx.execute("INSERT INTO node_wakes (node_id, actors, published_at_ms) VALUES (NULL, 'old', 0), (NULL, 'young', ?1)", [i64::try_from(STORE_TIME).unwrap()])?;
        Ok(())
    }).await.unwrap();
    clock.advance(2);
    let (answer, expired) = lash_core_ids::operational_metrics::sql::collect(
        "law.expired_maintenance",
        "sqlite",
        async {
            wakes.liveness().await.unwrap();
            store.reap(&reaper).await
        },
    )
    .await;
    assert!(
        answer.unwrap().is_empty(),
        "the expired node owned no actor"
    );
    assert_eq!(commits(&expired), 2, "each nonempty expiry commits once");
    assert_eq!(
        store.heartbeat(&expired_node).await.unwrap(),
        lash_durable::HeartbeatOutcome::Reaped,
        "the expired node was durably deleted"
    );
    store
        .conn
        .read(|tx| {
            let hints = tx
                .prepare("SELECT actors FROM node_wakes ORDER BY seq")?
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            assert_eq!(hints, ["young"]);
            Ok(())
        })
        .await
        .unwrap();
    let (answer, empty) = lash_core_ids::operational_metrics::sql::collect(
        "law.young_maintenance",
        "sqlite",
        async {
            wakes.liveness().await.unwrap();
            store.reap(&reaper).await
        },
    )
    .await;
    assert!(answer.unwrap().is_empty());
    assert_eq!(commits(&empty), 0, "unexpired hints need no write commit");
}
