// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::EffectSummaryWriter;
use super::segment_state::ReplayOrdinalsState;
use super::{
    EXECUTION_BOUND_EXHAUSTION_LOUD, LASHLANG_SEGMENT_STATE_VERSION, LashlangProcessExecutionTrace,
    LashlangProcessTraceIdentity, LashlangSegmentState, LashlangSegmentStateError,
    decode_lashlang_segment_state, process_lashlang_execution_result, process_trace_session_id,
    refuse_foreign_program,
};
use lash_sansio::ExecutionNodeKind;
use lash_sansio::sync::MutexExt;
use lash_trace::{
    TraceBranchMembership, TraceBranchSelection, TraceLanguageExecutionMap,
    TraceLanguageExecutionMapNode, TraceLanguageExecutionPayload, TraceLashlangGraphStore,
    TraceLashlangNodeObservation, TraceNodeAwaited, TraceNodeWaitResolution,
};
use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::sync::Arc;

/// `finish null`
fn finish_null() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;

    b::program(vec![b::finish(b::null())])
}

fn traced_process() -> (LashlangProcessExecutionTrace, Arc<TraceLashlangGraphStore>) {
    let hash = lashlang::ContentHash::new("trace-occurrence-tests");
    let store = Arc::new(TraceLashlangGraphStore::default());
    let trace = LashlangProcessExecutionTrace::new(
        product_trace(Some(store.clone()), lash_trace::TraceContext::default()),
        LashlangProcessTraceIdentity {
            session_id: None,
            process_id: lash_core::ProcessId::fixture("process"),
            source_identity: "source-identity".to_string(),
            module_ref: lashlang::ModuleRef::new(&hash),
            process_ref: lashlang::ProcessRef::new(hash, 0),
            process_name: "main".to_string(),
            attempt: 1,
            engine_execution_id: None,
        },
    );
    (trace, store)
}

fn execution_site(node_id: &str, kind: ExecutionNodeKind) -> lashlang::LashlangExecutionSite {
    lashlang::LashlangExecutionSite {
        node_id: node_id.to_string(),
        node_kind: kind,
        label: node_id.to_string(),
        branch: None,
        workflow_site: lashlang::WorkflowExecutionSite::new("main", [0], kind, node_id),
    }
}

#[test]
fn process_wait_resolution_and_cancellation_follow_only_the_active_occurrence() {
    let (trace, store) = traced_process();
    let completed = execution_site("completed", ExecutionNodeKind::Sleep);
    trace.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
        site: completed.clone(),
        occurrence: 1,
    });
    let completed_call = lashlang::LashlangExecutionCallSite {
        site: completed.clone(),
        occurrence: 1,
    };
    trace.emit_waiting(
        &completed_call,
        TraceNodeAwaited::Sleep {
            deadline_ms: Some(42),
        },
    );
    let graph = store.graphs().pop().expect("wait graph");
    assert!(matches!(
        graph
            .nodes
            .iter()
            .find(|node| node.id == "completed")
            .expect("completed node")
            .observation,
        TraceLashlangNodeObservation::Waiting {
            awaited: TraceNodeAwaited::Sleep {
                deadline_ms: Some(42)
            },
            ..
        }
    ));
    trace.emit_resumed(&completed_call, TraceNodeWaitResolution::TimedOut);
    trace.emit_observation(lashlang::LashlangExecutionObservation::NodeCompleted {
        site: completed,
        occurrence: 1,
    });

    let cancelled = execution_site("cancelled", ExecutionNodeKind::Wait);
    trace.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
        site: cancelled.clone(),
        occurrence: 1,
    });
    trace.emit_waiting(
        &lashlang::LashlangExecutionCallSite {
            site: cancelled,
            occurrence: 1,
        },
        TraceNodeAwaited::Signal {
            name: "ready".to_string(),
            key: "signal-key".to_string(),
        },
    );
    trace.emit_cancelled_in_flight();

    let graph = store.graphs().pop().expect("terminal graph");
    assert!(matches!(
        graph
            .nodes
            .iter()
            .find(|node| node.id == "completed")
            .expect("completed node")
            .observation,
        TraceLashlangNodeObservation::Completed { .. }
    ));
    assert!(matches!(
        graph
            .nodes
            .iter()
            .find(|node| node.id == "cancelled")
            .expect("cancelled node")
            .observation,
        TraceLashlangNodeObservation::Cancelled { .. }
    ));
    assert!(graph.history.iter().any(|item| matches!(
        item.event.payload,
        TraceLanguageExecutionPayload::NodeResumed {
            resolution: TraceNodeWaitResolution::Cancelled,
            ..
        }
    )));
}

