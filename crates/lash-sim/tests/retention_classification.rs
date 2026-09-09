//! The ratified 2026-09-08 retention census (FIG-2503): 41 SQLite / 42 PostgreSQL.
//! Like schema_congruence.rs, this ordinary integration test is discovered by
//! the workspace nextest CI shards. Every new durable table needs a declaration.
use std::collections::BTreeSet;

const SQLITE_SCHEMA: &str = include_str!("../../lash-sqlite-store/src/schema.rs");
const POSTGRES_SCHEMA: &str = include_str!("../../lash-postgres-store/schema.sql");

#[derive(Clone, Copy, Debug)]
enum RetentionClass {
    Bounded {
        lever: &'static str,
    },
    LifecycleOwned {
        scope: &'static str,
    },
    PermanentlyExempt {
        reason: &'static str,
    },
    /// Explicit known-gap(FIG-xxxx), never an implicit catch-all for new tables.
    KnownGap {
        issue: &'static str,
    },
}
use RetentionClass::{Bounded, KnownGap, LifecycleOwned, PermanentlyExempt};

// Names are SQLite logical names. PostgreSQL aliases are explicit below.
// A class describes eligibility, never an automatic background schedule.
const CENSUS: &[(&str, RetentionClass)] = &[
    (
        "blobs",
        Bounded {
            lever: "gc_unreachable; session-owner blob reclaim",
        },
    ),
    (
        "session_head",
        LifecycleOwned {
            scope: "session deletion",
        },
    ),
    (
        "node_anchors",
        Bounded {
            lever: "explicit unpin; pins outlive their source session",
        },
    ),
    (
        "checkpoint_blob_refs",
        LifecycleOwned {
            scope: "checkpoint root projection and graph GC",
        },
    ),
    (
        "deleted_sessions",
        PermanentlyExempt {
            reason: "non-reusable session identity: FIG-754 / FIG-748",
        },
    ),
    (
        "graph_nodes",
        Bounded {
            lever: "unreachable ancestry retirement and vacuum; deleted-owner reclaim",
        },
    ),
    (
        "fork_lineage",
        LifecycleOwned {
            scope: "fork session",
        },
    ),
    (
        "usage_deltas",
        Bounded {
            lever: "SessionStoreFactory::reclaim_retained_evidence; terminal session, receipt anti-join",
        },
    ),
    (
        "session_meta",
        LifecycleOwned {
            scope: "session deletion",
        },
    ),
    (
        "session_meta_pending_observer_intents",
        LifecycleOwned {
            scope: "session metadata projection replacement and deletion",
        },
    ),
    (
        "session_meta_fork_inheritance_processes",
        LifecycleOwned {
            scope: "session metadata projection replacement and deletion",
        },
    ),
    (
        "runtime_turn_commits",
        Bounded {
            lever: "SessionStoreFactory::reclaim_retained_evidence(RetentionBound); terminal session",
        },
    ),
    (
        "turn_cancel_requests",
        LifecycleOwned {
            scope: "session vacuum and deletion",
        },
    ),
    (
        "session_execution_leases",
        LifecycleOwned {
            scope: "one fence per session; session deletion",
        },
    ),
    (
        "queued_work_batches",
        LifecycleOwned {
            scope: "claim settlement, acknowledgement, session deletion",
        },
    ),
    (
        "queued_work_items",
        LifecycleOwned {
            scope: "owning queued-work batch",
        },
    ),
    (
        "wake_redelivery_fences",
        LifecycleOwned {
            scope: "receiver session; survives queue consumption",
        },
    ),
    (
        "pending_turn_inputs",
        LifecycleOwned {
            scope: "terminal-input vacuum and session deletion; accepted-row gap FIG-1511",
        },
    ),
    (
        "attachment_manifest",
        Bounded {
            lever: "explicit forget and attachment GC; retained fork/pin graph prefixes are prune preconditions",
        },
    ),
    (
        "attachment_condemnations",
        LifecycleOwned {
            scope: "digest writer/GC fence; explicit host release of abandoned fences",
        },
    ),
    (
        "artifact_refs",
        LifecycleOwned {
            scope: "session trigger manifest; other namespaces are explicit retained service roots",
        },
    ),
    (
        "processes",
        Bounded {
            lever: "prune_terminal_processes; projection watermark and no outstanding deliveries/plans",
        },
    ),
    (
        "process_change_clock",
        PermanentlyExempt {
            reason: "singleton monotone change sequence and retention horizon",
        },
    ),
    (
        "process_events",
        LifecycleOwned {
            scope: "terminal process reclamation",
        },
    ),
    (
        "wake_allocation_floors",
        LifecycleOwned {
            scope: "target session; survives process reincarnation",
        },
    ),
    (
        "process_wake_deliveries",
        LifecycleOwned {
            scope: "process; pending/enqueuing deliveries prevent its prune",
        },
    ),
    (
        "process_observers",
        LifecycleOwned {
            scope: "observer edge, session and process",
        },
    ),
    (
        "process_tombstones",
        Bounded {
            lever: "compact_process_tombstones; cutoff, projector watermark and delivery exclusions",
        },
    ),
    ("process_leases", LifecycleOwned { scope: "process" }),
    (
        "process_segment_handovers",
        LifecycleOwned {
            scope: "handover acknowledgement, supersession and process",
        },
    ),
    (
        "process_parent_end_plans",
        LifecycleOwned {
            scope: "completion and process; pending plan blocks process prune",
        },
    ),
    ("tool_intent_submissions", KnownGap { issue: "FIG-1509" }),
    (
        "trigger_subscriptions",
        LifecycleOwned {
            scope: "session-owner reconciliation; host/platform fences intentionally permanent",
        },
    ),
    (
        "trigger_occurrences",
        Bounded {
            lever: "reclaim_trigger_occurrences; terminally armed cutoff and no deliveries",
        },
    ),
    (
        "trigger_deliveries",
        LifecycleOwned {
            scope: "process retention reconciliation and owning occurrence",
        },
    ),
    (
        "trigger_mutation_receipts",
        // Host receipts lack a safe terminal gate (FIG-1956 / FIG-653).
        KnownGap { issue: "FIG-1956" },
    ),
    // Session and process retirement select by owner; runtime-operation scopes
    // retire through `EffectJournalRetirement::RuntimeOperation` once their
    // receipt is back (facade plugin operations) or their process is pruned
    // (trigger-delivery reconcile), groups and children in one transaction.
    (
        "runtime_effect_replay",
        LifecycleOwned {
            scope: "session, process, or runtime-operation retirement",
        },
    ),
    (
        "runtime_effect_group",
        LifecycleOwned {
            scope: "session, process, or runtime-operation retirement",
        },
    ),
    (
        "await_event_meta",
        PermanentlyExempt {
            reason: "singleton signing secret keeps issued promise keys valid across reopen",
        },
    ),
    // Session promises die with session revocation; process and
    // runtime-operation promises die with their scope's journal retirement.
    (
        "await_event_waits",
        LifecycleOwned {
            scope: "session revocation or process/runtime-operation retirement",
        },
    ),
    (
        "await_event_revoked_sessions",
        PermanentlyExempt {
            reason: "single-use session identity and permanent promise-key revocation",
        },
    ),
    (
        "effect_scope_retirements",
        PermanentlyExempt {
            reason: "single-use process and runtime-operation identities keep a permanent scope fence",
        },
    ),
];

