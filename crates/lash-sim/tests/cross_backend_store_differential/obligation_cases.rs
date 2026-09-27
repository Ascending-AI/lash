//! Obligation ledgers and the recovery leader lease (ADR 0109 §1) compared
//! across the three backends: one scripted sequence of arm, claim, settle,
//! re-arm and listing calls on the `session_delete` ledger, and of acquire,
//! renew and resign calls on one lease, must read back identically.
//!
//! The Postgres database is shared with every other case and every earlier
//! run, so each backend's rows carry the run nonce and every read is filtered
//! to them; ids, tokens and database-clock instants are normalized away, and
//! the ledger's own instants come from the script, not a clock.

use std::num::NonZeroUsize;

use lash_core::StoreSet;
use lash_core::store::{
    HolderId, LeaseAnswer, LeaseClaim, LeaseName, ObligationId, ObligationKey, ObligationKind,
    ObligationSettlement, StallReason,
};

use super::*;

const T0: u64 = 1_000_000;

/// One backend's answers to the script, with ids replaced by their aliases.
type Transcript = Vec<String>;

fn page() -> NonZeroUsize {
    NonZeroUsize::new(10_000).unwrap_or(NonZeroUsize::MIN)
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open or answer panics the harness with its name by design"
)]
async fn ledger_transcript(stores: &dyn StoreSet, prefix: &str) -> Transcript {
    let mut out = Transcript::new();
    let ledger = stores.obligation_ledger(ObligationKind::SessionDelete);
    let factory = stores.session_store_factory();
    let aliases = ["a", "b", "c"];
    let mut ids = BTreeMap::<&str, ObligationId>::new();
    let alias_of = |ids: &BTreeMap<&str, ObligationId>, id: &ObligationId| {
        ids.iter()
            .find(|(_, known)| *known == id)
            .map(|(alias, _)| (*alias).to_owned())
    };
    for alias in aliases {
        let session_id = SessionId::from(format!("{prefix}-obligation-{alias}"));
        factory
            .create_store(&SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: SessionRelation::Root,
                policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
            })
            .await
            .expect("create the session whose catalog row carries the obligation");
        let key = ObligationKey::SessionDelete { session_id };
        let id = ledger.arm(&key, T0).await.expect("arm");
        out.push(format!("arm {alias} -> {}", id.is_some()));
        ids.insert(alias, id.expect("a fresh row arms"));
    }
    let rearmed = ledger
        .arm(
            &ObligationKey::SessionDelete {
                session_id: SessionId::from(format!("{prefix}-obligation-a")),
            },
            T0,
        )
        .await
        .expect("arm an armed row");
    out.push(format!("arm a again -> {rearmed:?}"));
    let missing = ledger
        .arm(
            &ObligationKey::SessionDelete {
                session_id: SessionId::from(format!("{prefix}-obligation-missing")),
            },
            T0,
        )
        .await
        .expect("arm a missing row");
    out.push(format!("arm missing -> {missing:?}"));
    let stalled_before = ledger.count_stalled().await.expect("count stalled");

    let mut claimed = ledger
        .claim_due(T0, 1_000, page())
        .await
        .expect("claim the due page")
        .into_iter()
        .filter_map(|claim| alias_of(&ids, &claim.id).map(|alias| (alias, claim)))
        .collect::<Vec<_>>();
    claimed.sort_by(|left, right| left.0.cmp(&right.0));
    for (alias, claim) in &claimed {
        out.push(format!(
            "claimed {alias} attempts={} key_decodes={}",
            claim.attempts,
            claim.key.is_ok()
        ));
    }
    let token = |alias: &str| {
        claimed
            .iter()
            .find(|(known, _)| known == alias)
            .map(|(_, claim)| claim.token.clone())
            .expect("the script claimed this alias")
    };
    let settlements = [
        ("a", ObligationSettlement::Delivered),
        (
            "b",
            ObligationSettlement::Retry {
                due_at_ms: T0 + 5_000,
                error: "retry later".to_owned(),
            },
        ),
        (
            "c",
            ObligationSettlement::Stall {
                reason: StallReason::Refused,
                error: "refused".to_owned(),
            },
        ),
    ];
    for (alias, settlement) in settlements {
        let outcome = ledger
            .settle(&ids[alias], &token(alias), settlement, T0 + 10)
            .await
            .expect("settle");
        out.push(format!("settle {alias} -> {outcome:?}"));
    }
    let stale = ledger
        .settle(
            &ids["a"],
            &token("a"),
            ObligationSettlement::Delivered,
            T0 + 11,
        )
        .await
        .expect("settle a settled claim");
    out.push(format!("settle a again -> {stale:?}"));

    let early = ledger
        .claim_due(T0 + 1_000, 1_000, page())
        .await
        .expect("claim before the backoff")
        .into_iter()
        .filter_map(|claim| alias_of(&ids, &claim.id))
        .collect::<Vec<_>>();
    out.push(format!("claim_due before backoff -> {early:?}"));
    let immediate = ledger
        .claim(&ids["b"], T0 + 1_000, 60_000)
        .await
        .expect("immediate claim");
    out.push(format!(
        "claim b immediately -> attempts={:?}",
        immediate.as_ref().map(|claim| claim.attempts)
    ));
    let immediate = immediate.expect("a due row is claimed immediately");
    let exhausted = ledger
        .settle(
            &ids["b"],
            &immediate.token,
            ObligationSettlement::Stall {
                reason: StallReason::AttemptsExhausted,
                error: "exhausted".to_owned(),
            },
            T0 + 1_001,
        )
        .await
        .expect("stall b");
    out.push(format!("stall b -> {exhausted:?}"));

    let stalled = ledger
        .list_stalled(None, page())
        .await
        .expect("list stalled");
    // Listing order is id order, and ids are minted fresh per backend: the
    // comparison reads the listing in alias order instead.
    let mut listed = stalled
        .into_iter()
        .filter_map(|entry| {
            alias_of(&ids, &entry.id).map(|alias| {
                format!(
                    "stalled {alias} kind={} reason={} attempts={} error={:?} at={} key_decodes={}",
                    entry.kind.label(),
                    entry.reason.as_str(),
                    entry.attempts,
                    entry.last_error,
                    entry.stalled_at_ms,
                    entry.key.is_ok()
                )
            })
        })
        .collect::<Vec<_>>();
    listed.sort();
    out.extend(listed);
    let stalled_after = ledger.count_stalled().await.expect("count stalled");
    out.push(format!(
        "stalled count delta -> {}",
        stalled_after.saturating_sub(stalled_before)
    ));
    for alias in ["c", "c", "a"] {
        let rearmed = ledger.rearm(&ids[alias], T0 + 2_000).await.expect("rearm");
        out.push(format!("rearm {alias} -> {rearmed}"));
    }
    for alias in aliases {
        let state = ledger.state(&ids[alias]).await.expect("state");
        out.push(format!("state {alias} -> {state:?}"));
    }
    let unknown = ledger
        .state(&ObligationId::new(format!(
            "session_delete:{prefix}-unknown"
        )))
        .await
        .expect("state of an unknown id");
    out.push(format!("state unknown -> {unknown:?}"));
    out
}

