// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

use super::{
    EXECUTION_BOUND_EXHAUSTION_LOUD, LASHLANG_SEGMENT_STATE_VERSION, LashlangProcessExecutionTrace,
    LashlangProcessTraceIdentity, LashlangSegmentState, LashlangSegmentStateError,
    ReplayOrdinalsState, SEGMENT_BOUNDARY_DECLINED_TOTAL, decode_lashlang_segment_state,
    process_lashlang_execution_result, process_trace_session_id, record_segment_boundary_decline,
    resolve_child_max_attempts, validate_lashlang_program_hash,
};
use lash_sansio::sync::MutexExt;

/// `finish null`
fn finish_null() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;

    b::program(vec![b::finish(b::null())])
}

#[test]
fn process_trace_session_attribution_comes_only_from_a_session_originator() {
    let identity = |originator: lash_core::ProcessOriginator, attempt, incarnation| {
        let hash = lashlang::ContentHash::new("trace-provenance");
        LashlangProcessExecutionTrace::new(
            None,
            lash_trace::TraceContext::default().for_session("ambient-capability"),
            LashlangProcessTraceIdentity {
                session_id: process_trace_session_id(&originator),
                process_id: lash_core::ProcessId::from("process"),
                source_identity: "source-identity".to_string(),
                module_ref: lashlang::ModuleRef::new(&hash),
                process_ref: lashlang::ProcessRef::new(hash, 0),
                process_name: "main".to_string(),
                attempt,
                incarnation: lash_core::ProcessIncarnation::from_registration_sequence(incarnation),
                restate_invocation_id: None,
            },
        )
        .identity()
    };

    let host_identity = identity(lash_core::ProcessOriginator::host_scoped("operator"), 1, 1);
    assert_eq!(
        host_identity.scope.session_id, None,
        "a host namespace and ambient capability are not runtime session attribution"
    );
    assert_eq!(host_identity.attempt(), Some(1));
    assert_eq!(host_identity.incarnation(), Some(1));
    assert_ne!(
        host_identity.graph_key(),
        identity(lash_core::ProcessOriginator::host_scoped("operator"), 2, 1,).graph_key(),
        "attempts partition process trace graphs"
    );
    assert_ne!(
        host_identity.graph_key(),
        identity(lash_core::ProcessOriginator::host_scoped("operator"), 1, 2,).graph_key(),
        "incarnations partition process trace graphs"
    );
    assert_eq!(
        identity(
            lash_core::ProcessOriginator::session(lash_core::SessionScope::new("actual-session",)),
            1,
            1,
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
            Some(graphs.clone()),
            lash_trace::TraceContext::default(),
            LashlangProcessTraceIdentity {
                session_id: None,
                process_id: lash_core::ProcessId::from("recovered-process"),
                source_identity: "source-identity".to_string(),
                module_ref: lashlang::ModuleRef::new(&hash),
                process_ref: lashlang::ProcessRef::new(hash, 0),
                process_name: "main".to_string(),
                attempt,
                incarnation: lash_core::ProcessIncarnation::from_registration_sequence(3),
                restate_invocation_id: None,
            },
        )
    };
    let site = lashlang::LashlangExecutionSite {
        node_id: "node:read".to_string(),
        node_kind: lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND.to_string(),
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
    interrupted.record_resource_call(&call_site, "same-settled-effect-key");
    drop(interrupted); // A worker loss leaves the node started, without a terminal observation.

    let retried = trace_for_attempt(2);
    retried.emit_observation(lashlang::LashlangExecutionObservation::NodeStarted {
        site: site.clone(),
        occurrence: 1,
    });
    retried.record_resource_call(&call_site, "same-settled-effect-key");
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
            } if call_id == "same-settled-effect-key"
        )));
    }
}

#[test]
fn untraced_completed_resource_calls_retain_no_correlation_state() {
    let hash = lashlang::ContentHash::new("untraced-resource-correlation");
    let trace = LashlangProcessExecutionTrace::new(
        None,
        lash_trace::TraceContext::default(),
        LashlangProcessTraceIdentity {
            session_id: None,
            process_id: lash_core::ProcessId::from("process"),
            source_identity: "source-identity".to_string(),
            module_ref: lashlang::ModuleRef::new(&hash),
            process_ref: lashlang::ProcessRef::new(hash, 0),
            process_name: "main".to_string(),
            attempt: 1,
            incarnation: lash_core::ProcessIncarnation::from_registration_sequence(1),
            restate_invocation_id: None,
        },
    );
    assert!(trace.sink.is_none(), "the witness must run without tracing");

    for occurrence in 1..=8 {
        let site = lashlang::LashlangExecutionSite {
            node_id: "node:resource".to_string(),
            node_kind: lashlang::RESOURCE_OPERATION_EXECUTION_SITE_KIND.to_string(),
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
            &format!("call-{occurrence}"),
        );
        trace.emit_observation(lashlang::LashlangExecutionObservation::NodeCompleted {
            site,
            occurrence,
        });
    }

    assert!(trace.resource_call_ids.lock_recover().is_empty());
    assert!(trace.pending_resource_starts.lock_recover().is_empty());
}
use std::sync::atomic::Ordering;