const POSTGRES_ONLY: &[(&str, RetentionClass)] = &[(
    "lash_schema_versions",
    PermanentlyExempt {
        reason: "one current version per fixed component; not accumulating migration history",
    },
)];

fn declared_tables(source: &str) -> BTreeSet<String> {
    // Exclude SQLite's cfg(test) migration fixtures. All four production schema
    // constants precede that module. Token scanning accepts multiline DDL and
    // CREATE TABLE with or without IF NOT EXISTS.
    let words: Vec<_> = source
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .split_ascii_whitespace()
        .collect();
    words
        .windows(2)
        .enumerate()
        .filter_map(|(index, pair)| {
            if !pair[0].eq_ignore_ascii_case("CREATE") || !pair[1].eq_ignore_ascii_case("TABLE") {
                return None;
            }
            let mut name_index = index + 2;
            if words.get(name_index)?.eq_ignore_ascii_case("IF") {
                name_index += 3;
            }
            words
                .get(name_index)
                .map(|name| name.trim_end_matches('(').trim_matches('"').to_string())
        })
        .collect()
}

fn postgres_name(sqlite: &str) -> String {
    match sqlite {
        "session_head" => "lash_sessions".to_string(),
        "artifact_refs" => "lash_lashlang_artifacts".to_string(),
        name => format!("lash_{name}"),
    }
}

fn assert_classified(source: &str, postgres: bool) {
    assert_eq!(CENSUS.len(), 42, "ratified census must remain explicit");
    let mut declared = BTreeSet::new();
    let entries = CENSUS
        .iter()
        .chain(if postgres { POSTGRES_ONLY } else { &[] }.iter());
    for (table, class) in entries {
        let detail = match class {
            Bounded { lever } => lever,
            LifecycleOwned { scope } => scope,
            PermanentlyExempt { reason } => reason,
            KnownGap { issue } => {
                assert!(
                    ["FIG-1509", "FIG-1956", "FIG-2499", "FIG-2500"].contains(issue),
                    "new gaps need explicit review"
                );
                issue
            }
        };
        assert!(
            !detail.trim().is_empty(),
            "{table} must name its lever, owner, reason or known issue"
        );
        let name = if postgres && *table != "lash_schema_versions" {
            postgres_name(table)
        } else {
            (*table).to_string()
        };
        assert!(declared.insert(name), "duplicate census entry: {table}");
    }
    let actual = declared_tables(source);
    let unclassified: Vec<_> = actual.difference(&declared).collect();
    let absent: Vec<_> = declared.difference(&actual).collect();
    assert!(
        unclassified.is_empty() && absent.is_empty(),
        "retention census drift: unclassified={unclassified:?}; absent={absent:?}"
    );
}

#[test]
fn sqlite_retention_classes_cover_every_durable_table() {
    assert_classified(SQLITE_SCHEMA, false);
}

#[test]
fn postgres_retention_classes_cover_every_durable_table() {
    assert_classified(POSTGRES_SCHEMA, true);
}
