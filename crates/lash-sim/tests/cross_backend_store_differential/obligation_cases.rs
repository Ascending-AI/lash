//! Obligation ledgers and the recovery leader lease (ADR 0109 §1) compared
//! across the three backends: one scripted sequence of arm, claim, settle,
//! re-arm and listing calls on the `session_delete` ledger, the same script
//! on the `process_start` ledger armed by process registration (FIG-3964),
//! and of acquire, renew and resign calls on one lease, must read back
//! identically.
//!
//! The Postgres database is shared with every other case and every earlier
//! run, so each backend's rows carry the run nonce and every read is filtered
//! to them; ids, tokens and database-clock instants are normalized away, and
//! the ledger's own instants come from the script, not a clock. The
//! `processes` table is not session-scoped, so the `process_start` leg's
//! residue checks digest the rows the script registered by id — the digest
//! covers the `start_obligation_*` family, and a planted write to it must
//! read as a change.

use std::num::NonZeroUsize;

use lash_core::StoreSet;
use lash_core::store::{
    HolderId, LeaseAnswer, LeaseClaim, LeaseName, ObligationId, ObligationKey, ObligationKind,
    ObligationSettlement, StallReason, process_start_obligation_id,
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
            .admit_session(&SessionStoreCreateRequest {
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
        let standing = ledger.standing(&ids[alias]).await.expect("standing");
        out.push(format!("standing {alias} -> {standing:?}"));
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

/// One backend's `processes` residue handle: the process-registry database
/// of a SQLite store set — the open name `database_uri` hands out, a file
/// path or the shared `memdb` URI — or the pool of the shared PostgreSQL
/// database (FIG-3964).
enum ProcessResidue<'a> {
    Sqlite(&'a str),
    Postgres(&'a PgPool),
}

impl ProcessResidue<'_> {
    async fn digest(&self, process_ids: &[String]) -> ResidueDigest {
        match self {
            Self::Sqlite(open_name) => {
                sqlite_process_obligation_digest(Path::new(open_name), process_ids)
            }
            Self::Postgres(pool) => postgres_process_obligation_digest(pool, process_ids).await,
        }
    }

    /// Write one `start_obligation_*` column around the ledger: the residue
    /// coverage is proven by this write moving the digest, and unconstrained
    /// `last_error` is exactly where a leaked write would land.
    #[expect(
        clippy::expect_used,
        reason = "test support: a backend that cannot take the planted write panics the harness with its name by design"
    )]
    async fn plant_start_obligation_divergence(&self, process_id: &str) {
        match self {
            Self::Sqlite(open_name) => {
                let connection = rusqlite::Connection::open(Path::new(open_name))
                    .expect("open the SQLite residue writer");
                connection
                    .execute(
                        "UPDATE processes SET start_obligation_last_error = 'planted divergence'
                         WHERE process_id = ?1",
                        rusqlite::params![process_id],
                    )
                    .expect("plant a start_obligation divergence");
            }
            Self::Postgres(pool) => {
                sqlx::query(
                    "UPDATE lash_processes SET start_obligation_last_error = 'planted divergence'
                     WHERE process_id = $1",
                )
                .bind(process_id)
                .execute(*pool)
                .await
                .expect("plant a start_obligation divergence");
            }
        }
    }
}

/// The `process_start` ledger's leg (FIG-3964): registration is its producer
/// arm, so the script registers engine-owned processes — armed in the
/// registration transaction under the derived id — and an externally-owned
/// one, which owes no start and is the one row the repair arm can still
/// take. Claim, settle, stall, re-arm and listing calls then run the same
/// course as the `session_delete` script above, each refused answer checked
/// for residue on the `start_obligation_*` family.
#[expect(
    clippy::expect_used,
    reason = "test support: a backend that cannot open or answer panics the harness with its name by design"
)]
async fn process_start_transcript(
    stores: &dyn StoreSet,
    prefix: &str,
    residue: &ProcessResidue<'_>,
) -> Transcript {
    let mut out = Transcript::new();
    let ledger = stores.obligation_ledger(ObligationKind::ProcessStart);
    let registry = stores.process_registry();
    let aliases = ["a", "b", "c"];
    let mut ids = BTreeMap::<&str, ObligationId>::new();
    let mut processes = BTreeMap::<&str, lash_sansio::ProcessId>::new();
    let alias_of = |ids: &BTreeMap<&str, ObligationId>, id: &ObligationId| {
        ids.iter()
            .find(|(_, known)| *known == id)
            .map(|(alias, _)| (*alias).to_owned())
    };
    let registration = |input: lash_core::ProcessInput| {
        lash_core::ProcessRegistration::new(
            input,
            lash_core::ProcessProvenance::host(),
            lash_core::Lifetime::Detached,
        )
    };
    for alias in aliases {
        let record = registry
            .register_process(
                registration(lash_core::ProcessInput::Engine {
                    kind: "process-start-differential".to_string(),
                    payload: serde_json::Value::Null,
                })
                .with_execution_env_ref(Some(
                    lash_core::ProcessExecutionEnvRef::new(
                        "process-env:process-start-differential",
                    ),
                )),
            )
            .await
            .expect("register an engine process");
        let id = process_start_obligation_id(&record.id);
        out.push(format!(
            "register {alias} -> {:?}",
            ledger.state(&id).await.expect("read the armed state")
        ));
        ids.insert(alias, id);
        processes.insert(alias, record.id);
    }
    let external = registry
        .register_process(registration(lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        }))
        .await
        .expect("register an external process");
    out.push(format!(
        "register external -> {:?}",
        ledger
            .state(&process_start_obligation_id(&external.id))
            .await
            .expect("read the unarmed state")
    ));
    processes.insert("d", external.id);
    let process_ids = || -> Vec<String> {
        processes
            .values()
            .map(|id| id.as_str().to_owned())
            .collect()
    };
    let residue_line = |label: &str, before: &ResidueDigest, after: &ResidueDigest| {
        format!("residue of {label} -> {:?}", before.changed_tables(after))
    };

    let before = residue.digest(&process_ids()).await;
    let rearmed = ledger
        .arm(
            &ObligationKey::ProcessStart {
                process_id: processes["a"].clone(),
            },
            T0,
        )
        .await
        .expect("arm an armed row");
    out.push(format!("arm a again -> {rearmed:?}"));
    out.push(residue_line(
        "arming an armed row",
        &before,
        &residue.digest(&process_ids()).await,
    ));
    let before = residue.digest(&process_ids()).await;
    let missing = ledger
        .arm(
            &ObligationKey::ProcessStart {
                process_id: lash_sansio::ProcessId::fixture(&format!("{prefix}-missing")),
            },
            T0,
        )
        .await
        .expect("arm a missing row");
    out.push(format!("arm missing -> {missing:?}"));
    out.push(residue_line(
        "arming a missing row",
        &before,
        &residue.digest(&process_ids()).await,
    ));
    let armed = ledger
        .arm(
            &ObligationKey::ProcessStart {
                process_id: processes["d"].clone(),
            },
            T0,
        )
        .await
        .expect("arm the unarmed row");
    out.push(format!("arm external -> {}", armed.is_some()));
    ids.insert("d", armed.expect("an unarmed row arms"));
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
        ("d", ObligationSettlement::Delivered),
    ];
    for (alias, settlement) in settlements {
        let outcome = ledger
            .settle(&ids[alias], &token(alias), settlement, T0 + 10)
            .await
            .expect("settle");
        out.push(format!("settle {alias} -> {outcome:?}"));
    }
    let before = residue.digest(&process_ids()).await;
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
    out.push(residue_line(
        "settling a settled claim",
        &before,
        &residue.digest(&process_ids()).await,
    ));

    let before = residue.digest(&process_ids()).await;
    let early = ledger
        .claim_due(T0 + 1_000, 1_000, page())
        .await
        .expect("claim before the backoff")
        .into_iter()
        .filter_map(|claim| alias_of(&ids, &claim.id))
        .collect::<Vec<_>>();
    out.push(format!("claim_due before backoff -> {early:?}"));
    out.push(residue_line(
        "claiming before the backoff",
        &before,
        &residue.digest(&process_ids()).await,
    ));
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
    // Listing order is id order, and ids are minted per backend: the
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
    let before = residue.digest(&process_ids()).await;
    let rearmed = ledger.rearm(&ids["c"], T0 + 2_000).await.expect("rearm");
    out.push(format!("rearm c -> {rearmed}"));
    let rearmed = ledger.rearm(&ids["c"], T0 + 2_000).await.expect("rearm");
    out.push(format!("rearm c again -> {rearmed}"));
    out.push(residue_line(
        "re-arming a re-armed row",
        &before,
        &residue.digest(&process_ids()).await,
    ));
    let before = residue.digest(&process_ids()).await;
    let rearmed = ledger.rearm(&ids["a"], T0 + 2_000).await.expect("rearm");
    out.push(format!("rearm a -> {rearmed}"));
    out.push(residue_line(
        "re-arming a delivered row",
        &before,
        &residue.digest(&process_ids()).await,
    ));
    for alias in ["a", "b", "c", "d"] {
        let standing = ledger.standing(&ids[alias]).await.expect("standing");
        out.push(format!("standing {alias} -> {standing:?}"));
    }
    let unknown = ledger
        .state(&ObligationId::new(format!(
            "process_start:{prefix}-unknown"
        )))
        .await
        .expect("state of an unknown id");
    out.push(format!("state unknown -> {unknown:?}"));

    let before = residue.digest(&process_ids()).await;
    residue
        .plant_start_obligation_divergence(processes["a"].as_str())
        .await;
    out.push(residue_line(
        "a planted start_obligation divergence",
        &before,
        &residue.digest(&process_ids()).await,
    ));
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
    // Every registration arms a ProcessStart obligation and nothing on the
    // shared database ever delivers one, so earlier runs leave due rows the
    // leg's first claim would otherwise compete with. Quiesce them: the due
    // page must be this leg's alone.
    sqlx::query(
        "UPDATE lash_processes
         SET start_obligation_state = 'delivered', start_obligation_due_at_ms = NULL,
             start_obligation_claim_token = NULL, start_obligation_settled_at_ms = 0
         WHERE start_obligation_state IN ('due', 'claimed')",
    )
    .execute(postgres.pool())
    .await
    .expect("quiesce stale process-start obligations");
    let memory_registry = memory.database_uri(lash_sqlite_store::SqliteDatabase::ProcessRegistry);
    let file_registry = file.database_uri(lash_sqlite_store::SqliteDatabase::ProcessRegistry);
    let backends: [(&str, &dyn StoreSet, ProcessResidue<'_>); 3] = [
        (
            "sqlite-memory",
            &memory,
            ProcessResidue::Sqlite(&memory_registry),
        ),
        ("sqlite", &file, ProcessResidue::Sqlite(&file_registry)),
        (
            "postgres",
            &postgres_stores,
            ProcessResidue::Postgres(postgres.pool()),
        ),
    ];
    let mut observations = Vec::new();
    for (name, stores, residue) in backends {
        let prefix = format!("fig-3850-{nonce}-{name}");
        let ledger = ledger_transcript(stores, &prefix).await;
        let lease = lease_transcript(stores, &prefix).await;
        let starts = process_start_transcript(stores, &prefix, &residue).await;
        observations.push((name, ledger, lease, starts));
    }
    for pair in observations.windows(2) {
        let (
            (left, left_ledger, left_lease, left_starts),
            (right, right_ledger, right_lease, right_starts),
        ) = (&pair[0], &pair[1]);
        assert_eq!(
            left_ledger, right_ledger,
            "obligation ledger answers differ between {left} and {right}"
        );
        assert_eq!(
            without_claim_policy(left_lease),
            without_claim_policy(right_lease),
            "recovery leader answers differ between {left} and {right}"
        );
        assert_eq!(
            left_starts, right_starts,
            "process-start obligation answers differ between {left} and {right}"
        );
    }
    for (name, _, lease, starts) in &observations {
        assert_eq!(
            lease.last().map(String::as_str),
            Some(if *name == "postgres" {
                "due claims need a leader -> false"
            } else {
                "due claims need a leader -> true"
            }),
            "{name} declares the wrong due-claim policy"
        );
        assert_eq!(
            starts.last().map(String::as_str),
            Some("residue of a planted start_obligation divergence -> [\"processes\"]"),
            "{name}: the residue digest did not see the planted start_obligation write"
        );
    }
    eprintln!(
        "PASS obligation_ledgers_and_recovery_lease: backends=3 ledger_steps={} lease_steps={} process_start_steps={}",
        observations[0].1.len(),
        observations[0].2.len(),
        observations[0].3.len()
    );
}