#[test]
fn process_branch_selection_derives_the_untaken_arm_in_each_iteration() {
    let (mut trace, store) = traced_process();
    let branch_node_id = "branch".to_string();
    let arm_node = |id: &str, arm| TraceLanguageExecutionMapNode {
        id: id.to_string(),
        site: lashlang::WorkflowExecutionSite::new("main", [0], ExecutionNodeKind::Call, id),
        kind: ExecutionNodeKind::Call,
        label: id.to_string(),
        branch_memberships: vec![TraceBranchMembership {
            branch_node_id: branch_node_id.clone(),
            arm,
        }],
        label_metadata: None,
    };
    trace.execution_map = Some(Arc::new(TraceLanguageExecutionMap {
        nodes: vec![
            arm_node("then", TraceBranchSelection::Then),
            arm_node("else", TraceBranchSelection::Else),
        ],
        edges: Vec::new(),
    }));
    trace.emit(lash_trace::TraceLanguageExecution {
        event_key: trace.event_key("started"),
        identity: trace.identity(),
        payload: TraceLanguageExecutionPayload::ExecutionStarted {
            execution_map: trace
                .execution_map
                .as_ref()
                .expect("test execution map")
                .as_ref()
                .clone(),
        },
    });
    let branch = execution_site("branch", ExecutionNodeKind::Branch);
    for (occurrence, selected) in [
        (1, lashlang::ProcessBranchSelection::Then),
        (2, lashlang::ProcessBranchSelection::Else),
    ] {
        trace.emit_observation(lashlang::LashlangExecutionObservation::BranchSelected {
            site: branch.clone(),
            occurrence,
            edge_id: format!("edge-{occurrence}"),
            selected,
        });
        let taken = if occurrence == 1 { "then" } else { "else" };
        trace.emit_observation(lashlang::LashlangExecutionObservation::NodeCompleted {
            site: execution_site(taken, ExecutionNodeKind::Call),
            occurrence: 1,
        });
        if occurrence == 1 {
            let first = store.graphs().pop().expect("first branch graph");
            assert!(first.nodes.iter().any(|node| {
                node.id == "else"
                    && matches!(
                        node.observation,
                        TraceLashlangNodeObservation::Skipped {
                            branch_occurrence: 1,
                            ..
                        }
                    )
            }));
        }
    }
    let graph = store.graphs().pop().expect("branch graph");
    let mut skipped = graph
        .nodes
        .iter()
        .filter_map(|node| match node.observation {
            TraceLashlangNodeObservation::Skipped {
                branch_occurrence, ..
            } => Some((node.id.as_str(), branch_occurrence)),
            _ => None,
        })
        .collect::<Vec<_>>();
    skipped.sort_unstable();
    assert_eq!(skipped, [("then", 2)]);
}

#[test]
fn process_trace_session_attribution_comes_only_from_a_session_originator() {
    let identity = |originator: lash_core::ProcessOriginator, attempt, process: &str| {
        let hash = lashlang::ContentHash::new("trace-provenance");
        LashlangProcessExecutionTrace::new(
            product_trace(
                None,
                lash_trace::TraceContext::default().for_session("ambient-capability"),
            ),
            LashlangProcessTraceIdentity {
                session_id: process_trace_session_id(&originator),
                process_id: lash_core::ProcessId::fixture(process),
                source_identity: "source-identity".to_string(),
                module_ref: lashlang::ModuleRef::new(&hash),
                process_ref: lashlang::ProcessRef::new(hash, 0),
                process_name: "main".to_string(),
                attempt,
                engine_execution_id: None,
            },
        )
        .identity()
    };

    let host_identity = identity(
        lash_core::ProcessOriginator::host_scoped("operator"),
        1,
        "process",
    );
    assert_eq!(
        host_identity.scope.session_id, None,
        "a host namespace and ambient capability are not runtime session attribution"
    );
    assert_eq!(host_identity.attempt(), Some(1));
    assert_ne!(
        host_identity.graph_key(),
        identity(
            lash_core::ProcessOriginator::host_scoped("operator"),
            2,
            "process"
        )
        .graph_key(),
        "attempts partition process trace graphs"
    );
    assert_ne!(
        host_identity.graph_key(),
        identity(
            lash_core::ProcessOriginator::host_scoped("operator"),
            1,
            "other-process"
        )
        .graph_key(),
        "processes partition process trace graphs"
    );
    assert_eq!(
        identity(
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new("actual-session",)),
            1,
            "process",
        )
        .scope
        .session_id,
        Some(lash_sansio::SessionId::from("actual-session"))
    );
}