const UNVERSIONED_SEGMENT_STATE: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_state_unversioned.json");
const VM_V10_SEGMENT_STATE: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_state_vm_v10.json");
const BYTECODE_V17_PARKED_LOOP: &[u8] =
    include_bytes!("../fixtures/lashlang_bytecode_v17_parked_loop.json");
// Captured by the real predecessor writer at
// f0bdb98f6567e94d41b28a7404a8920e2f9966eb after one observed `tools.echo`
// effect parked. Only nondeterministic elapsed time and nonce were normalized.
const SEGMENT_V12_PARKED_OLD_IDS: &[u8] =
    include_bytes!("../fixtures/lashlang_segment_v12_parked_old_ids.json");

struct SegmentFixtureHost;

impl lashlang::ExecutionHost for SegmentFixtureHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::Sleep(_) => {
                Ok(lashlang::AbilityResult::Value(lashlang::Value::Null))
            }
            _ => Err(lashlang::ExecutionHostError::new(
                "the segment fixture executes only its loop sleep",
            )),
        }
    }
}

fn bytecode_v17_loop_input() -> crate::LashlangProcessInput {
    let hash = lashlang::ContentHash::new("bytecode-v17-parked-loop");
    crate::LashlangProcessInput {
        module_ref: lashlang::ModuleRef::new(&hash),
        process_ref: lashlang::ProcessRef::new(hash.clone(), 0),
        host_requirements_ref: lashlang::HostRequirementsRef::new(&hash),
        process_name: "loop_fixture".to_string(),
        args: serde_json::Map::new(),
    }
}

fn bytecode_v17_loop_program() -> lashlang::Program {
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

#[tokio::test(flavor = "current_thread")]
#[ignore = "run only against the version-17 compiler before the loop-site cutover"]
async fn capture_bytecode_v17_parked_loop_from_predecessor_writer() {
    assert_eq!(
        lashlang::BYTECODE_FORMAT_VERSION,
        17,
        "capture this fixture only from the version-17 predecessor writer"
    );
    let compiled = lashlang::compile_ast(&bytecode_v17_loop_program()).expect("compile loop");
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
        "the predecessor must be parked inside its loop"
    );
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: continuation,
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 0,
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
    };
    let input = bytecode_v17_loop_input();
    let mut fixture = serde_json::json!({
        "bytecode_format_version": 17,
        "source_commit": "4f96c76629575e46b8d7f29526bb0cab7c16625b",
        "program": "for (const item of [1]) { await sleep(item); } finish(null);",
        "input": input,
        "program_hash": super::lashlang_program_hash(&input),
        "segment_state": segment_state,
    });
    fixture["segment_state"]["vm"]["execution_nonce"] = serde_json::json!(958985677965949815_u64);
    fixture["segment_state"]["vm"]["active_execution_elapsed"] =
        serde_json::json!({"nanos": 0, "secs": 0});
    let mut bytes = serde_json::to_vec_pretty(&fixture).expect("serialize parked loop fixture");
    bytes.push(b'\n');
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/fixtures/lashlang_bytecode_v17_parked_loop.json"),
        bytes,
    )
    .expect("write version-17 parked loop fixture");
}

#[test]
#[ignore = "run only with the predecessor writer at ccab40166"]
fn capture_vm_v10_segment_state_from_predecessor_writer() {
    assert_eq!(
        lashlang::VM_CONTINUATION_FORMAT_VERSION,
        10,
        "capture this fixture only from predecessor writer commit ccab40166"
    );
    let program = lashlang::compile_ast(&finish_null()).expect("compile fixture program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct fixture VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture fixture VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 3,
            event_sequence: 5,
            signal_wait_ordinals: [("ready".to_string(), 11)].into(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
    };
    let mut wire = serde_json::to_value(segment_state).expect("serialize segment-state writer");
    wire["vm"]["execution_nonce"] = serde_json::json!(16294208416658607535_u64);
    wire["vm"]["active_execution_elapsed"] = serde_json::json!({"nanos": 0, "secs": 0});
    let mut bytes = serde_json::to_vec(&wire).expect("serialize v10 predecessor");
    bytes.push(b'\n');
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/fixtures/lashlang_segment_state_vm_v10.json"),
        bytes,
    )
    .expect("write v10 predecessor fixture");
}

