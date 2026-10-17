//! Ready hints are selected by the actual producer transaction, before COMMIT.

use super::*;
use lash_durable::laws::node_wakes::{Claims, CountingStore};

/// The capture-buffer reads `receipt` counts.
fn capture_reads(receipt: &lash_core_ids::operational_metrics::sql::Receipt) -> u64 {
    receipt
        .shapes
        .iter()
        .filter(|(sql, _)| sql.starts_with("select ") && sql.contains("temp.lash_"))
        .map(|(_, shape)| shape.statements)
        .sum()
}

/// WAKE-CAPTURE-WORK: a producer that captured nothing reads no capture
/// buffer (FIG-6617), and one that captured a wake reads both buffers
/// together, in one statement.
#[tokio::test]
async fn a_wake_capture_is_read_only_when_a_trigger_captured() {
    let dir = tempfile::tempdir().unwrap();
    let stores = SqliteStoreSet::open(dir.path().join("lash.db"), crate::SqliteSynchronous::Full)
        .await
        .unwrap();
    let store = stores.durable_store();
    let lease = store
        .register_node(&lash_durable::NodeSpec {
            node: NodeId::new("capture-owner"),
            decodes: vec![formats()],
            serves: Vec::new(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let (answer, quiet) = lash_core_ids::operational_metrics::sql::collect(
        "node.heartbeat",
        "sqlite",
        store.heartbeat(&lease),
    )
    .await;
    answer.unwrap();
    eprintln!(
        "WAKE-CAPTURE SQL {}",
        serde_json::to_string(&quiet).unwrap()
    );
    assert_eq!(capture_reads(&quiet), 0, "nothing was captured");
    let actor = ActorKey::session("capture-woken").unwrap();
    let (_, woke) = lash_core_ids::operational_metrics::sql::collect(
        "mail.create",
        "sqlite",
        create(&store, &actor),
    )
    .await;
    eprintln!("WAKE-CAPTURE SQL {}", serde_json::to_string(&woke).unwrap());
    assert_eq!(
        capture_reads(&woke),
        1,
        "capture buffers must share a statement"
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

async fn listened(evidence: &mut mpsc::UnboundedReceiver<WakeEvidence>) -> NodeLease {
    let WakeEvidence::Listening(lease) = evidence_until(evidence, |event| {
        matches!(event, WakeEvidence::Listening(_))
    })
    .await
    else {
        unreachable!("the predicate selects a listening lease");
    };
    lease
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
        peers.push((peer, attachment, peer_stores));
    }
    claimed(&claims, PEERS).await;
    assert!(claims.take().iter().all(|(_, taken)| *taken == 0));
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
    claimed(&claims, 1).await;
    assert!(claims.take().iter().all(|(_, taken)| *taken == 0));
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