#[test]
fn interrupted_resource_node_and_retried_occurrence_keep_distinct_trace_generations() {
    let graphs = std::sync::Arc::new(lash_trace::TraceLashlangGraphStore::default());
    let trace_for_attempt = |attempt| {
        let hash = lashlang::ContentHash::new("retried-resource-trace");
        LashlangProcessExecutionTrace::new(
            product_trace(Some(graphs.clone()), lash_trace::TraceContext::default()),
            LashlangProcessTraceIdentity {
                session_id: None,
                process_id: lash_core::ProcessId::fixture("recovered-process"),
                source_identity: "source-identity".to_string(),
                module_ref: lashlang::ModuleRef::new(&hash),
                process_ref: lashlang::ProcessRef::new(hash, 0),
                process_name: "main".to_string(),
                attempt,
                engine_execution_id: None,
            },
        )
    };
    let site = lashlang::LashlangExecutionSite {
        node_id: "node:read".to_string(),
        node_kind: lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND,
        label: "read".to_string(),
        branch: None,
        workflow_site: lashlang::WorkflowExecutionSite::new(
            "main",
            [0],
            lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND,
            "read",
        ),
    };
    let call_site = lashlang::LashlangExecutionCallSite {
        site: site.clone(),
        occurrence: 1,
    };

    let interrupted = trace_for_attempt(1);
    interrupted.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
        site: site.clone(),
        occurrence: 1,
    });
    interrupted.record_resource_call(
        &call_site,
        &lash_core::ToolCallId::fixture("same-settled-effect-key"),
    );
    drop(interrupted); // A worker loss leaves the node started, without a terminal observation.

    let retried = trace_for_attempt(2);
    retried.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
        site: site.clone(),
        occurrence: 1,
    });
    retried.record_resource_call(
        &call_site,
        &lash_core::ToolCallId::fixture("same-settled-effect-key"),
    );
    retried.emit_observation(lashlang::LashlangExecutionObservation::NodeCompleted {
        site,
        occurrence: 1,
    });

    let first = graphs
        .graph(&trace_for_attempt(1).identity().graph_key())
        .expect("interrupted attempt remains visible");
    let second = graphs
        .graph(&trace_for_attempt(2).identity().graph_key())
        .expect("retry has its own trace graph");
    assert_eq!(graphs.graphs().len(), 2);
    assert_eq!(first.nodes.len(), 1);
    assert_eq!(second.nodes.len(), 1);
    assert!(matches!(
        first.nodes[0].observation,
        lash_trace::TraceLashlangNodeObservation::Running { occurrence: 1, .. }
    ));
    assert!(matches!(
        second.nodes[0].observation,
        lash_trace::TraceLashlangNodeObservation::Completed { occurrence: 1, .. }
    ));
    for graph in [first, second] {
        assert!(graph.history.iter().any(|record| matches!(
            &record.event.payload,
            lash_trace::TraceLanguageExecutionPayload::NodeStarted {
                occurrence: 1,
                call_id: Some(call_id),
                ..
            } if *call_id == lash_core::ToolCallId::fixture("same-settled-effect-key")
        )));
    }
}

