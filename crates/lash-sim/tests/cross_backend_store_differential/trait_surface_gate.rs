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
const SESSION_STORE_SOURCE: &str = include_str!("../../../lash-core/src/store/mod.rs");
const ATTACHMENT_STORE_SOURCE: &str = include_str!("../../../lash-core/src/attachments.rs");

/// Every source file that makes up this test binary. A method counts as
/// covered when the harness calls it from one of these.
const HARNESS_SOURCES: &[&str] = &[
    include_str!("../cross_backend_store_differential.rs"),
    include_str!("checkpoint_cases.rs"),
    include_str!("coalesced_batch_oracles.rs"),
    include_str!("corrupt_input_cases.rs"),
    include_str!("fork_cases.rs"),
    include_str!("generated_surface.rs"),
    include_str!("observations.rs"),
    include_str!("plugin_state_case.rs"),
    include_str!("raw_durable_reader.rs"),
    include_str!("residue.rs"),
    include_str!("session_meta_layout.rs"),
    include_str!("surface_sweep.rs"),
];

/// The gated store traits. `RuntimePersistence` is the blanket alias over the
/// first five, so covering them covers the whole runtime-store surface.
const GATED_SESSION_TRAITS: &[&str] = &[
    "SessionCommitStore",
    "TurnInputStore",
    "SessionExecutionLeaseStore",
    "QueuedWorkStore",
    "StoreMaintenance",
];

/// Fallible session-store methods the harness deliberately does not drive.
///
/// Every entry is a method this differential cannot reach with the fixture it
/// builds, together with the suite that does own it. Removing a method from
/// both this list and the harness fails `store_trait_surface_is_fully_gated`.
const SESSION_STORE_EXCLUSIONS: &[(&str, &str)] = &[
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
        "repair_orphaned_active_turn_inputs",
        "repair is authorized by a turn-cancellation intent snapshot this fixture cannot \
         produce without a TurnCancellationAuthority; owned by the turn_control conformance suite",
    ),
];

/// Fallible attachment (blob artifact) store methods.
///
/// The three session stores this harness compares hold no attachment bytes:
/// their durable surface is the attachment *manifest* row, which the
/// residue digest already covers. The blob store itself is compared by
/// `attachment_blob_store_differential_agrees` in the same crate, which runs
/// the same three-backend comparison over file, S3, and in-memory blob stores.
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

/// Extract the fallible method names declared directly in `trait_name`.
///
/// A method is fallible when its signature (everything before the body brace
/// or the trailing semicolon) returns `Result` or the maintenance alias.
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

/// True when some harness source calls `method` on a store handle.
fn harness_drives(method: &str) -> bool {
    let call = format!(".{method}(");
    HARNESS_SOURCES.iter().any(|source| source.contains(&call))
}

#[test]
fn store_trait_surface_is_fully_gated() {
    let mut missing = Vec::new();
    let mut stale_exclusions = Vec::new();
    let mut covered = 0usize;
    let mut excluded = 0usize;

    for trait_name in GATED_SESSION_TRAITS {
        for method in fallible_trait_methods(SESSION_STORE_SOURCE, trait_name) {
            let exclusion = SESSION_STORE_EXCLUSIONS
                .iter()
                .find(|(name, _)| *name == method);
            let driven = harness_drives(&method);
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
        covered >= 34,
        "the differential drives only {covered} fallible store-trait methods; \
         the inventory has been narrowed"
    );
    assert_eq!(
        excluded,
        SESSION_STORE_EXCLUSIONS.len() + ATTACHMENT_STORE_EXCLUSIONS.len(),
        "every exclusion must name a method the gated traits still declare"
    );
}
