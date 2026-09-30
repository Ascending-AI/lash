//! Completeness gate for the durability law (FIG-2841).
//!
//! The differential harness is only a law about "every refused store
//! operation" if its operation inventory actually covers the fallible store
//! trait surface. This module reads the trait definitions out of the real
//! sources, reads the harness's own sources back, and refuses any fallible
//! trait method that is neither driven by the harness nor named in an
//! explicit exclusion list with a reason.
//!
//! Adding a fallible method to one of the gated traits therefore fails this
//! test until the method is either driven or excluded on the record.

/// Trait sources that define the gated surface.
const SESSION_STORE_SOURCE: &str = include_str!("../../../lash-core-store/src/store/mod.rs");
const SESSION_CATALOG_SOURCE: &str = include_str!("../../../lash-core-store/src/store/catalog.rs");
const SESSION_HISTORY_SOURCE: &str = include_str!("../../../lash-core-store/src/store/history.rs");
const DRIVE_EPOCH_SOURCE: &str = include_str!("../../../lash-core-store/src/store/drive_fence.rs");
const ROOT_STORE_SOURCE: &str = include_str!("../../../lash-core-store/src/store/root.rs");
const ATTACHMENT_STORE_SOURCE: &str = include_str!("../../../lash-core-store/src/attachments.rs");
const ATTACHMENT_REFERRERS_SOURCE: &str =
    include_str!("../../../lash-core-store/src/store/attachment_referrers.rs");
/// The factory's control-intent ledger (FIG-3600 S7): every method is driven,
/// none is excluded.
const CONTROL_INTENT_SOURCE: &str =
    include_str!("../../../lash-core-store/src/store/control_intent.rs");
/// The obligation ledgers and the recovery leader lease (ADR 0109 §1): every
/// method is driven by `obligation_cases`, none is excluded.
const OBLIGATION_SOURCE: &str = include_str!("../../../lash-core-store/src/store/obligation.rs");
const RECOVERY_LEADER_SOURCE: &str =
    include_str!("../../../lash-core-store/src/store/recovery_leader.rs");
/// A session's two-phase delete reads (ADR 0109 §4): every method is driven
/// by `session_delete_cases`, none is excluded.
const SESSION_DELETE_SOURCE: &str =
    include_str!("../../../lash-core-store/src/store/session_delete.rs");

/// The build-generation drain (FIG-3799): every method is driven by
/// `generation_drain_cases`, none is excluded.
const GENERATION_DRAIN_SOURCE: &str =
    include_str!("../../../lash-core-store/src/store/generation_drain.rs");

/// Every source file that makes up this test binary: the root file and every
/// `*.rs` under its module directory, read from the package at run time. A
/// method counts as covered when the harness calls it from one of these.
#[expect(
    clippy::expect_used,
    reason = "test support: the harness sources ship with the test; an unreadable one panics the gate by design"
)]
fn harness_sources() -> Vec<String> {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut paths = vec![tests.join("cross_backend_store_differential.rs")];
    let mut pending = vec![tests.join("cross_backend_store_differential")];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read the harness module directory") {
            let path = entry.expect("read a harness directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                paths.push(path);
            }
        }
    }
    assert!(
        paths.len() > 2,
        "found only {paths:?} under {}; the harness sources are not shipped with the test",
        tests.display()
    );
    paths
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("read a harness source"))
        .collect()
}

/// Every fallible segment of `RuntimeStore`; fleet format is infallible and
/// attachment referrers are checked separately below.
const GATED_SESSION_TRAITS: &[(&str, &str)] = &[
    (SESSION_CATALOG_SOURCE, "SessionCatalogStore"),
    (SESSION_STORE_SOURCE, "SessionCommitStore"),
    (SESSION_HISTORY_SOURCE, "SessionHistoryStore"),
    (SESSION_STORE_SOURCE, "TurnInputStore"),
    (SESSION_STORE_SOURCE, "QueuedWorkStore"),
    (DRIVE_EPOCH_SOURCE, "DriveEpochStore"),
    (ROOT_STORE_SOURCE, "RootStore"),
    (SESSION_STORE_SOURCE, "StoreMaintenance"),
];