#[test]
fn untraced_completed_resource_calls_retain_no_correlation_state() {
    let hash = lashlang::ContentHash::new("untraced-resource-correlation");
    let trace = LashlangProcessExecutionTrace::new(
        product_trace(None, lash_trace::TraceContext::default()),
        LashlangProcessTraceIdentity {
            session_id: None,
            process_id: lash_core::ProcessId::fixture("process"),
            source_identity: "source-identity".to_string(),
            module_ref: lashlang::ModuleRef::new(&hash),
            process_ref: lashlang::ProcessRef::new(hash, 0),
            process_name: "main".to_string(),
            attempt: 1,
            engine_execution_id: None,
        },
    );
    assert!(
        !trace.tracing.observes_language(),
        "the witness must run without tracing"
    );

    for occurrence in 1..=8 {
        let site = lashlang::LashlangExecutionSite {
            node_id: "node:resource".to_string(),
            node_kind: lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND,
            label: "echo".to_string(),
            branch: None,
            workflow_site: lashlang::WorkflowExecutionSite::new(
                "main",
                [0],
                lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND,
                "echo",
            ),
        };
        trace.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
            site: site.clone(),
            occurrence,
        });
        trace.record_resource_call(
            &lashlang::LashlangExecutionCallSite {
                site: site.clone(),
                occurrence,
            },
            &lash_core::ToolCallId::fixture(&format!("call-{occurrence}")),
        );
        trace.emit_observation(lashlang::LashlangExecutionObservation::NodeCompleted {
            site,
            occurrence,
        });
    }

    assert!(trace.resource_call_ids.lock_recover().is_empty());
    assert!(trace.pending_resource_starts.lock_recover().is_empty());
    let call_site = lashlang::LashlangExecutionCallSite {
        site: execution_site("untraced-wait", ExecutionNodeKind::Sleep),
        occurrence: 1,
    };
    trace.emit_waiting(&call_site, TraceNodeAwaited::Sleep { deadline_ms: None });
    trace.emit_resumed(&call_site, TraceNodeWaitResolution::TimedOut);
    assert!(trace.waiting_nodes.is_empty());
}
use std::sync::atomic::Ordering;

/// A loop parked by this build inside its `for` body after one sleep, with the
/// generation that wrote it. Regenerate with `capture_parked_loop_segment`.
pub(crate) const PARKED_LOOP_SEGMENT: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_parked_loop.json");

/// The generation that wrote a parked-segment golden: a golden is evidence
/// about the build whose generation it records, and about no other.
#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SegmentGoldenGeneration {
    segment_state_version: u32,
    bytecode_format_version: u32,
    vm_continuation_format_version: u32,
}

impl SegmentGoldenGeneration {
    pub(crate) fn of_this_build() -> Self {
        Self {
            segment_state_version: LASHLANG_SEGMENT_STATE_VERSION,
            bytecode_format_version: lashlang::BYTECODE_FORMAT_VERSION,
            vm_continuation_format_version: lashlang::VM_CONTINUATION_FORMAT_VERSION,
        }
    }
}

/// The parked-loop golden as JSON, after checking this build wrote it.
pub(crate) fn parked_loop_segment_golden() -> serde_json::Value {
    let golden: serde_json::Value =
        serde_json::from_slice(PARKED_LOOP_SEGMENT).expect("the parked-loop golden is JSON");
    let generation: SegmentGoldenGeneration =
        serde_json::from_value(golden["generation"].clone()).expect("the golden's generation");
    assert_eq!(
        generation,
        SegmentGoldenGeneration::of_this_build(),
        "the parked-loop golden was written by another generation; recapture it with \
         capture_parked_loop_segment"
    );
    golden
}

struct SegmentFixtureHost;

impl lashlang::ExecutionHost for SegmentFixtureHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityOutcome, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::Sleep(_) => {
                Ok(lashlang::AbilityOutcome::Value(lashlang::Value::Null))
            }
            _ => Err(lashlang::ExecutionHostError::new(
                "the segment fixture executes only its loop sleep",
            )),
        }
    }
}

fn parked_loop_input() -> crate::LashlangProcessInput {
    let hash = lashlang::ContentHash::new("parked-loop");
    crate::LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 0),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "loop_fixture".to_string(),
        args: serde_json::Map::new(),
    }
}

fn parked_loop_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;

    b::program(vec![
        b::for_in(
            "item",
            b::list(vec![b::num(1.0)]),
            b::block(vec![b::sleep_for(b::var("item"))]),
        ),
        b::finish(b::null()),
    ])
}

const CAPTURE_ENV: &str = "LASH_REGENERATE";