#[test]
fn unversioned_prior_shape_is_typed_rejection_with_cutover_remedy() {
    let Err(error) = decode_lashlang_segment_state(UNVERSIONED_SEGMENT_STATE) else {
        panic!("unversioned handover must not have a compatibility decoder");
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

/// The predecessor fixture is refused twice over, and each fence is asserted on
/// its own.
///
/// The fixture is a v11 envelope carrying a v10 VM continuation. Once the
/// envelope generation moved (v12, FIG-3394), decoding it whole stops at the
/// outer version and never reaches the continuation, so a single assertion
/// would silently stop testing the inner fence it was written for. The fixture
/// bytes are not edited to keep it reachable — they are a predecessor capture
/// (`capture_vm_v10_segment_state_from_predecessor_writer`) and hand-editing
/// them would make the evidence describe a shape no writer ever produced.
/// Instead the *outer* refusal is asserted on the file as captured, and the
/// inner one on the same file's `vm` node re-enveloped at the current
/// generation, which is the only construction that can reach the continuation
/// fence at all.
#[test]
fn the_v11_envelope_is_refused_by_the_current_envelope_version() {
    let Err(error) = decode_lashlang_segment_state(VM_V10_SEGMENT_STATE) else {
        panic!("a predecessor envelope must not decode");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found: 11,
            }
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn vm_v10_shape_with_projected_slots_is_a_versioned_rejection() {
    assert!(
        VM_V10_SEGMENT_STATE
            .windows(b"projected_slots".len())
            .any(|window| window == b"projected_slots"),
        "the predecessor fixture must preserve the retired key"
    );
    let mut wire: serde_json::Value =
        serde_json::from_slice(VM_V10_SEGMENT_STATE).expect("the predecessor fixture is JSON");
    wire["version"] = serde_json::json!(LASHLANG_SEGMENT_STATE_VERSION);
    let re_enveloped = serde_json::to_vec(&wire).expect("re-envelope the predecessor continuation");

    let Err(LashlangSegmentStateError::FormatMismatch { details }) =
        decode_lashlang_segment_state(&re_enveloped)
    else {
        panic!("the v10 VM continuation must be refused by the current decoder");
    };
    assert!(
        details.contains("version 10"),
        "unexpected refusal: {details}"
    );
    assert!(
        details.contains(&format!(
            "version {}",
            lashlang::VM_CONTINUATION_FORMAT_VERSION
        )),
        "the refusal must name the current VM continuation version: {details}"
    );
}

#[test]
fn resume_rejects_changed_bytecode_program_hash_with_typed_failure() {
    let output = validate_lashlang_program_hash("sha256:old", "sha256:current")
        .expect_err("changed bytecode identity must fail closed");
    assert!(matches!(
        *output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "restate_segment_program_hash_mismatch")
    ));
}

#[test]
fn bytecode_v17_parked_loop_is_refused_before_continuation_restore() {
    let fixture: serde_json::Value = serde_json::from_slice(BYTECODE_V17_PARKED_LOOP)
        .expect("the version-17 parked-loop fixture is JSON");
    assert_eq!(fixture["bytecode_format_version"], 17);
    assert_eq!(
        fixture["source_commit"],
        "4f96c76629575e46b8d7f29526bb0cab7c16625b"
    );
    let input: crate::LashlangProcessInput =
        serde_json::from_value(fixture["input"].clone()).expect("fixture input decodes");
    let persisted = fixture["program_hash"]
        .as_str()
        .expect("fixture program hash");
    let current = super::lashlang_program_hash(&input);
    assert_ne!(
        persisted, current,
        "the bytecode version must move identity"
    );

    let output = validate_lashlang_program_hash(persisted, &current)
        .expect_err("the predecessor must fail at the program-identity fence");
    assert!(matches!(
        *output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "restate_segment_program_hash_mismatch")
    ));

    let segment: LashlangSegmentState = serde_json::from_value(fixture["segment_state"].clone())
        .expect("the fixture carries a structurally valid current-envelope continuation");
    assert_eq!(
        segment.vm.iterator_stack.len(),
        1,
        "the refused continuation is parked inside the predecessor loop"
    );
}

/// The v11 envelope no longer carries `signal_send_sequence`: its only
/// producer was deleted with the signal special forms (FIG-2999), and a
/// durable field with no producer is removed rather than round-tripped.
#[test]
fn the_current_envelope_carries_no_dead_send_ordinal() {
    let program = lashlang::compile_ast(&finish_null()).expect("compile pinning program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct pinning VM");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture pinning VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 1,
            event_sequence: 2,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: std::num::NonZeroU32::new(5).expect("non-zero"),
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
    };
    let wire = serde_json::to_value(&segment_state).expect("serialize current segment state");
    assert_eq!(wire["version"], LASHLANG_SEGMENT_STATE_VERSION);
    assert!(
        wire.get("signal_send_sequence").is_none(),
        "the dead send ordinal must not be written: {wire}"
    );
}

