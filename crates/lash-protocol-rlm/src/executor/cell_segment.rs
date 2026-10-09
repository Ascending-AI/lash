//! A code cell's envelope: the host's half of its snapshot (ADR 0132 §8).
//!
//! Every quiet point of a cell commits the VM's state with the broker ledger
//! that matches it, and this envelope beside them as the checkpoint's host
//! state. The cell's parent holds the rest of the cell in ledgers, and the
//! envelope carries every one of them to the activation that resumes the
//! cell from that snapshot, so the resumed cell issues the operation it
//! stopped on again and runs on as if it had never stopped.
//!
//! The envelope holds the started children. A cell
//! owns more than a process body does, and all of it is plain data:
//! its prints and printed images,
//! the calls it made, the tool calls it counts against `max_tool_calls`, and
//! everything it linked against, which its journal recorded before its first
//! effect — the binding set, the projected bindings, the host environment
//! and the deferred grants. Those records live in the journal of the segment
//! that wrote them, so the envelope carries their contents and the resumed
//! cell links against them without journaling them again.
//! A projection the cell holds is plain data too (ADR 0132 §9), so nothing
//! keeps a cell on the build that started it.

use std::collections::BTreeMap;

use lash_core::RuntimeExecutionContext;

use super::host_bridge::CellHostLedgers;

/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_CELL_SEGMENT_CODE_DOMAIN_VERSION, code_digest))
const LASH_RLM_CELL_SEGMENT_CODE_DOMAIN_VERSION: &str = "lash-rlm-cell-segment-code/v1";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(super) struct RecordedPrint(#[serde(with = "lash_vm::effect_value")] pub lash_vm::Value);

/// A cell's envelope at a quiet point, as the activation that resumes the
/// cell reads it.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CellSegmentState {
    /// The digest of the cell's source: a snapshot under the cell's
    /// execution holds this source's state.
    pub code: String,
    /// The session's projected bindings as the cell recorded them.
    pub projected_bindings: BTreeMap<String, crate::projection::bindings::RecordedProjection>,
    /// The host environment the cell linked against.
    pub host_environment: lash_vm::LashVmHostEnvironment,
    /// The cell's journaled ambient binding set (FIG-3587).
    pub cell_bindings: lash_vm_runtime::RecordedCellToolBindings,
    /// The grants the cell's deferred resolutions recorded.
    pub deferred_execution_grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
    pub prints: Vec<RecordedPrint>,
    pub host: CellHostLedgers,
    pub started_process_ids: Vec<lash_core::ProcessId>,
}