#[tokio::test(flavor = "current_thread")]
#[ignore = "regenerates crates/lash-lashlang-runtime/src/fixtures/lashlang_segment_parked_loop.json"]
async fn capture_parked_loop_segment() {
    assert_eq!(
        std::env::var(CAPTURE_ENV).as_deref(),
        Ok("1"),
        "set {CAPTURE_ENV}=1 to acknowledge replacing the committed golden"
    );
    let compiled = lashlang::testing::harness::try_compile_program(&parked_loop_program())
        .expect("compile loop");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).process();
    let mut vm = lashlang::Vm::from_state(&compiled, &mut state, &environment)
        .expect("construct loop fixture VM");
    assert_eq!(
        vm.run_process_until_effect().await.expect("park in loop"),
        lashlang::VmRunOutcome::EffectCompleted
    );
    let continuation = vm.suspend().expect("capture parked loop continuation");
    assert_eq!(
        continuation.iterator_stack.len(),
        1,
        "the loop must be parked inside its body"
    );
    let process_id = lash_sansio::ProcessId::fixture("fixture");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: lash_vm_protocol::OpaqueVmState::seal(
            lash_vm_protocol::VmStateKind::Continuation,
            super::segment_continuation_owner(&process_id),
            lashlang::vm_contract_versions(),
            worker_parked_continuation(continuation.to_bytes().expect("encode the continuation")),
        ),
        ordinals: ReplayOrdinalsState {
            commands: crate::LashlangRunOrdinals::start(),
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
        pending_summary: Vec::new(),
        effect_omissions: std::collections::BTreeMap::new(),
        worker_recovery: Default::default(),
    };
    let input = parked_loop_input();
    let golden = serde_json::json!({
        "generation": SegmentGoldenGeneration::of_this_build(),
        "program": "for (const item of [1]) { await sleep(item); } finish(null);",
        "input": input,
        "program_hash": super::lashlang_program_hash(&input),
        "segment_state": segment_state,
    });
    let mut bytes = serde_json::to_vec_pretty(&golden).expect("serialize the parked-loop golden");
    bytes.push(b'\n');
    let root = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || std::path::Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
        |root| std::path::PathBuf::from(root).join("crates/lash-lashlang-runtime"),
    );
    std::fs::write(
        root.join("src/fixtures/lashlang_segment_parked_loop.json"),
        bytes,
    )
    .expect("write the parked-loop golden");
}

/// The golden this build parked decodes, names this build's program identity
/// and restores in the worker inside its loop.
#[tokio::test]
async fn the_parked_loop_golden_is_this_builds_segment() {
    let golden = parked_loop_segment_golden();
    let input: crate::LashlangProcessInput =
        serde_json::from_value(golden["input"].clone()).expect("the golden's input");
    assert_eq!(
        golden["program_hash"],
        serde_json::json!(super::lashlang_program_hash(&input)),
        "the golden names this build's program identity"
    );
    let engine_state =
        serde_json::to_vec(&golden["segment_state"]).expect("encode the parked handover");
    let segment = decode_lashlang_segment_state(&engine_state)
        .unwrap_or_else(|error| panic!("this build's segment decodes: {error}"));
    assert_eq!(segment.version, LASHLANG_SEGMENT_STATE_VERSION);
    assert_eq!(
        worker_continuation_info(&segment.vm).await,
        Ok(1),
        "the continuation is parked inside its loop"
    );
}

/// A continuation sealed the way a boundary seals it for `process_id`.
fn sealed_continuation(
    continuation: &lashlang::VmContinuation,
    process_id: &lash_sansio::ProcessId,
) -> lash_vm_protocol::OpaqueVmState {
    lash_vm_protocol::OpaqueVmState::seal(
        lash_vm_protocol::VmStateKind::Continuation,
        super::segment_continuation_owner(process_id),
        lashlang::vm_contract_versions(),
        continuation.to_bytes().expect("encode the continuation"),
    )
}