#[test]
fn declined_boundary_is_warned_and_counted() {
    let before = SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed);
    record_segment_boundary_decline(&"projected state", "test boundary decline");
    assert_eq!(
        SEGMENT_BOUNDARY_DECLINED_TOTAL.load(Ordering::Relaxed),
        before + 1
    );
}

#[test]
fn durable_exhaustion_has_a_typed_process_failure_surface() {
    let previous = EXECUTION_BOUND_EXHAUSTION_LOUD.swap(false, Ordering::SeqCst);
    let output =
        process_lashlang_execution_result(Err(lashlang::RuntimeError::InstructionBudgetExceeded {
            limit: 1,
        }));
    EXECUTION_BOUND_EXHAUSTION_LOUD.store(previous, Ordering::SeqCst);
    assert!(matches!(
        output,
        lash_core::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Failure(ref failure)
                if failure.code == "process_execution_bound_exhausted")
    ));
}

#[test]
fn predecessor_segment_with_old_node_id_occurrence_counters_is_refused() {
    let wire: serde_json::Value = serde_json::from_slice(SEGMENT_V12_PARKED_OLD_IDS)
        .expect("the real v12 predecessor segment is JSON");
    assert_eq!(wire["version"], 12, "the fixture must remain literal v12");
    assert_eq!(
        wire["vm"]["occurrence_counters"]["resource_operation:f5157b6682a34e8b5f1fccdc"], 1,
        "the predecessor bytes must retain their real old-family occurrence counter"
    );

    let Err(error) = decode_lashlang_segment_state(SEGMENT_V12_PARKED_OLD_IDS) else {
        panic!("an old-id handover must not decode against the new node-id generation");
    };
    assert!(
        matches!(
            &error,
            LashlangSegmentStateError::VersionMismatch {
                expected: LASHLANG_SEGMENT_STATE_VERSION,
                found,
            } if *found == 12
        ),
        "unexpected error: {error}"
    );
    let message = error.to_string();
    assert!(message.contains("drain in-flight sessions on the old build"));
    assert!(message.contains("recreate development/test stores"));
}

#[test]
fn a_resumed_segment_keeps_the_recorded_attempt_bound_across_a_host_default_change() {
    let program = lashlang::compile_ast(&finish_null()).expect("compile pinning program");
    let mut state = lashlang::State::new();
    let host = SegmentFixtureHost;
    let environment = lashlang::ExecutionEnvironment::new(&host).foreground();
    let mut vm =
        lashlang::Vm::from_state(&program, &mut state, &environment).expect("construct pinning VM");
    let recorded = std::num::NonZeroU32::new(3).expect("non-zero recorded bound");
    let segment_state = LashlangSegmentState {
        version: LASHLANG_SEGMENT_STATE_VERSION,
        vm: vm.suspend().expect("capture pinning VM continuation"),
        ordinals: ReplayOrdinalsState {
            sleep_sequence: 0,
            event_sequence: 0,
            signal_wait_ordinals: Default::default(),
        },
        started_process_ids: Vec::new(),
        child_max_attempts: recorded,
        incorporation_ledger: lash_core::session::IncorporationLedger::default(),
    };
    let encoded = serde_json::to_vec(&segment_state).expect("encode segment handover");
    let decoded = decode_lashlang_segment_state(&encoded).expect("decode segment handover");

    let changed_host_default = std::num::NonZeroU32::new(11).expect("non-zero host default");
    assert_eq!(
        resolve_child_max_attempts(Some(&decoded), changed_host_default),
        recorded,
        "a resumed segment re-registers children with the bound already in their fingerprint"
    );
    assert_eq!(
        resolve_child_max_attempts(None, changed_host_default),
        changed_host_default,
        "only a first segment reads the live host default"
    );
}
