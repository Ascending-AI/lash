use chrono::{TimeZone, Utc};
use lash_sansio::{ExprSlot, ProcessId, SessionId, TurnId, WorkflowSitePath};

use super::*;
use crate::{TraceBranchSelection, TraceContext, TraceLanguageExecution, WorkflowDocumentEntry};

fn identity() -> LanguageIdentity {
    LanguageIdentity {
        scope: TraceRuntimeScope {
            session_id: Some(SessionId::from("session-1")),
            turn_id: Some(TurnId::from("turn-1")),
            turn_index: Some(0),
            protocol_iteration: Some(0),
        },
        subject: TraceRuntimeSubject::Effect {
            address: lash_sansio::EffectAddress::new(
                lash_sansio::ExecutionScope::turn("session-1", "turn-1"),
                "exec-replay-1",
            )
            .expect("valid trace test effect address"),
            effect_id: "exec-1".to_string(),
        },
        document: crate::WorkflowDocumentRef {
            source_identity: "source-1".to_string(),
            module_ref: lash_sansio::ModuleRef::new(&lash_sansio::ContentHash::new("module-1")),
            entry: crate::WorkflowDocumentEntry::Main,
            ir_version: 1,
        },
        entry_name: "main".to_string(),
        engine_execution_id: None,
        generation: None,
    }
}

fn process_identity(process: &ProcessId) -> LanguageIdentity {
    LanguageIdentity {
        scope: TraceRuntimeScope::none(),
        subject: TraceRuntimeSubject::Process {
            process_id: process.clone(),
        },
        entry_name: "worker".to_string(),
        ..identity()
    }
}

fn reference() -> WorkflowDocumentRef {
    WorkflowDocumentRef {
        source_identity: "source-1".to_string(),
        module_ref: lash_sansio::ModuleRef::new(&lash_sansio::ContentHash::new("module-1")),
        entry: WorkflowDocumentEntry::Main,
        ir_version: 1,
    }
}

fn site(node: &str) -> WorkflowSiteRef {
    WorkflowSiteRef::node(node)
}

/// The fixture document: a branch, a call in each arm, and a statement with
/// two calls.
fn document() -> WorkflowOverlayDocument {
    WorkflowOverlayDocument::new(
        reference(),
        [
            site("branch"),
            site("then"),
            site("else"),
            arg_site("pair", 0),
            arg_site("pair", 1),
        ],
    )
}

fn arg_site(node: &str, arg: u32) -> WorkflowSiteRef {
    WorkflowSiteRef::new(node, WorkflowSitePath::slots([ExprSlot::Arg(arg)]))
}

fn context(site: &WorkflowSiteRef) -> lash_sansio::WorkflowOccurrenceContext {
    lash_sansio::WorkflowOccurrenceContext {
        site_path: site.site_path.clone(),
        loops: Vec::new(),
    }
}

fn at(ms: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_millis_opt(ms).single().expect("timestamp")
}

fn language(event_key: &str, payload: TraceLanguageExecutionPayload) -> TraceLanguageExecution {
    TraceLanguageExecution {
        event_key: event_key.to_string(),
        identity: identity(),
        payload,
    }
}

fn started_event(event_key: &str) -> TraceLanguageExecution {
    language(event_key, TraceLanguageExecutionPayload::ExecutionStarted)
}

fn started_at(event_key: &str, site: &WorkflowSiteRef, occurrence: u64) -> TraceLanguageExecution {
    language(
        event_key,
        TraceLanguageExecutionPayload::NodeStarted {
            node_id: site.node_id.clone(),
            occurrence,
            call_id: None,
            context: context(site),
        },
    )
}

fn completed_at(
    event_key: &str,
    site: &WorkflowSiteRef,
    occurrence: u64,
) -> TraceLanguageExecution {
    language(
        event_key,
        TraceLanguageExecutionPayload::NodeCompleted {
            node_id: site.node_id.clone(),
            occurrence,
            call_id: None,
            context: context(site),
        },
    )
}

fn node_started(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
    started_at(event_key, &site("branch"), occurrence)
}

fn node_completed(event_key: &str, occurrence: u64) -> TraceLanguageExecution {
    completed_at(event_key, &site("branch"), occurrence)
}

fn node_failed(event_key: &str, occurrence: u64, error: &str) -> TraceLanguageExecution {
    language(
        event_key,
        TraceLanguageExecutionPayload::NodeFailed {
            node_id: "branch".to_string(),
            occurrence,
            call_id: None,
            failure: TraceLanguageExecutionFailure::Runtime {
                code: "test_failure".to_string(),
                message: error.to_string(),
            },
            context: Default::default(),
        },
    )
}

fn node_waiting(
    event_key: &str,
    occurrence: u64,
    awaited: crate::TraceNodeAwaited,
) -> TraceLanguageExecution {
    language(
        event_key,
        TraceLanguageExecutionPayload::NodeWaiting {
            node_id: "branch".to_string(),
            occurrence,
            awaited,
            context: Default::default(),
        },
    )
}