/// Required Run and worker-recovery ledgers cannot default to fresh facts.
#[test]
fn a_segment_state_without_its_worker_recovery_ledger_is_a_typed_format_rejection() {
    let program = lashlang::testing::harness::try_compile_program(&finish_null())
        .expect("compile the boundary program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm = lashlang::Vm::from_state(&program, &mut state, &environment)
        .expect("construct the boundary VM");
    let envelope = serde_json::to_value(LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: sealed_continuation(
            &vm.suspend().expect("capture the boundary continuation"),
            &lash_sansio::ProcessId::fixture("boundary"),
        ),
        ordinals: ReplayOrdinalsState {
            commands: crate::LashlangRunOrdinals::start(),
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
        pending_summary: Vec::new(),
        effect_omissions: Default::default(),
        worker_recovery: Default::default(),
    })
    .expect("encode the segment state");
    let full = serde_json::to_vec(&envelope).expect("encode the full envelope");
    assert!(
        decode_lashlang_segment_state(&full).is_ok(),
        "the full envelope decodes"
    );

    let field = "worker_recovery";
    let mut partial = envelope.clone();
    partial
        .as_object_mut()
        .expect("the envelope is an object")
        .remove(field)
        .expect("the envelope carries the ledger");
    let partial = serde_json::to_vec(&partial).expect("encode the partial envelope");
    let Err(error) = decode_lashlang_segment_state(&partial) else {
        panic!("an envelope without {field} must not decode");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::FormatMismatch { details }
                if details.contains(&format!("missing field `{field}`"))
        ),
        "unexpected error: {error}"
    );
}

/// A handover with no version stamp has no compatibility decoder: it is the
/// typed mismatch, with the drain remedy.
#[test]
fn an_unversioned_segment_is_a_typed_rejection_with_the_drain_remedy() {
    let mut unversioned = parked_loop_segment_golden()["segment_state"].clone();
    unversioned
        .as_object_mut()
        .expect("the segment state is an object")
        .remove("version");
    let bytes = serde_json::to_vec(&unversioned).expect("encode the unversioned handover");
    let Err(error) = decode_lashlang_segment_state(&bytes) else {
        panic!("an unversioned handover must not have a compatibility decoder");
    };

    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found: 0,
            }
        ),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

/// A segment another generation parked is refused at both fences: the envelope
/// version, on the bytes as written, and the continuation's own format, which
/// the parent refuses structurally without decoding it.
#[test]
fn another_generations_segment_is_refused_at_both_fences() {
    let other_version = LASHLANG_SEGMENT_STATE_VERSION + 1;
    let mut other = parked_loop_segment_golden()["segment_state"].clone();
    other["version"] = serde_json::json!(other_version);
    let bytes = serde_json::to_vec(&other).expect("encode the other generation's handover");
    let Err(error) = decode_lashlang_segment_state(&bytes) else {
        panic!("another generation's envelope must not decode");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found,
            } if *found == other_version
        ),
        "unexpected error: {error}"
    );

    let segment: LashlangSegmentState =
        serde_json::from_value(parked_loop_segment_golden()["segment_state"].clone())
            .expect("this build's envelope");
    let process_id = lash_sansio::ProcessId::fixture("fixture");
    let owner = super::segment_continuation_owner(&process_id);
    let other_format = lashlang::VM_CONTINUATION_FORMAT_VERSION + 1;
    let sealed = lash_vm_protocol::OpaqueVmState::seal(
        lash_vm_protocol::VmStateKind::Continuation,
        owner.clone(),
        lash_vm_protocol::VmContract {
            continuation: other_format,
            ..lashlang::vm_contract_versions()
        },
        segment.vm.bytes().to_vec(),
    );
    let reads = lashlang::vm_contract_reads();
    assert_eq!(
        sealed.check(&super::segment_continuation_expectation(&owner, &reads)),
        Err(
            lash_vm_protocol::OpaqueStateRefusal::ComponentOutsideReadRange {
                component: lash_vm_protocol::VmContractComponent::Continuation,
                reads: lashlang::VM_CONTINUATION_READ_RANGE,
                found: other_format,
            }
        )
    );
}

/// The shared resume refusal (FIG-3588), naming the identity it refused.
#[track_caller]
fn assert_retired_generation(output: &lash_core::ProcessAwaitOutput, found: &str) {
    assert!(
        matches!(
            output,
            lash_core::ProcessAwaitOutput::Abandoned { evidence, .. }
                if evidence.writer == lash_core::AbandonWriter::ResumeRefused {
                    reason: lash_core::ProcessResumeRefusal::RetiredGeneration {
                        found: found.to_string(),
                    },
                }
        ),
        "{output:?}"
    );
}

