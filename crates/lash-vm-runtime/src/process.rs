//! A lash_vm process's identity, its admission for a run, and the
//! terminals a run ends in. The process runs on the durable engine
//! ([`crate::engine`]).

/// version_surface = "coexist"
/// version_guard(items(LASH_VM_PROGRAM_DOMAIN_VERSION, lash_vm_program_hash))
const LASH_VM_PROGRAM_DOMAIN_VERSION: &str = "lash-vm-program/v3";

mod execution_result;
pub(crate) use execution_result::{
    process_lash_vm_execution_result, process_lash_vm_failure, process_worker_failure,
};
use lash_sansio::ProcessId;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{
    LashVmHostEnvironmentCheck, LashVmProcessFailureCode, LashVmProcessInput,
    bridge::lash_vm_value_to_json, validate_lash_vm_process_admission,
};

static PARK_DECLINED_TOTAL: AtomicU64 = AtomicU64::new(0);
#[cfg(any(test, feature = "testing"))]
static EXECUTION_BOUND_EXHAUSTION_LOUD: AtomicBool = AtomicBool::new(true);

pub(crate) fn record_park_decline(error: &dyn std::fmt::Display, message: &'static str) {
    let declined_total = PARK_DECLINED_TOTAL
        .fetch_add(1, Ordering::Relaxed)
        .saturating_add(1);
    tracing::warn!(error = %error, declined_total, "{message}");
}

/// The executable generation a Lash VM process runs as (FIG-3571).
///
/// Its preimage is the process body's [`lash_vm::ExecutableIdentity`] (the
/// module, the exported process, and the semantic-hash, bytecode,
/// instruction-accounting and node-id contracts the body compiles and reports
/// under) plus what this tier adds on top: the engine-state generation a
/// parked body resumes from and the host requirements it was admitted
/// against. Nothing
/// stored says "this was written by version N": a run compares the generation
/// its start record names against the one this build mints for the same
/// payload, which is a pure function computed before the artifact is loaded.
///
/// Public because a readability preflight has no other way to ask the
/// question.
#[expect(
    clippy::expect_used,
    reason = "the identity is a tuple of strings and integer constants serialized straight to in-memory bytes"
)]
pub fn lash_vm_program_hash(input: &LashVmProcessInput) -> String {
    let executable = lash_vm::ExecutableIdentity::of(
        &input.module_ref,
        lash_vm::Entry::Process(&input.process_ref),
    );
    let identity = serde_json::to_vec(&(
        executable.as_str(),
        crate::engine::LASH_VM_SEGMENT_STATE_VERSION,
        &input.host_requirements_ref,
    ))
    .expect("lash_vm program identity should serialize");
    format!(
        "blake3:{}",
        lash_sansio::core_support::blake3_domain_hash_hex(LASH_VM_PROGRAM_DOMAIN_VERSION, identity,)
    )
}

/// The shared resume refusal for a run whose stored generation this build
/// retired: the process ends Abandoned with `ResumeRefused {
/// RetiredGeneration }` naming the identity it found, before any effect.
pub(crate) fn retired_generation_at(found: String, epoch_ms: u64) -> lash_core::ProcessAwaitOutput {
    lash_core::ProcessAwaitOutput::Abandoned {
        evidence: Box::new(lash_core::AbandonEvidence {
            writer: lash_core::AbandonWriter::ResumeRefused {
                reason: lash_core::ProcessResumeRefusal::RetiredGeneration { found },
            },
            owner: None,
            epoch_ms,
        }),
        control: None,
    }
}

pub(crate) fn validate_lash_vm_process_for_run(
    artifact: &lash_vm_client::InspectedArtifact,
    input: &LashVmProcessInput,
    host: LashVmHostEnvironmentCheck<'_>,
) -> Result<(), Box<lash_core::ProcessAwaitOutput>> {
    validate_lash_vm_process_admission(artifact, input, host).map_err(|refusal| {
        Box::new(process_lash_vm_failure(
            refusal.failure_code(),
            refusal.to_string(),
            None,
        ))
    })
}

/// The most snapshot bytes a parked process may carry: the preset the
/// measurement lane finalises.
const MAX_PROCESS_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

/// Whose snapshot a process's VM state is: the durable process it parks.
pub(crate) fn segment_continuation_owner(process_id: &ProcessId) -> lash_vm_protocol::VmOwner {
    lash_vm_protocol::VmOwner::new(format!("process:{process_id}"))
}

/// A process snapshot belongs to this process, is inside each VM
/// component's read range, and is within the size bound.
pub(crate) fn segment_continuation_expectation<'a>(
    owner: &'a lash_vm_protocol::VmOwner,
    reads: &'a lash_vm_protocol::VmContractReads,
) -> lash_vm_protocol::StateExpectation<'a> {
    lash_vm_protocol::StateExpectation {
        kind: lash_vm_protocol::VmStateKind::Continuation,
        owner,
        reads,
        max_bytes: MAX_PROCESS_SNAPSHOT_BYTES,
    }
}

#[path = "process/schema.rs"]
mod schema;
pub use schema::lash_vm_type_expr_schema;

#[path = "process/trace_map.rs"]
mod trace_map;
pub use trace_map::{trace_lashlang_main_map, trace_lashlang_process_map};

#[cfg(test)]
#[path = "process/opaque_state_tests.rs"]
mod opaque_state_tests;