/// Fallible session-store methods the harness deliberately does not drive.
///
/// Every entry is a method this differential cannot reach with the fixture it
/// builds, together with the suite that does own it. Removing a method from
/// both this list and the harness fails `store_trait_surface_is_fully_gated`.
const SESSION_STORE_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "list_sessions",
        "catalog-wide enumeration across the shared PostgreSQL database; owned by the session_store_factory_enumeration conformance suite",
    ),
    (
        "fork_points",
        "catalog-wide retained-point enumeration; owned by the session_store_factory conformance suite",
    ),
    (
        "contains_active_ancestor",
        "bounded ancestry predicate; owned by the session_history conformance suite",
    ),
    (
        "load_usage_totals",
        "head usage summary; owned by the runtime_persistence and session_history conformance suites",
    ),
    (
        "load_usage_ledger_page",
        "bounded usage history; owned by the session_history conformance suite",
    ),
    (
        "load_failure_evidence_page",
        "bounded failure history; owned by the session_history conformance suite",
    ),
    (
        "has_claimable_queued_work",
        "claimability predicate; owned by the queued_work conformance suite",
    ),
    (
        "validate_turn_cancellation_binding",
        "turn-cancellation surface: this fixture wires no TurnCancellationAuthority, so the \
         binding is never valid on any backend; owned by the turn_control conformance suite",
    ),
    (
        "authorize_turn_cancel_closure",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "pending_turn_cancel_closures",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "pending_turn_cancel_closure_pins",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "turn_is_committed",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "record_turn_cancel_request",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "turn_cancel_request",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "turn_cancel_request_intent",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "reconcile_turn_cancel_winner",
        "turn-cancellation surface; owned by the turn_control conformance suite",
    ),
    (
        "gc_unreachable",
        "store-wide blob reclamation across every session the factory owns. This differential \
         runs all of its cases against one shared PostgreSQL database, so a sweep launched \
         mid-run would collect blobs belonging to the other cases and report their loss as this \
         harness's own defect. Owned by the session_delete_blob_reclaim and \
         store_maintenance_outcome conformance suites, which each own their database",
    ),
    (
        "retain_admission_base",
        "turn-admission base-retention surface (FIG-3682): called by a turn's admission under \
         the session's execution lease, which this fixture never runs; owned by the \
         admission_base_retention conformance suite",
    ),
];

/// Fallible attachment (blob artifact) store methods.
///
/// The three session stores this harness compares hold no attachment bytes:
/// their durable surface is the attachment edge and pending-write rows, which the
/// residue digest already covers. The blob store itself is compared by
/// `attachment_blob_store_differential_agrees` in the same crate, which runs
/// the same three-backend comparison over SQLite memory, file and S3 blob
/// stores.
const ATTACHMENT_STORE_EXCLUSIONS: &[(&str, &str)] = &[
    (
        "put",
        "blob-byte store, not a session-row store; compared by \
         attachment_blob_store_differential_agrees",
    ),
    (
        "get",
        "blob-byte store, not a session-row store; compared by \
         attachment_blob_store_differential_agrees",
    ),
    (
        "delete",
        "blob-byte store, not a session-row store; compared by \
         attachment_blob_store_differential_agrees",
    ),
    (
        "list",
        "blob-byte store, not a session-row store; compared by \
         attachment_blob_store_differential_agrees",
    ),
    (
        "head",
        "blob-byte store, not a session-row store; compared by \
         attachment_blob_store_differential_agrees",
    ),
];

/// Every `AttachmentReferrers` method is driven. The trait is part of the
/// runtime store contract, with no excluded attachment-referrer methods.
const ATTACHMENT_REFERRERS_EXCLUSIONS: &[(&str, &str)] = &[];

/// Extract the fallible method names declared directly in `trait_name`.
///
/// A method is fallible when its signature (everything before the body brace
/// or the trailing semicolon) returns `Result` or the maintenance alias.
#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn fallible_trait_methods(source: &str, trait_name: &str) -> Vec<String> {
    let needle = format!("pub trait {trait_name}");
    let trait_start = source
        .find(&needle)
        .unwrap_or_else(|| panic!("trait `{trait_name}` is no longer declared in its source file"));
    let body_start = trait_start
        + source[trait_start..]
            .find('{')
            .expect("trait declaration opens a body");
    let mut depth = 0usize;
    let mut body_end = body_start;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = body_start + offset;
                    break;
                }
            }
            _ => {}
        }
    }
    let lines: Vec<&str> = source[body_start + 1..body_end].lines().collect();

    let mut methods = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        // Exactly one indent level: a method declared by this trait, never a
        // nested item inside a default body.
        let Some(name) = declared_method_name(line) else {
            index += 1;
            continue;
        };
        let mut signature = line.to_string();
        while !signature.contains('{') && !signature.trim_end().ends_with(';') {
            index += 1;
            signature.push('\n');
            signature.push_str(lines[index]);
        }
        let head = signature.split('{').next().unwrap_or(&signature);
        if head.contains("-> Result<") || head.contains("-> MaintenanceResult<") {
            methods.push(name);
        }
        index += 1;
    }
    assert!(
        !methods.is_empty(),
        "trait `{trait_name}` declared no fallible methods; the gate's parser has drifted"
    );
    methods
}