fn lease_line(step: &str, answer: &LeaseAnswer, holders: &[(&str, &HolderId)]) -> String {
    let holder = answer.row.as_ref().map(|row| {
        holders
            .iter()
            .find(|(_, holder)| **holder == row.holder)
            .map_or("?", |(alias, _)| *alias)
    });
    format!(
        "{step} -> leader={} holder={holder:?} rank={:?} term={:?}",
        answer.leader,
        answer.row.as_ref().map(|row| row.generation_rank),
        answer.row.as_ref().map(|row| row.term)
    )
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot answer panics the harness with its name by design"
)]
async fn lease_transcript(stores: &dyn StoreSet, prefix: &str) -> Transcript {
    let lease = stores.recovery_leader();
    let name = LeaseName::new(format!("recovery:{prefix}"));
    let claim = |holder: &str, rank: i64| LeaseClaim {
        name: name.clone(),
        holder: HolderId::new(format!("{prefix}:{holder}")),
        generation_rank: rank,
        ttl_ms: 60_000,
        min_tenure_ms: 60_000,
    };
    let (one, two, three) = (claim("one", 0), claim("two", 0), claim("three", 5));
    let holders = [
        ("one", &one.holder),
        ("two", &two.holder),
        ("three", &three.holder),
    ];
    let mut out = vec![
        lease_line(
            "acquire one",
            &lease.acquire(&one).await.expect("acquire"),
            &holders,
        ),
        lease_line(
            "acquire two",
            &lease.acquire(&two).await.expect("acquire"),
            &holders,
        ),
        lease_line(
            "acquire three within tenure",
            &lease.acquire(&three).await.expect("acquire"),
            &holders,
        ),
        lease_line(
            "renew one",
            &lease.renew(&one, 1).await.expect("renew"),
            &holders,
        ),
        lease_line(
            "renew two",
            &lease.renew(&two, 1).await.expect("renew"),
            &holders,
        ),
    ];
    let resigned = lease.resign(&name, &two.holder, 1).await.expect("resign");
    out.push(format!("resign two -> {resigned}"));
    let resigned = lease.resign(&name, &one.holder, 1).await.expect("resign");
    out.push(format!("resign one -> {resigned}"));
    out.push(lease_line(
        "acquire two after resign",
        &lease.acquire(&two).await.expect("acquire"),
        &holders,
    ));
    out.push(lease_line(
        "renew one after resign",
        &lease.renew(&one, 1).await.expect("renew"),
        &holders,
    ));
    out.push(format!(
        "due claims need a leader -> {}",
        lease.due_claims_need_leader()
    ));
    out
}