fn execution_finished(event_key: &str, status: LanguageExecutionStatus) -> TraceLanguageExecution {
    language(
        event_key,
        TraceLanguageExecutionPayload::ExecutionFinished {
            status,
            error: None,
        },
    )
}

fn branch_selected(occurrence: u64, selected: TraceBranchSelection) -> TraceLanguageExecution {
    language(
        "branch",
        TraceLanguageExecutionPayload::BranchSelected {
            node_id: "branch".to_string(),
            occurrence,
            context: Default::default(),
            selected,
        },
    )
}

fn fixture_record(event: TraceEvent, ms: i64) -> TraceRecord {
    TraceRecord {
        schema_version: TRACE_SCHEMA_VERSION,
        id: format!("record-{ms}"),
        timestamp: at(ms),
        content: crate::TelemetryContent::Captured,
        context: TraceContext::default().for_session("session-1"),
        event,
    }
}

fn record_at(event: TraceLanguageExecution, ms: i64) -> TraceRecord {
    fixture_record(
        TraceEvent::LanguageExecution {
            language: Some("lashvm".to_string()),
            event,
        },
        ms,
    )
}

fn step_body(process: &ProcessId, occurrence: u64, call: &str, attempt: u32) -> StepBodyStarted {
    StepBodyStarted {
        process_id: process.clone(),
        node_id: "then".to_string(),
        occurrence,
        context: Default::default(),
        call_id: lash_sansio::ToolCallId::fixture(call),
        attempt,
    }
}

fn step_record(step: StepBodyStarted, ms: i64) -> TraceRecord {
    fixture_record(TraceEvent::StepBodyStarted { step }, ms)
}

fn in_process(mut event: TraceLanguageExecution, process: &ProcessId) -> TraceLanguageExecution {
    event.identity = process_identity(process);
    event
}

fn fold(
    previous: Option<&WorkflowExecutionOverlay>,
    records: &[TraceRecord],
) -> Result<WorkflowExecutionOverlay, WorkflowOverlayFoldError> {
    fold_workflow_overlay(
        previous,
        None,
        records,
        DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT,
    )
}

fn state<'a>(
    overlay: &'a WorkflowExecutionOverlay,
    site: &WorkflowSiteRef,
) -> &'a WorkflowOverlaySite {
    overlay
        .sites
        .iter()
        .find(|state| state.site == *site)
        .unwrap_or_else(|| panic!("no state for {site}"))
}

fn test_permutations(records: &[TraceRecord]) -> Vec<Vec<TraceRecord>> {
    if records.len() <= 1 {
        return vec![records.to_vec()];
    }
    let mut permutations = Vec::new();
    for index in 0..records.len() {
        let mut rest = records.to_vec();
        let head = rest.remove(index);
        for mut tail in test_permutations(&rest) {
            let mut permutation = vec![head.clone()];
            permutation.append(&mut tail);
            permutations.push(permutation);
        }
    }
    permutations
}

/// Every order and every two-way partition of `records` folds to the same
/// bytes, through the pure fold and through the accumulator.
fn assert_fold_law(
    document: Option<&WorkflowOverlayDocument>,
    records: &[TraceRecord],
    history_limit: usize,
) -> WorkflowExecutionOverlay {
    let batch = fold_workflow_overlay(None, document, records, history_limit).expect("batch fold");
    let bytes = serde_json::to_vec(&batch).expect("serialize batch");
    for ordered in test_permutations(records) {
        let permuted =
            fold_workflow_overlay(None, document, &ordered, history_limit).expect("permuted fold");
        assert_eq!(serde_json::to_vec(&permuted).expect("serialize"), bytes);
        for split in 1..ordered.len() {
            let first = fold_workflow_overlay(None, document, &ordered[..split], history_limit)
                .expect("first partition");
            let second =
                fold_workflow_overlay(Some(&first), document, &ordered[split..], history_limit)
                    .expect("second partition");
            assert_eq!(serde_json::to_vec(&second).expect("serialize"), bytes);
        }
        let mut accumulator = WorkflowExecutionOverlayAccumulator::new(history_limit);
        if let Some(document) = document {
            accumulator.set_document(document.clone());
        }
        for record in &ordered {
            accumulator
                .fold(std::slice::from_ref(record))
                .expect("accumulate");
        }
        assert_eq!(accumulator.snapshot().as_ref(), Some(&batch));
    }
    batch
}

/// The fold is a function of the set of observations: arrival order,
/// partitioning and the accumulator's index change nothing, with and without
/// the document.
#[test]
fn every_permutation_and_partition_folds_to_the_same_overlay() {
    let records = [
        record_at(started_event("seed"), 900),
        record_at(node_started("start", 1), 1_000),
        record_at(branch_selected(1, TraceBranchSelection::Then), 1_100),
        record_at(completed_at("then", &site("then"), 1), 1_250),
    ];
    let document = document();
    let without = assert_fold_law(None, &records, DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT);
    let with = assert_fold_law(
        Some(&document),
        &records,
        DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT,
    );
    assert_eq!(with.sites, without.sites);
    assert!(with.coverage.is_complete());
    assert!(!without.coverage.is_complete());
}

