//! The ingress ledger (ADR 0109 §3, FIG-3851) compared across the three
//! backends: admission arms a turn input and a queued batch in its own
//! transaction, and the composed ledger over the two tables must claim,
//! settle, stall, list and re-arm them identically.
//!
//! Admission arms at the store's own clock, so the script claims at a fixed
//! instant far past it; every settle instant comes from the script. The
//! Postgres database is shared with every other case and run, so each
//! backend's rows carry the run nonce and every read is filtered to them.

use std::num::NonZeroUsize;

use lash_core::StoreSet;
use lash_core::store::ingress_obligation::ingress_obligation_id;
use lash_core::store::{
    ObligationId, ObligationKey, ObligationKind, ObligationSettlement, StallReason,
};

use super::*;

/// An instant past every admission clock: 2100-01-01.
const FAR: u64 = 4_102_444_800_000;

type Transcript = Vec<String>;

fn page() -> NonZeroUsize {
    NonZeroUsize::new(10_000).unwrap_or(NonZeroUsize::MIN)
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open or answer panics the harness with its name by design"
)]
async fn ingress_transcript(stores: &dyn StoreSet, prefix: &str) -> Transcript {
    let mut out = Transcript::new();
    let ledger = stores.obligation_ledger(ObligationKind::Ingress);
    let session_id = SessionId::from(format!("{prefix}-ingress"));
    let store = admit_test_session(
        stores.session_store_factory(),
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        },
    )
    .await
    .expect("create the ingress session");
    let first = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            session_id.clone(),
            TurnInputIngress::NextTurn,
            TurnInput::text("first"),
        ))
        .await
        .expect("admit the first input")
        .input_id
        .to_string();
    let second = store
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            session_id.clone(),
            TurnInputIngress::NextTurn,
            TurnInput::text("second"),
        ))
        .await
        .expect("admit the second input")
        .input_id
        .to_string();
    let batch = store
        .enqueue_queued_work(QueuedWorkBatchDraft::new(
            session_id.clone(),
            DeliveryPolicy::EarliestSafeBoundary,
            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                reason: "ingress differential".to_owned(),
            },
        ))
        .await
        .expect("admit the batch")
        .batch_id
        .to_string();
    let items = [("a", first), ("b", second), ("c", batch)];
    let ids: BTreeMap<&str, ObligationId> = items
        .iter()
        .map(|(alias, item)| (*alias, ingress_obligation_id(item)))
        .collect();
    let alias_of = |id: &ObligationId| {
        ids.iter()
            .find(|(_, known)| *known == id)
            .map(|(alias, _)| (*alias).to_owned())
    };
    for (alias, item) in &items {
        let state = ledger.state(&ids[alias]).await.expect("state");
        out.push(format!("admitted {alias} -> {state:?}"));
        let again = ledger
            .arm(
                &ObligationKey::Ingress {
                    session_id: session_id.clone(),
                    item_id: item.clone(),
                },
                FAR,
            )
            .await
            .expect("arm an admitted row");
        out.push(format!("arm {alias} again -> {again:?}"));
    }
    let missing = ledger
        .arm(
            &ObligationKey::Ingress {
                session_id: session_id.clone(),
                item_id: format!("{prefix}-missing"),
            },
            FAR,
        )
        .await
        .expect("arm a missing row");
    out.push(format!("arm missing -> {missing:?}"));
    let stalled_before = ledger.count_stalled().await.expect("count stalled");

    let mut claimed = ledger
        .claim_due(FAR, 1_000, page())
        .await
        .expect("claim the due page")
        .into_iter()
        .filter_map(|claim| alias_of(&claim.id).map(|alias| (alias, claim)))
        .collect::<Vec<_>>();
    claimed.sort_by(|left, right| left.0.cmp(&right.0));
    for (alias, claim) in &claimed {
        let names_its_row = claim.key.as_ref().is_ok_and(|key| {
            *key == ObligationKey::Ingress {
                session_id: session_id.clone(),
                item_id: items
                    .iter()
                    .find(|(known, _)| *known == alias.as_str())
                    .map(|(_, item)| item.clone())
                    .unwrap_or_default(),
            }
        });
        out.push(format!(
            "claimed {alias} attempts={} names_its_row={names_its_row}",
            claim.attempts
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
                due_at_ms: FAR + 5_000,
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
            .settle(&ids[alias], &token(alias), settlement, FAR + 10)
            .await
            .expect("settle");
        out.push(format!("settle {alias} -> {outcome:?}"));
    }
    let retried = ledger
        .claim(&ids["b"], FAR + 1_000, 60_000)
        .await
        .expect("claim immediately")
        .expect("a due row is claimed immediately, backoff or not");
    out.push(format!(
        "claim b immediately -> attempts={}",
        retried.attempts
    ));
    let again = ledger
        .claim(&ids["b"], FAR + 1_001, 60_000)
        .await
        .expect("claim a claimed row");
    out.push(format!("claim b again -> {}", again.is_some()));
    let exhausted = ledger
        .settle(
            &ids["b"],
            &retried.token,
            ObligationSettlement::Stall {
                reason: StallReason::AttemptsExhausted,
                error: "exhausted".to_owned(),
            },
            FAR + 5_001,
        )
        .await
        .expect("stall b");
    out.push(format!("stall b -> {exhausted:?}"));

    let mut listed = ledger
        .list_stalled(None, page())
        .await
        .expect("list stalled")
        .into_iter()
        .filter_map(|entry| {
            alias_of(&entry.id).map(|alias| {
                format!(
                    "stalled {alias} kind={} reason={} attempts={} error={:?} at={}",
                    entry.kind.label(),
                    entry.reason.as_str(),
                    entry.attempts,
                    entry.last_error,
                    entry.stalled_at_ms,
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
        let rearmed = ledger.rearm(&ids[alias], FAR + 6_000).await.expect("rearm");
        out.push(format!("rearm {alias} -> {rearmed}"));
    }
    for alias in ["a", "b", "c"] {
        let state = ledger.state(&ids[alias]).await.expect("state");
        out.push(format!("state {alias} -> {state:?}"));
    }
    out
}

#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open panics the harness with its name by design"
)]
pub(super) async fn compare_ingress_ledgers(
    sqlite_root: &Path,
    postgres: &PostgresStorage,
    nonce: &str,
) {
    let memory = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open the SQLite memory ingress store set");
    let file = lash_sqlite_store::SqliteStoreSet::open(sqlite_root.join("ingress-ledgers"))
        .await
        .expect("open the SQLite file ingress store set");
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
        let prefix = format!("fig-3851-{nonce}-{name}");
        observations.push((name, ingress_transcript(stores, &prefix).await));
    }
    for pair in observations.windows(2) {
        let ((left, left_ledger), (right, right_ledger)) = (&pair[0], &pair[1]);
        assert_eq!(
            left_ledger, right_ledger,
            "ingress ledger answers differ between {left} and {right}"
        );
    }
    eprintln!(
        "PASS ingress_ledgers: backends=3 steps={}",
        observations[0].1.len()
    );
}
