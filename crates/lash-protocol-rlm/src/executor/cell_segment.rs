//! The state of a code cell stopped at a segment boundary inside it
//! (FIG-4739).
//!
//! A Run on a draining build hands over at its next quiet point, and a cell
//! parked on a durable wait is one: the worker has already captured the VM
//! there and released its slot (FIG-4159). The cell's parent then holds the
//! rest of the cell in ledgers, and this envelope carries every one of them
//! to the segment that resumes the cell, so the resumed cell issues the wait
//! it stopped on again and runs on as if it had never stopped.
//!
//! What a process segment hands over, a cell hands over too: the VM
//! continuation, the command ordinals and the started children. The kernel's
//! Run continuation carries the opener's incorporation ledger and held groups.
//! A cell owns more than a process body does, and all of it is plain data:
//! its prints and printed images,
//! the calls it made, the tool calls it counts against `max_tool_calls`, and
//! everything it linked against, which its journal recorded before its first
//! effect — the binding set, the projected bindings, the host environment
//! and the deferred grants. Those records live in the journal of the segment
//! that wrote them, so the envelope carries their contents and the resumed
//! cell links against them without journaling them again.
//!
//! One thing cannot be carried: a host descriptor a tool outcome exported
//! into the run's projection registry. A run holding one is never handed
//! over: `HandOverRefusal::ExportedHostDescriptors` keeps it on its admitted
//! build, whose drain still counts the turn until it ends.

use std::collections::BTreeMap;

use lash_core::RuntimeExecutionContext;

use super::host_bridge::CellHostLedgers;

/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_CELL_SEGMENT_CODE_DOMAIN_VERSION, code_digest))
const LASH_RLM_CELL_SEGMENT_CODE_DOMAIN_VERSION: &str = "lash-rlm-cell-segment-code/v1";

/// version_surface = "coexist"
/// version_guard(items(LASH_RLM_CELL_PROJECTION_NAMESPACE_DOMAIN_VERSION, projection_namespace))
const LASH_RLM_CELL_PROJECTION_NAMESPACE_DOMAIN_VERSION: &str =
    "lash-rlm-cell-projection-namespace/v1";

/// The namespace a fresh execution of a cell mints its projection tokens
/// under: derived from the cell's own replay namespace, so every execution
/// of the same cell — its first, a replay, a retry — mints the same tokens
/// and captures the same state at a boundary inside it. A random one would
/// make the boundary's commit content differ between an execution and its
/// replay, and the replay's commit would be refused as other content under
/// the same operation.
pub(super) fn projection_namespace(
    cell: &lash_lashlang_runtime::LashlangReplayNamespace,
) -> String {
    lash_sansio::core_support::blake3_domain_hash_hex(
        LASH_RLM_CELL_PROJECTION_NAMESPACE_DOMAIN_VERSION,
        cell.seal().as_bytes(),
    )
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(super) struct RecordedPrint(#[serde(with = "lashlang::effect_value")] pub lashlang::Value);

/// A cell stopped at a segment boundary, as its successor segment resumes it.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CellSegmentState {
    /// The logical Run that owns this continuation across physical segments.
    pub owner: lash_core_worker::TurnAddress,
    /// The digest of the cell's source, checked after Run ownership.
    pub code: String,
    /// The worker's continuation, opaque to the parent (ADR 0123).
    pub vm: lash_vm_protocol::OpaqueVmState,
    /// The run's issue-ordinal state. The command the cell stopped on has
    /// returned its ordinal, so the resumed cell issues it under the same key.
    pub ordinals: lash_lashlang_runtime::LashlangRunOrdinals,
    /// Native call identities keep the admitting cell across physical turns.
    pub cell_opener: lash_core::EffectOpener,
    pub cell_execution: String,
    /// The namespace the run's projection tokens name.
    pub projection_namespace: Option<String>,
    /// The session's projected bindings as the cell recorded them.
    pub projected_bindings: BTreeMap<String, crate::projection::bindings::RecordedProjection>,
    /// The host environment the cell linked against.
    pub host_environment: lashlang::LashlangHostEnvironment,
    /// The cell's journaled ambient binding set (FIG-3587).
    pub cell_bindings: lash_lashlang_runtime::RecordedCellToolBindings,
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

    /// Another Run or another cell starts fresh, even with identical source.
    pub(super) fn decode_for(
        bytes: &[u8],
        owner: Option<&lash_core_worker::TurnAddress>,
        code: &str,
    ) -> Result<Option<Self>, String> {
        let state: Self = rmp_serde::from_slice(bytes).map_err(|error| error.to_string())?;
        Ok((owner == Some(&state.owner) && state.code == Self::code_digest(code)).then_some(state))
    }

    /// The parent ledgers of `ctx` a boundary hands over.
    pub(super) fn restore_context(&self, ctx: &RuntimeExecutionContext<'_>) {
        ctx.restore_started_process_ids(&self.started_process_ids);
    }
}

impl lash_core::store::DurableRecord for CellSegmentState {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::executor::RLM_SNAPSHOT_VERSION);
}

#[cfg(test)]
mod tests {
    #[test]
    fn segment_bindings_refuse_untyped_payloads() {
        for value in [
            serde_json::json!(42),
            serde_json::json!({"tool": {"invented": true}}),
        ] {
            assert!(
                serde_json::from_value::<lash_lashlang_runtime::RecordedCellToolBindings>(value)
                    .is_err()
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