impl CellSegmentState {
    pub(super) fn code_digest(code: &str) -> String {
        lash_sansio::core_support::blake3_domain_hash_hex(
            LASH_RLM_CELL_SEGMENT_CODE_DOMAIN_VERSION,
            code.as_bytes(),
        )
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>, String> {
        rmp_serde::to_vec_named(self).map_err(|error| error.to_string())
    }

    /// The envelope of a snapshot of the cell running `code`: a snapshot
    /// under the cell's execution of other source is refused.
    fn decode_for(bytes: &[u8], code: &str) -> Result<Self, String> {
        let state: Self = rmp_serde::from_slice(bytes).map_err(|error| error.to_string())?;
        if state.code != Self::code_digest(code) {
            return Err("the snapshot under this cell's execution holds other source".to_owned());
        }
        Ok(state)
    }

    /// The cell's envelope now: what its parent holds of it at a quiet
    /// point.
    pub(super) fn at_quiet_point(
        ctx: &RuntimeExecutionContext<'_>,
        host: &super::host_bridge::HostBridge<'_>,
        code: &str,
        linked: (
            BTreeMap<String, crate::projection::bindings::RecordedProjection>,
            lash_vm_runtime::RecordedCellToolBindings,
            lash_vm::LashVmHostEnvironment,
        ),
        deferred_execution_grants: BTreeMap<lash_core::ToolId, lash_core::ToolExecutionGrant>,
        prints: &std::sync::Mutex<Vec<lash_vm::Value>>,
    ) -> Self {
        let (projected_bindings, cell_bindings, host_environment) = linked;
        Self {
            code: Self::code_digest(code),
            projected_bindings,
            host_environment,
            cell_bindings,
            deferred_execution_grants,
            prints: prints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned()
                .map(RecordedPrint)
                .collect(),
            host: host.ledgers(),
            started_process_ids: ctx.started_process_ids(),
        }
    }

    /// The parent ledgers of `ctx` a boundary hands over.
    pub(super) fn restore_context(&self, ctx: &RuntimeExecutionContext<'_>) {
        ctx.restore_started_process_ids(&self.started_process_ids);
    }
}

/// The records of the tool calls a cell completed, in call order, as its
/// stored `snapshot` holds them in its envelope's call ledger.
pub(crate) fn snapshot_tool_calls(
    snapshot: &str,
) -> Result<Vec<lash_core::ToolCallRecord>, String> {
    Ok(stored_envelope(snapshot)?
        .map(|state| state.host.tool_call_records())
        .unwrap_or_default())
}

/// Whether this build decodes `snapshot`, a cell's stored snapshot: the
/// broker's checkpoint, the envelope a resumed cell runs on with, and the VM
/// state the cell resumes from. The VM bytes are the worker's to read, so a
/// worker of `workers` answers for them through the VM's own decoder; the
/// inner `Err` is the account of whichever does not decode.
///
/// # Errors
///
/// The worker's failure to answer: nothing is known of the VM state.
pub(crate) async fn check_cell_snapshot(
    workers: &lash_vm_client::service::Service,
    snapshot: &str,
) -> Result<Result<(), String>, lash_core::RuntimeError> {
    use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
    use lash_vm_client::service::{Request, Response};

    let checkpoint: lash_vm_broker::Checkpoint = match serde_json::from_str(snapshot) {
        Ok(checkpoint) => checkpoint,
        Err(error) => return Ok(Err(error.to_string())),
    };
    if let Err(error) = envelope_of(&checkpoint) {
        return Ok(Err(error));
    }
    match workers
        .request_accounted(Request::CheckState {
            state: checkpoint.vm,
        })
        .await
        .map_err(lash_vm_client::PoolError::into_runtime_error)?
    {
        Response::StateCheck { refusal: None } => Ok(Ok(())),
        Response::StateCheck {
            refusal: Some(refusal),
        } => Ok(Err(refusal.to_string())),
        other => Err(lash_core::RuntimeError::new(
            lash_core::RuntimeErrorCode::VmWorkerFailed,
            format!("the worker answered a state check with {other:?}"),
        )),
    }
}

/// The refusal of the VM state a resumed cell was handed, if `failure` is
/// one: the state's structural refusal, or the worker's refusal to decode
/// its bytes. Another build wrote that state, so it is no failure of the
/// cell's program and no fault of this attempt.
pub(super) fn resumed_state_refusal(
    failure: &lash_vm_broker::BrokerFailure,
) -> Option<lash_vm_protocol::RunRefusal> {
    use lash_vm_broker::{BrokerFailure, CheckoutRefusal};
    use lash_vm_protocol::{InfrastructureOutcome, RunInput, RunRefusal};
    match failure {
        BrokerFailure::StateRefused { refusal } => Some(RunRefusal::State {
            refusal: refusal.clone(),
        }),
        BrokerFailure::WorkerLost {
            outcome: InfrastructureOutcome::RunRefused { refusal },
        }
        | BrokerFailure::Unavailable {
            refusal: CheckoutRefusal::Infrastructure(InfrastructureOutcome::RunRefused { refusal }),
        } if matches!(
            refusal,
            RunRefusal::State { .. }
                | RunRefusal::Undecodable {
                    input: RunInput::State { .. },
                    ..
                }
        ) =>
        {
            Some(refusal.clone())
        }
        _ => None,
    }
}

/// The envelope `snapshot`, a cell's stored snapshot, holds, if it holds
/// one.
fn stored_envelope(snapshot: &str) -> Result<Option<CellSegmentState>, String> {
    let checkpoint: lash_vm_broker::Checkpoint =
        serde_json::from_str(snapshot).map_err(|error| error.to_string())?;
    envelope_of(&checkpoint)
}

/// The envelope `checkpoint` holds, if it holds one.
fn envelope_of(
    checkpoint: &lash_vm_broker::Checkpoint,
) -> Result<Option<CellSegmentState>, String> {
    checkpoint
        .host
        .as_ref()
        .map(|host| rmp_serde::from_slice(&host.0).map_err(|error| error.to_string()))
        .transpose()
}

/// A cell resumed from its latest snapshot: the checkpoint the broker runs
/// on from, and the envelope its host runs on with.
pub(super) struct ResumedCell {
    pub from: lash_vm_broker::Checkpoint,
    pub envelope: CellSegmentState,
}

impl ResumedCell {
    /// The cell running `code` as its latest snapshot holds it, if it has
    /// one.
    pub(super) async fn latest(
        snapshots: &lash_vm_broker::DurableSnapshotStore,
        code: &str,
    ) -> Result<Option<Self>, String> {
        use lash_vm_broker::SnapshotStore as _;
        let Some((_, from)) = snapshots.latest().await.map_err(|refusal| refusal.0)? else {
            return Ok(None);
        };
        let host = from
            .host
            .as_ref()
            .ok_or_else(|| "the cell's snapshot has no envelope".to_owned())?;
        let envelope = CellSegmentState::decode_for(&host.0, code)?;
        Ok(Some(Self { from, envelope }))
    }
}

impl lash_core::store::DurableRecord for CellSegmentState {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::executor::RLM_SNAPSHOT_VERSION);
}

#[cfg(test)]
mod tests {
    /// FIG-5613: a worker's refusal to decode the state a resumed cell was
    /// handed is that state's refusal, which parks the cell's turn; a
    /// refusal of any other input of the run stays the run's own.
    #[test]
    fn only_a_refusal_of_the_resumed_state_is_the_cell_snapshots() {
        use lash_vm_protocol::{Detail, RunInput, RunRefusal, VmStateKind};
        let lost = |refusal: RunRefusal| lash_vm_broker::BrokerFailure::WorkerLost {
            outcome: refusal.into(),
        };
        let undecodable = RunRefusal::Undecodable {
            input: RunInput::State {
                kind: VmStateKind::Continuation,
            },
            detail: Detail::new("invalid type: map, expected a sequence"),
        };
        assert_eq!(
            super::resumed_state_refusal(&lost(undecodable.clone())),
            Some(undecodable)
        );
        for other in [
            RunRefusal::UnknownContext,
            RunRefusal::Undecodable {
                input: RunInput::Artifact,
                detail: Detail::new("truncated"),
            },
        ] {
            assert_eq!(super::resumed_state_refusal(&lost(other)), None);
        }
    }

    #[test]
    fn segment_bindings_refuse_untyped_payloads() {
        for value in [
            serde_json::json!(42),
            serde_json::json!({"tool": {"invented": true}}),
        ] {
            assert!(
                serde_json::from_value::<lash_vm_runtime::RecordedCellToolBindings>(value).is_err()
            );
        }
        assert!(
            serde_json::from_value::<
                std::collections::BTreeMap<String, crate::projection::bindings::RecordedProjection>,
            >(serde_json::json!({"binding": false}))
            .is_err()
        );
    }
}