fn declared_method_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix("    ")?;
    if rest.starts_with(' ') {
        return None;
    }
    let rest = rest.strip_prefix("async ").unwrap_or(rest);
    let rest = rest.strip_prefix("fn ")?;
    let name: String = rest
        .chars()
        .take_while(|character| character.is_alphanumeric() || *character == '_')
        .collect();
    (!name.is_empty()).then_some(name)
}

fn harness_drives(sources: &[String], method: &str) -> bool {
    let call = format!(".{method}(");
    sources.iter().any(|source| source.contains(&call))
}

#[test]
fn store_trait_surface_is_fully_gated() {
    let sources = harness_sources();
    let mut missing = Vec::new();
    let mut stale_exclusions = Vec::new();
    let mut covered = 0usize;
    let mut excluded = 0usize;

    for &(source, trait_name) in GATED_SESSION_TRAITS {
        for method in fallible_trait_methods(source, trait_name) {
            let exclusion = SESSION_STORE_EXCLUSIONS
                .iter()
                .find(|(name, _)| *name == method);
            let driven = harness_drives(&sources, &method);
            match (driven, exclusion) {
                (true, None) => covered += 1,
                (false, Some((_, reason))) => {
                    assert!(
                        !reason.trim().is_empty(),
                        "exclusion for `{trait_name}::{method}` carries no reason"
                    );
                    excluded += 1;
                }
                (true, Some(_)) => stale_exclusions.push(format!("{trait_name}::{method}")),
                (false, None) => missing.push(format!("{trait_name}::{method}")),
            }
        }
    }

    for method in fallible_trait_methods(ATTACHMENT_REFERRERS_SOURCE, "AttachmentReferrers") {
        let exclusion = ATTACHMENT_REFERRERS_EXCLUSIONS
            .iter()
            .find(|(name, _)| *name == method);
        let driven = harness_drives(&sources, &method);
        match (driven, exclusion) {
            (true, None) => covered += 1,
            (false, Some((_, reason))) => {
                assert!(
                    !reason.trim().is_empty(),
                    "exclusion for `AttachmentReferrers::{method}` carries no reason"
                );
                excluded += 1;
            }
            (true, Some(_)) => stale_exclusions.push(format!("AttachmentReferrers::{method}")),
            (false, None) => missing.push(format!("AttachmentReferrers::{method}")),
        }
    }

    for method in fallible_trait_methods(CONTROL_INTENT_SOURCE, "ControlIntentStore") {
        if harness_drives(&sources, &method) {
            covered += 1;
        } else {
            missing.push(format!("ControlIntentStore::{method}"));
        }
    }

    for (source, trait_name) in [
        (OBLIGATION_SOURCE, "ObligationLedger"),
        (RECOVERY_LEADER_SOURCE, "RecoveryLeaderStore"),
        (SESSION_DELETE_SOURCE, "SessionDeleteLedger"),
        (GENERATION_DRAIN_SOURCE, "GenerationDrainStore"),
    ] {
        for method in fallible_trait_methods(source, trait_name) {
            if harness_drives(&sources, &method) {
                covered += 1;
            } else {
                missing.push(format!("{trait_name}::{method}"));
            }
        }
    }

    // The attachment blob store's method names (`put`, `get`, `list`, ...) are
    // too generic to detect by call site, so its surface is gated by requiring
    // a reason for every declared fallible method instead.
    for method in fallible_trait_methods(ATTACHMENT_STORE_SOURCE, "AttachmentStore") {
        match ATTACHMENT_STORE_EXCLUSIONS
            .iter()
            .find(|(name, _)| *name == method)
        {
            Some((_, reason)) => {
                assert!(
                    !reason.trim().is_empty(),
                    "exclusion for `AttachmentStore::{method}` carries no reason"
                );
                excluded += 1;
            }
            None => missing.push(format!("AttachmentStore::{method}")),
        }
    }

    assert!(
        missing.is_empty(),
        "fallible store-trait methods are in neither the differential's operation inventory \
         nor its exclusion list: {missing:?}. Drive the method from the harness, or add it to \
         SESSION_STORE_EXCLUSIONS / ATTACHMENT_STORE_EXCLUSIONS with the suite that owns it."
    );
    assert!(
        stale_exclusions.is_empty(),
        "these methods are excluded but the harness now drives them; delete their exclusions: \
         {stale_exclusions:?}"
    );
    // A floor, not a pin: covering more methods must never fail the gate, but
    // silently dropping drivers until the inventory is a token sample must.
    assert!(
        covered >= 39,
        "the differential drives only {covered} fallible store-trait methods; \
         the inventory has been narrowed"
    );
    assert_eq!(
        excluded,
        SESSION_STORE_EXCLUSIONS.len()
            + ATTACHMENT_STORE_EXCLUSIONS.len()
            + ATTACHMENT_REFERRERS_EXCLUSIONS.len(),
        "every exclusion must name a method the gated traits still declare"
    );
}