#[test]
fn resume_rejects_changed_bytecode_program_hash_with_typed_failure() {
    let output = refuse_foreign_program("sha256:old", "sha256:current", None)
        .expect("changed bytecode identity must fail closed");
    assert_retired_generation(&output, "sha256:old");
}

/// E5 (FIG-3571): the summary a boundary carries in segment state is bounded
/// by construction, not by a flush schedule: however many times a run's
/// effect nodes fire, at most the cap per node is ever pending, the rest are
/// counts, and a successor restored from the encoded state keeps counting from
/// it and commits what it inherited at its first boundary.
#[test]
fn a_segment_boundary_carries_at_most_the_cap_per_node_of_pending_summary() {
    let cap = lash_core::PROCESS_EFFECT_OCCURRENCE_CAP;
    let nodes = ["node:a", "node:b", "node:c"];
    let occurrence = |node: &str, occurrence: u64| {
        lash_core::ProcessEffectOccurrence::new(
            node,
            occurrence,
            "tool:bounded",
            lash_core::ProcessEffectOutcomeClass::Success,
            None,
            format!("run:{node}:{occurrence}"),
            lash_core::FleetFormat::current(),
        )
    };
    let writer = EffectSummaryWriter::default();
    for fired in 1..=3 * cap {
        for node in nodes {
            writer.record(occurrence(node, fired));
        }
    }

    let program = lashlang::testing::harness::try_compile_program(&finish_null())
        .expect("compile the boundary program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm = lashlang::Vm::from_state(&program, &mut state, &environment)
        .expect("construct the boundary VM");
    let encoded = serde_json::to_vec(&LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: sealed_continuation(
            &vm.suspend().expect("capture the boundary continuation"),
            &lash_sansio::ProcessId::fixture("boundary"),
        ),
        ordinals: ReplayOrdinalsState {
            commands: crate::LashlangRunOrdinals::start(),
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
        pending_summary: writer.pending(),
        effect_omissions: writer.omissions(),
        worker_recovery: Default::default(),
    })
    .expect("encode the boundary's segment state");
    let decoded = decode_lashlang_segment_state(&encoded).expect("decode the segment state");
    assert_eq!(
        decoded.pending_summary.len() as u64,
        cap * nodes.len() as u64,
        "at most the cap per node is pending"
    );
    for node in nodes {
        assert_eq!(
            decoded
                .pending_summary
                .iter()
                .filter(|pending| pending.node_id == node)
                .map(|pending| pending.occurrence)
                .collect::<Vec<_>>(),
            (1..=cap).collect::<Vec<_>>(),
            "{node} pends its first cap occurrences, in order"
        );
        assert_eq!(decoded.effect_omissions[node].success, 2 * cap);
    }

    // The successor keeps counting from the encoded state and commits what
    // it inherited, then only the omission record is left for its terminal.
    let successor = EffectSummaryWriter::restore(decoded.pending_summary, decoded.effect_omissions);
    successor.record(occurrence("node:a", 3 * cap + 1));
    let prelude = successor.prelude();
    assert_eq!(prelude.requests.len() as u64, cap * nodes.len() as u64);
    successor.settle(&prelude);
    assert!(
        successor.pending().is_empty(),
        "a committed boundary drops what it carried"
    );
    let terminal = successor.terminal_prelude(
        "run:omissions".to_string(),
        lash_core::FleetFormat::current(),
    );
    assert_eq!(
        terminal.len(),
        1,
        "the terminal batch carries only the omission record"
    );
    assert_eq!(
        lash_core::ProcessEffectOmissions::decode(
            terminal[0].payload.clone(),
            lash_core::FleetFormat::current(),
        )
        .expect("decode the omission record")
        .nodes["node:a"]
            .success,
        2 * cap + 1
    );
}

#[test]
fn durable_exhaustion_has_a_typed_process_failure_surface() {
    let previous = EXECUTION_BOUND_EXHAUSTION_LOUD.swap(false, Ordering::SeqCst);
    let output = process_lashlang_execution_result(
        Err(lashlang::RuntimeError::InstructionBudgetExceeded { limit: 1 }),
        None,
    );
    EXECUTION_BOUND_EXHAUSTION_LOUD.store(previous, Ordering::SeqCst);
    assert!(matches!(
        output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "process_execution_bound_exhausted")
    ));
}