/// The one line where the backends are meant to differ: SQLite has one
/// writer, so its due claims are leader-only; PostgreSQL claims skip each
/// other's locked rows, so every deployment claims.
fn without_claim_policy(transcript: &Transcript) -> Transcript {
    transcript
        .iter()
        .filter(|line| !line.starts_with("due claims need a leader"))
        .cloned()
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open panics the harness with its name by design"
)]
pub(super) async fn compare_obligation_ledgers(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    nonce: &str,
) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the SQLite memory obligation store set");
    let file = lash_sqlite_store::SqliteStoreSet::open(sqlite_root.join("obligation-ledgers"))
        .await
        .expect("open the SQLite file obligation store set");
    let attachments = tempfile::tempdir().expect("attachment directory");
    let postgres_stores = lash_postgres_store::PostgresStoreSet::new(
        postgres,
        Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        )),
    );
    let backends: [(&str, &dyn StoreSet); 3] = [
        ("sqlite-memory", &memory),
        ("sqlite", &file),
        ("postgres", &postgres_stores),
    ];
    let mut observations = Vec::new();
    for (name, stores) in backends {
        let prefix = format!("fig-3850-{nonce}-{name}");
        let ledger = ledger_transcript(stores, &prefix).await;
        let lease = lease_transcript(stores, &prefix).await;
        observations.push((name, ledger, lease));
    }
    for pair in observations.windows(2) {
        let ((left, left_ledger, left_lease), (right, right_ledger, right_lease)) =
            (&pair[0], &pair[1]);
        assert_eq!(
            left_ledger, right_ledger,
            "obligation ledger answers differ between {left} and {right}"
        );
        assert_eq!(
            without_claim_policy(left_lease),
            without_claim_policy(right_lease),
            "recovery leader answers differ between {left} and {right}"
        );
    }
    for (name, _, lease) in &observations {
        assert_eq!(
            lease.last().map(String::as_str),
            Some(if *name == "postgres" {
                "due claims need a leader -> false"
            } else {
                "due claims need a leader -> true"
            }),
            "{name} declares the wrong due-claim policy"
        );
    }
    eprintln!(
        "PASS obligation_ledgers_and_recovery_lease: backends=3 ledger_steps={} lease_steps={}",
        observations[0].1.len(),
        observations[0].2.len()
    );
}