/// FIG-5576: an observer that attaches after the execution started loads the
/// document the execution names and is told its coverage is incomplete; the
/// start, when it is replayed, completes the coverage and changes no
/// observation.
#[test]
fn a_late_attach_without_the_start_marks_incomplete_coverage_over_the_loaded_document() {
    let document = document();
    let records = [
        record_at(node_started("start", 1), 1_000),
        record_at(node_completed("complete", 1), 1_250),
    ];
    let late = fold_workflow_overlay(None, Some(&document), &records, 8).expect("late attach");
    assert_eq!(late.document.as_ref(), Some(document.reference()));
    assert_eq!(
        late.coverage,
        WorkflowOverlayCoverage {
            document_loaded: true,
            start_observed: false,
        }
    );
    assert!(late.mismatches.is_empty());
    assert_eq!(
        late.sites.len(),
        1,
        "a site with no observation is the document's to list, not the overlay's"
    );

    let seeded = fold_workflow_overlay(
        Some(&late),
        Some(&document),
        &[record_at(started_event("seed"), 900)],
        8,
    )
    .expect("replayed start");
    assert!(seeded.coverage.is_complete());
    assert_eq!(seeded.sites, late.sites);
    assert!(matches!(
        state(&seeded, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Completed {
            duration_ms: Some(250),
            ..
        }
    ));

    let unloaded = fold(None, &records).expect("no document");
    assert_eq!(unloaded.document, None);
    assert!(!unloaded.coverage.document_loaded);
}

/// FIG-5576: a site outside the claimed document is a typed mismatch and is
/// never grafted into the overlay, whether the document was there first or
/// arrived after the observation; a start that names another document is
/// reported too.
#[test]
fn a_site_outside_the_document_is_a_typed_mismatch_and_never_a_site() {
    let document = document();
    let stray = site("another-document");
    let mut other_start = started_event("seed");
    let claimed = &mut other_start.identity.document;
    claimed.source_identity = "source-2".to_string();
    let claimed = claimed.clone();
    let records = [
        record_at(other_start, 900),
        record_at(node_started("start", 1), 1_000),
        record_at(started_at("stray-start", &stray, 1), 1_100),
        record_at(completed_at("stray-end", &stray, 1), 1_200),
    ];
    let overlay = assert_fold_law(Some(&document), &records, 8);
    assert_eq!(
        overlay.mismatches,
        vec![
            WorkflowOverlayMismatch::Document { claimed },
            WorkflowOverlayMismatch::SiteOutsideDocument {
                site: stray.clone()
            },
        ]
    );
    assert!(overlay.sites.iter().all(|state| state.site != stray));
    assert!(
        overlay
            .history
            .iter()
            .all(|item| item.identity.site.as_ref() != Some(&stray))
    );
    assert!(overlay.coverage.start_observed, "the execution did start");

    let unloaded = fold(None, &records).expect("no document");
    assert!(unloaded.sites.iter().any(|state| state.site == stray));
    let loaded_late =
        fold_workflow_overlay(Some(&unloaded), Some(&document), &[], 8).expect("late document");
    assert_eq!(loaded_late, overlay);

    let mut accumulator = WorkflowExecutionOverlayAccumulator::new(8);
    accumulator.fold(&records).expect("accumulate");
    accumulator.set_document(document);
    assert_eq!(accumulator.snapshot(), Some(overlay));
}

/// More mismatches than the overlay lists are counted as truncated, and the
/// ones it keeps do not depend on arrival order.
#[test]
fn mismatches_are_bounded_and_canonical() {
    let document = document();
    let records = (0..MISMATCH_LIMIT as i64 + 3)
        .map(|index| {
            record_at(
                started_at("stray", &site(&format!("stray-{index:03}")), 1),
                index,
            )
        })
        .collect::<Vec<_>>();
    let forward = fold_workflow_overlay(None, Some(&document), &records, 8).expect("forward");
    let reversed = records.iter().rev().cloned().collect::<Vec<_>>();
    let backward = fold_workflow_overlay(None, Some(&document), &reversed, 8).expect("backward");
    assert_eq!(forward.mismatches.len(), MISMATCH_LIMIT);
    assert!(forward.mismatches_truncated);
    assert_eq!(forward.mismatches, backward.mismatches);
}

/// FIG-5575: two sites of one node are two overlay sites. Each counts its
/// own occurrences, and neither's state stands for the other's.
#[test]
fn two_sites_of_one_node_keep_separate_occurrence_states() {
    let (first, second) = (arg_site("pair", 0), arg_site("pair", 1));
    let records = [
        record_at(started_at("a", &first, 1), 1_000),
        record_at(completed_at("b", &first, 1), 1_100),
        record_at(started_at("c", &second, 1), 1_200),
    ];
    let overlay = assert_fold_law(Some(&document()), &records, 8);
    assert!(matches!(
        state(&overlay, &first).occurrence,
        WorkflowOverlayOccurrence::Completed { occurrence: 1, .. }
    ));
    assert!(matches!(
        state(&overlay, &second).occurrence,
        WorkflowOverlayOccurrence::Running { occurrence: 1, .. }
    ));
    assert!(overlay.conflicts.is_empty());
}

/// A branch selection names the typed arm at its site and occurrence, and is
/// a fact distinct from the branch's own terminal. Which nodes the other arm
/// holds is the document's to say: the overlay marks none of them.
#[test]
fn a_branch_selection_records_the_typed_arm_at_its_site() {
    let selected = fold(
        None,
        &[record_at(
            branch_selected(1, TraceBranchSelection::Else),
            1_000,
        )],
    )
    .expect("selection");
    let branch = state(&selected, &site("branch"));
    assert_eq!(branch.branch, Some(TraceBranchSelection::Else));
    assert!(matches!(
        branch.occurrence,
        WorkflowOverlayOccurrence::Completed {
            occurrence: 1,
            start: None,
            ..
        }
    ));
    assert_eq!(
        selected.sites.len(),
        1,
        "no arm node is invented or skipped"
    );

    let failed = fold(
        Some(&selected),
        &[record_at(node_failed("failed", 1, "boom"), 1_100)],
    )
    .expect("explicit terminal");
    let branch = state(&failed, &site("branch"));
    assert_eq!(branch.branch, Some(TraceBranchSelection::Else));
    assert!(matches!(
        branch.occurrence,
        WorkflowOverlayOccurrence::Failed { .. }
    ));
    assert_eq!(failed.history.len(), 2);

    let reselected = fold(
        Some(&failed),
        &[record_at(
            branch_selected(2, TraceBranchSelection::Then),
            1_200,
        )],
    )
    .expect("second iteration");
    assert_eq!(
        state(&reselected, &site("branch")).branch,
        Some(TraceBranchSelection::Then),
        "the site shows its latest choice; each choice stays in the history"
    );
}

/// FIG-5576: the body start of an admitted step binds its site occurrence to
/// the call, and a retried body keeps the occurrence and the call while its
/// attempt advances. Nothing but that fact binds a call: a start the VM
/// reported for a step that was refused shows no admitted call.
#[test]
fn a_step_body_start_binds_its_occurrence_to_the_call_and_a_retry_keeps_both() {
    let process = ProcessId::fixture("worker");
    let then = site("then");
    let first = step_record(step_body(&process, 1, "call-1", 1), 1_000);
    let retried = step_record(step_body(&process, 1, "call-1", 2), 2_000);
    let completed = record_at(
        in_process(
            language(
                "done",
                TraceLanguageExecutionPayload::NodeCompleted {
                    node_id: "then".to_string(),
                    occurrence: 1,
                    call_id: Some(lash_sansio::ToolCallId::fixture("call-1")),
                    context: Default::default(),
                },
            ),
            &process,
        ),
        3_000,
    );
    let overlay = assert_fold_law(None, &[first.clone(), retried, completed], 8);
    assert_eq!(
        overlay.subject,
        TraceRuntimeSubject::Process {
            process_id: process.clone()
        }
    );
    let bound = state(&overlay, &then);
    assert_eq!(
        bound.call,
        Some(WorkflowOverlayCall {
            occurrence: 1,
            call_id: lash_sansio::ToolCallId::fixture("call-1"),
            attempt: Some(2),
        })
    );
    assert_eq!(bound.summary.retained_occurrences, 1);
    assert_eq!(bound.summary.started_count, 1);
    assert!(matches!(
        bound.occurrence,
        WorkflowOverlayOccurrence::Completed {
            occurrence: 1,
            duration_ms: Some(2_000),
            ..
        }
    ));
    assert!(overlay.conflicts.is_empty(), "two attempts are two facts");

    let only_started = fold(None, std::slice::from_ref(&first)).expect("body start alone");
    assert!(matches!(
        state(&only_started, &then).occurrence,
        WorkflowOverlayOccurrence::Running { occurrence: 1, .. }
    ));

    let refused = fold(
        None,
        &[
            record_at(
                in_process(started_at("vm-start", &then, 1), &process),
                1_000,
            ),
            record_at(
                in_process(node_failed("refused", 1, "refused"), &process),
                1_100,
            ),
        ],
    )
    .expect("refused step");
    assert!(
        refused.sites.iter().all(|state| state.call.is_none()),
        "a refused step has no admitted call"
    );

    let other = step_record(
        step_body(&ProcessId::fixture("other"), 1, "call-9", 1),
        4_000,
    );
    assert!(matches!(
        fold(Some(&overlay), &[other]),
        Err(WorkflowOverlayFoldError::PreviousExecutionMismatch { .. })
    ));
}

/// FIG-5548 and FIG-5576: a committed terminal settles the overlay with no
/// `ExecutionFinished` at all, a provisional finish settles nothing for a
/// process, and a provisional record that arrives after the settlement never
/// reopens it. Cancellation ends only the occurrences observed in flight;
/// any other terminal leaves them incomplete instead of inventing an outcome.
#[test]
fn a_committed_terminal_settles_without_a_finish_and_is_never_reopened() {
    let process = ProcessId::fixture("settled");
    let observe = |accumulator: &mut WorkflowExecutionOverlayAccumulator, ms, event| {
        accumulator
            .observe(&crate::LanguageExecutionObservation {
                language: Some("fixture".into()),
                execution: in_process(event, &process),
                observed_at_ms: ms,
            })
            .expect("observe")
    };
    let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
    observe(&mut accumulator, 0, started_event("seed"));
    observe(&mut accumulator, 1, node_started("start", 1));
    observe(&mut accumulator, 2, completed_at("done", &site("then"), 1));
    observe(
        &mut accumulator,
        3,
        execution_finished("vm-finished", LanguageExecutionStatus::Completed),
    );
    let before = accumulator.snapshot().expect("overlay");
    assert_eq!(
        before.status,
        LanguageExecutionStatus::Running,
        "a process's VM finish is provisional"
    );

    let cancelled = WorkflowOverlaySettlement {
        terminal: WorkflowOverlayTerminal::Cancelled,
        occurred_at: Some(at(10)),
    };
    let undated = WorkflowOverlaySettlement {
        occurred_at: None,
        ..cancelled
    };
    let mut snapshot_only = before.clone();
    snapshot_only.settle(undated);
    assert_eq!(snapshot_only.status, LanguageExecutionStatus::Cancelled);
    assert!(matches!(
        state(&snapshot_only, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Incomplete {
            occurrence: 1,
            settled_at: None,
            terminal: WorkflowOverlayTerminal::Cancelled,
            ..
        }
    ));
    assert_eq!(
        state(&snapshot_only, &site("branch")).summary,
        state(&before, &site("branch")).summary,
        "an unknown time cannot invent a dated terminal record"
    );
    snapshot_only.settle(cancelled);
    accumulator.settle(cancelled);
    accumulator.settle(undated);
    let settled = accumulator.snapshot().expect("settled");
    assert_eq!(
        snapshot_only, settled,
        "a committed time refines an undated one"
    );
    assert!(matches!(
        state(&settled, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Cancelled { occurrence: 1, .. }
    ));
    assert_eq!(
        state(&settled, &site("then")).occurrence,
        state(&before, &site("then")).occurrence,
        "a proven outcome is untouched"
    );

    observe(&mut accumulator, 6, node_started("late", 2));
    let reopened = accumulator.snapshot().expect("late record");
    let partitioned = fold(
        Some(&settled),
        &[record_at(in_process(node_started("late", 2), &process), 6)],
    )
    .expect("late record through the fold");
    assert_eq!(reopened, partitioned);
    assert_eq!(reopened.status, LanguageExecutionStatus::Cancelled);
    assert!(reopened.sites.iter().all(|state| !matches!(
        state.occurrence,
        WorkflowOverlayOccurrence::Running { .. } | WorkflowOverlayOccurrence::Waiting { .. }
    )));

    accumulator.reset_live();
    let reset = accumulator.snapshot().expect("reset");
    assert_eq!(reset.settlement, Some(cancelled));
    assert!(reset.sites.is_empty() && reset.history.is_empty());
    assert!(!reset.coverage.start_observed, "the gap lost the start");

    let mut unobserved = WorkflowExecutionOverlayAccumulator::default();
    unobserved.settle(cancelled);
    assert!(
        unobserved.snapshot().is_none(),
        "settlement never invents an execution"
    );

    for terminal in [
        WorkflowOverlayTerminal::Completed,
        WorkflowOverlayTerminal::Failed,
        WorkflowOverlayTerminal::Abandoned,
    ] {
        let mut overlay = before.clone();
        overlay.settle(WorkflowOverlaySettlement {
            terminal,
            occurred_at: Some(at(10)),
        });
        assert_eq!(overlay.status, terminal.execution_status());
        assert!(matches!(
            state(&overlay, &site("branch")).occurrence,
            WorkflowOverlayOccurrence::Incomplete { occurrence: 1, terminal: category, .. }
                if category == terminal
        ));
    }
}

/// An execution that is not a process's settles on its own finish, exactly:
/// a terminal status beats a conflicting running one in either order, and a
/// start replayed after the finish changes nothing.
#[test]
fn a_cell_execution_settles_on_its_own_finish() {
    let events = [
        record_at(
            execution_finished("terminal", LanguageExecutionStatus::Completed),
            2_000,
        ),
        record_at(
            execution_finished("running", LanguageExecutionStatus::Running),
            1_000,
        ),
    ];
    for ordered in [events.clone(), [events[1].clone(), events[0].clone()]] {
        let overlay = fold(None, &ordered).expect("statuses");
        assert_eq!(overlay.status, LanguageExecutionStatus::Completed);
        assert_eq!(overlay.conflicts.len(), 1);
    }
    for status in [
        LanguageExecutionStatus::Completed,
        LanguageExecutionStatus::Failed,
        LanguageExecutionStatus::Cancelled,
    ] {
        let finished =
            fold(None, &[record_at(execution_finished("f", status), 2_000)]).expect("finished");
        let seeded =
            fold(Some(&finished), &[record_at(started_event("seed"), 1_000)]).expect("late start");
        assert_eq!(seeded.status, status);
    }
}

/// One logical observation delivered twice is one observation, whatever its
/// publisher's key; two different ones under one identity are a typed,
/// bounded conflict.
#[test]
fn an_identical_duplicate_is_a_noop_and_a_conflicting_one_is_typed() {
    let start = record_at(node_started("publisher-a", 1), 1_000);
    let duplicate = record_at(node_started("publisher-b", 1), 1_000);
    let deduplicated = fold(None, &[start.clone(), duplicate]).expect("duplicate");
    assert_eq!(deduplicated.history.len(), 1);
    assert!(deduplicated.conflicts.is_empty());

    let conflict = fold(
        None,
        &[start, record_at(node_started("publisher-c", 1), 1_001)],
    )
    .expect("conflict");
    assert_eq!(conflict.conflicts.len(), 1);
    assert_eq!(
        conflict.conflicts[0].kind,
        WorkflowOverlayConflictKind::ConflictingDuplicate
    );
    assert_eq!(conflict.conflicts[0].variants.len(), 2);
}

/// A terminal occurrence is never downgraded by its late start, a later
/// occurrence is what the site shows, and an occurrence that failed is one
/// execution's alone: the same site and occurrence in another attempt or
/// process is another overlay.
#[test]
fn a_terminal_never_downgrades_and_the_next_occurrence_is_visible() {
    let overlay = fold(
        None,
        &[
            record_at(node_completed("complete-1", 1), 1_250),
            record_at(node_started("late-start-1", 1), 1_000),
            record_at(node_started("start-2", 2), 2_000),
        ],
    )
    .expect("occurrences");
    let branch = state(&overlay, &site("branch"));
    assert!(matches!(
        branch.occurrence,
        WorkflowOverlayOccurrence::Running { occurrence: 2, .. }
    ));
    assert_eq!(branch.summary.retained_occurrences, 2);
    assert_eq!(branch.summary.terminal_count, 1);

    let failure = |attempt, process: &str, message: &str| {
        let mut event = node_failed("same-publication-key", 1, message);
        event.identity.subject = TraceRuntimeSubject::Process {
            process_id: ProcessId::fixture(process),
        };
        event.identity.generation = Some(crate::TraceLanguageExecutionGeneration::new(attempt));
        event
    };
    let first = record_at(failure(1, "worker-a", "first attempt"), 1_000);
    let folded = fold(None, std::slice::from_ref(&first)).expect("first attempt");
    assert_eq!(
        folded.execution_key,
        format!("process:{}:attempt:1", ProcessId::fixture("worker-a"))
    );
    for other in [
        failure(2, "worker-a", "retried segment"),
        failure(1, "worker-b", "another process"),
    ] {
        let other = record_at(other, 2_000);
        assert!(matches!(
            fold(Some(&folded), std::slice::from_ref(&other)),
            Err(WorkflowOverlayFoldError::PreviousExecutionMismatch { .. })
        ));
        assert!(matches!(
            fold(None, &[first.clone(), other]),
            Err(WorkflowOverlayFoldError::MixedExecutions { .. })
        ));
    }
}

/// History is bounded per site with a canonical watermark: an evicted
/// occurrence keeps its terminal in the site's summary, another site's
/// progress is untouched, and what arrives late at or below the watermark
/// folds to what a batch fold retains.
#[test]
fn per_site_history_is_bounded_with_a_canonical_watermark() {
    let (a, z) = (site("then"), site("else"));
    let records = [
        record_at(started_at("a-start", &a, 1), 1_000),
        record_at(completed_at("a-end", &a, 1), 1_100),
        record_at(started_at("z-one", &z, 1), 2_000),
        record_at(started_at("z-two", &z, 2), 3_000),
        record_at(started_at("a-two", &a, 2), 4_000),
        record_at(started_event("seed"), 900),
    ];
    let overlay = assert_fold_law(Some(&document()), &records, 1);
    let state_a = state(&overlay, &a);
    assert_eq!(state_a.summary.terminal_count, 1);
    assert_eq!(state_a.summary.retained_occurrences, 2);
    assert!(matches!(
        state_a.occurrence,
        WorkflowOverlayOccurrence::Running { occurrence: 2, .. }
    ));
    for site in [&a, &z] {
        assert!(overlay.history.iter().any(|item| {
            item.identity.site.as_ref() == Some(site) && item.identity.occurrence == Some(2)
        }));
        assert!(overlay.history.iter().all(|item| {
            item.identity.site.as_ref() != Some(site) || item.identity.occurrence != Some(1)
        }));
    }
    assert_eq!(
        overlay
            .retention
            .iter()
            .map(|retention| (&retention.site, retention.truncation_watermark))
            .collect::<Vec<_>>(),
        vec![(&z, 1), (&a, 1)]
    );
    assert!(
        overlay.coverage.start_observed,
        "the start is never evicted"
    );
}

/// The default limit keeps the latest occurrence and counts every one.
#[test]
fn the_default_limit_retains_the_latest_occurrence_and_summary() {
    let limit = DEFAULT_WORKFLOW_OVERLAY_HISTORY_LIMIT as u64;
    let records = (1..=limit + 1)
        .map(|occurrence| record_at(node_started("start", occurrence), occurrence as i64))
        .collect::<Vec<_>>();
    let overlay = fold(None, &records).expect("default limit");
    let branch = state(&overlay, &site("branch"));
    assert_eq!(branch.summary.retained_occurrences, limit + 1);
    assert!(matches!(
        branch.occurrence,
        WorkflowOverlayOccurrence::Running { occurrence, .. } if occurrence == limit + 1
    ));
    assert_eq!(overlay.retention[0].truncation_watermark, 1);
}

/// One append clones only the occurrence it evicts, however many the site
/// retains.
#[test]
fn appending_an_observation_clones_only_the_evicted_occurrence() {
    for retained in [16, 256] {
        let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
        let mut append = |event, timestamp| {
            accumulator
                .fold(std::slice::from_ref(&record_at(event, timestamp)))
                .expect("fold");
        };
        append(started_event("seed"), 0);
        for occurrence in 1..=retained {
            append(node_started("start", occurrence), occurrence as i64);
            append(node_completed("end", occurrence), occurrence as i64 + 1);
        }
        model::HISTORY_CLONES.with(|count| count.set(0));
        append(node_started("next", retained + 1), retained as i64 + 2);
        let clones = model::HISTORY_CLONES.with(std::cell::Cell::get);
        assert!(
            clones <= 4,
            "one append cloned {clones} history events with {retained} retained occurrences"
        );
    }
}

/// A wait is dated by its record, names what it awaits, and is resolved only
/// for its own occurrence; a completion dominates it in every order.
#[test]
fn a_wait_keeps_its_awaited_identity_and_resolves_only_its_own_occurrence() {
    let sleep = crate::TraceNodeAwaited::Sleep {
        deadline_ms: Some(9_000),
    };
    let signal = crate::TraceNodeAwaited::Signal {
        name: "approval".into(),
        key: "key-2".into(),
    };
    let resumed = |occurrence| {
        language(
            "resumed",
            TraceLanguageExecutionPayload::NodeResumed {
                node_id: "branch".to_string(),
                occurrence,
                context: Default::default(),
                resolution: crate::TraceNodeWaitResolution::Resumed,
            },
        )
    };
    let waiting = [
        record_at(node_started("start", 1), 1_000),
        record_at(node_waiting("wait", 1, sleep.clone()), 2_000),
    ];
    let parked = fold(None, &waiting).expect("parked");
    assert_eq!(
        state(&parked, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Waiting {
            occurrence: 1,
            start: Some(at(1_000)),
            since: at(2_000),
            awaited: sleep.clone(),
        }
    );

    let mut completed = waiting.to_vec();
    completed.push(record_at(node_completed("done", 1), 3_000));
    let overlay = assert_fold_law(None, &completed, 8);
    assert!(matches!(
        state(&overlay, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Completed {
            duration_ms: Some(2_000),
            ..
        }
    ));

    let two = fold(
        None,
        &[
            record_at(node_waiting("wait-1", 1, sleep), 2_000),
            record_at(node_waiting("wait-2", 2, signal.clone()), 2_500),
            record_at(resumed(1), 3_000),
        ],
    )
    .expect("two waits");
    assert_eq!(
        state(&two, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Waiting {
            occurrence: 2,
            start: None,
            since: at(2_500),
            awaited: signal,
        },
        "resolving occurrence 1 leaves occurrence 2 parked"
    );
    let resolved = fold(Some(&two), &[record_at(resumed(2), 3_500)]).expect("resolved");
    assert_eq!(
        state(&resolved, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Unobserved,
        "a resumed wait with no observed start is not running"
    );
}

/// A cancellation ends only an occurrence that was observed in flight.
#[test]
fn a_cancellation_changes_only_an_observed_in_flight_occurrence() {
    let cancelled = |occurrence| {
        language(
            "cancelled",
            TraceLanguageExecutionPayload::NodeCancelled {
                node_id: "branch".to_string(),
                occurrence,
                context: Default::default(),
            },
        )
    };
    let unobserved = fold(None, &[record_at(cancelled(1), 2_000)]).expect("cancel alone");
    assert_eq!(
        state(&unobserved, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Unobserved
    );
    assert_eq!(
        state(&unobserved, &site("branch")).summary.terminal_count,
        0
    );

    let records = [
        record_at(node_started("start", 1), 1_000),
        record_at(cancelled(1), 2_000),
    ];
    let overlay = assert_fold_law(None, &records, 8);
    assert_eq!(
        state(&overlay, &site("branch")).occurrence,
        WorkflowOverlayOccurrence::Cancelled {
            occurrence: 1,
            start: Some(at(1_000)),
            end: at(2_000),
        }
    );
}

/// A child link names the parent's exact site and the child's execution, and
/// is not an occurrence of the site.
#[test]
fn a_child_link_names_the_parent_site_and_the_child_execution() {
    let child = ProcessId::fixture("child");
    let overlay = fold(
        None,
        &[record_at(
            language(
                "child",
                TraceLanguageExecutionPayload::ChildStarted {
                    parent_node_id: "then".to_string(),
                    occurrence: 1,
                    context: Default::default(),
                    child: crate::TraceLanguageChildExecution {
                        scope: TraceRuntimeScope::none(),
                        process_id: child.clone(),
                        attempt: Some(1),
                        document: Some(WorkflowDocumentRef {
                            entry: WorkflowDocumentEntry::Process {
                                process_ref: "0:1".to_owned(),
                            },
                            ..reference()
                        }),
                    },
                },
            ),
            1_000,
        )],
    )
    .expect("child");
    assert_eq!(overlay.children.len(), 1);
    let link = &overlay.children[0];
    assert_eq!(link.parent_execution_key, overlay.execution_key);
    assert_eq!(link.parent_site, site("then"));
    assert_eq!(
        link.document.as_ref().map(|document| &document.entry),
        Some(&WorkflowDocumentEntry::Process {
            process_ref: "0:1".to_owned()
        })
    );
    assert_eq!(
        link.child_execution_key,
        Some(format!("process:{child}:attempt:1"))
    );
    let parent = state(&overlay, &site("then"));
    assert_eq!(parent.occurrence, WorkflowOverlayOccurrence::Unobserved);
    assert_eq!(parent.summary.retained_occurrences, 0);
}

/// A fold needs an observation, a limit and one execution; a refused batch
/// leaves the accumulator as it was.
#[test]
fn a_fold_refuses_an_empty_unbounded_or_mixed_input_without_mutation() {
    assert_eq!(
        fold(None, &[]),
        Err(WorkflowOverlayFoldError::NoExecutionObservations)
    );
    assert_eq!(
        fold_workflow_overlay(None, None, &[record_at(started_event("seed"), 0)], 0),
        Err(WorkflowOverlayFoldError::ZeroHistoryLimit)
    );
    let process = ProcessId::fixture("other");
    let foreign = record_at(in_process(node_started("foreign", 1), &process), 2_000);
    let own = record_at(node_started("own", 1), 1_000);
    let mut accumulator = WorkflowExecutionOverlayAccumulator::default();
    assert!(matches!(
        accumulator.fold(&[own.clone(), foreign.clone()]),
        Err(WorkflowOverlayFoldError::MixedExecutions { .. })
    ));
    assert!(accumulator.snapshot().is_none());
    accumulator.fold(std::slice::from_ref(&own)).expect("own");
    let before = accumulator.snapshot();
    assert!(matches!(
        accumulator.fold(&[record_at(node_completed("done", 1), 1_500), foreign]),
        Err(WorkflowOverlayFoldError::PreviousExecutionMismatch { .. })
    ));
    assert_eq!(accumulator.snapshot(), before);
    assert_eq!(
        accumulator.observe(&crate::LanguageExecutionObservation {
            language: Some("fixture".into()),
            execution: node_completed("done", 1),
            observed_at_ms: u64::MAX,
        }),
        Err(WorkflowOverlayFoldError::InvalidObservationTimestamp {
            observed_at_ms: u64::MAX
        })
    );
}

/// A stored overlay is decoded only at the schema version it was written
/// under, and the version is checked before its shape.
#[test]
fn an_overlay_decodes_at_its_own_schema_version_and_round_trips() {
    let overlay = fold_workflow_overlay(
        None,
        Some(&document()),
        &[
            record_at(started_event("seed"), 900),
            record_at(node_started("start", 1), 1_000),
            record_at(branch_selected(1, TraceBranchSelection::Then), 1_100),
        ],
        8,
    )
    .expect("overlay");
    let mut value = serde_json::to_value(&overlay).expect("serialize");
    assert_eq!(
        serde_json::from_value::<WorkflowExecutionOverlay>(value.clone()).expect("round trip"),
        overlay
    );
    value["schema_version"] = serde_json::json!(TRACE_SCHEMA_VERSION + 1);
    value["sites"] = serde_json::json!("not a list");
    let error = serde_json::from_value::<WorkflowExecutionOverlay>(value)
        .expect_err("another version is refused");
    assert!(
        error
            .to_string()
            .contains("unsupported trace schema version"),
        "{error}"
    );
}