/// FIG-4420 wraps the VM wire in the worker's MessagePack `ParkedRun`.
/// These witnesses park after a completed effect, so no request needs reissue.
pub(super) fn worker_parked_continuation(vm: Vec<u8>) -> Vec<u8> {
    #[derive(serde::Serialize)]
    struct ParkedRun {
        vm: lash_vm_protocol::EncodedPayload,
        request: Option<()>,
    }

    rmp_serde::to_vec_named(&ParkedRun {
        vm: lash_vm_protocol::EncodedPayload(vm),
        request: None,
    })
    .expect("encode the worker's parked continuation")
}

async fn worker_continuation_info(
    state: &lash_vm_protocol::OpaqueVmState,
) -> Result<usize, String> {
    match lash_vm_client::service::Service::default()
        .request_accounted(lash_vm_client::service::Request::ContinuationInfo {
            bytes: state.bytes().to_vec(),
        })
        .await
        .map_err(|e| e.to_string())?
    {
        lash_vm_client::service::Response::ContinuationInfo { iterator_count } => {
            Ok(iterator_count)
        }
        lash_vm_client::service::Response::Refused { message, .. } => Err(message),
        other => Err(format!("unexpected continuation response: {other:?}")),
    }
}

fn product_trace(
    sink: Option<Arc<dyn lash_trace::TraceSink>>,
    context: lash_trace::TraceContext,
) -> lash_core::plugin::PluginExecutionTrace {
    let mut runtime =
        lash_core::trace::TraceRuntime::new(Arc::new(lash_core::facade_support::SystemClock))
            .with_base_context(context);
    if let Some(sink) = sink {
        runtime = runtime.with_product_observer(sink);
    }
    lash_core::plugin::PluginExecutionTrace::new(runtime.unreplayed(None))
}

#[test]
fn process_graph_replay_uses_the_shared_runtime_without_exporting_again() {
    #[derive(Default)]
    struct Records(std::sync::Mutex<Vec<lash_trace::TraceRecord>>);
    impl lash_trace::TraceSink for Records {
        fn append(
            &self,
            record: &lash_trace::TraceRecord,
        ) -> Result<(), lash_trace::TraceSinkError> {
            self.0.lock_recover().push(record.clone());
            Ok(())
        }
    }
    let (mut trace, graphs) = traced_process();
    let records = Arc::new(Records::default());
    let runtime = lash_core::trace::TraceRuntime::new(Arc::new(
        lash_core::testing::TestClock::new(1_700_000_000_123),
    ))
    .with_trace_sink(records.clone())
    .with_product_observer(graphs.clone());
    let scope = lash_trace::DurableTraceScope {
        scope: lash_trace::TraceScopeId::admission(lash_trace::TraceScopeOwner::Process {
            process_id: trace.process_id.clone(),
        }),
        cause: lash_trace::TraceCause::Root,
        anchor: lash_trace::TraceAnchor::Untraced,
        started_at_ms: 1_700_000_000_000,
    };
    trace.tracing =
        lash_core::plugin::PluginExecutionTrace::new(runtime.unreplayed(Some(scope.clone())));
    let observation = || lashlang::LashlangExecutionObservation::NodeStarted {
        site: execution_site("shared-node", ExecutionNodeKind::Sleep),
        occurrence: 1,
    };
    trace.emit_observation(observation());
    let original = graphs.graphs();
    assert_eq!(original.len(), 1);
    let emitted = records.0.lock_recover().clone();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].timestamp.timestamp_millis(), 1_700_000_000_123);
    assert_eq!(trace.tracing.trace_scope(), Some(&scope));
    let controller = lash_core::ActorContext::unavailable()
        .scoped(lash_core::AdmittedScope::process(trace.process_id.clone()))
        .expect("the process controller");
    graphs.clear();
    trace.tracing = lash_core::plugin::PluginExecutionTrace::new(
        runtime.shift(Some(scope.clone()), &controller),
    );
    trace.emit_observation(observation());
    assert_eq!(trace.tracing.trace_scope(), Some(&scope));
    assert_eq!(graphs.graphs(), original);
    assert_eq!(
        *records.0.lock_recover(),
        emitted,
        "product replay exports no copies"
    );
}
