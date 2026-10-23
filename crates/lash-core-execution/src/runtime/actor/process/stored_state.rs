//! Read committed engine state for operator inspection; the owner uses its continuation.
use lash_durable::{DurableError, DurableReads};
use lash_execution_contracts::process::EngineState;
use lash_vocabulary::identity::ProcessId;

/// The engine state `process` holds, in the format it was written in, read
/// without a fence; `None` before its first transition and after its end.
/// For an operator's survey: an owner never decides anything from it.
///
/// # Errors
///
/// The store's, and a stored root that does not load as the process's
/// state.
pub async fn stored_engine_state(
    reads: &dyn DurableReads,
    process: &ProcessId,
    limits: lash_core_store::checkpoint_graph::DecodeLimits,
) -> Result<Option<EngineState>, DurableError> {
    Ok(
        super::state_graph::read_engine_state(reads, process, limits)
            .await?
            .map(|(_, _, state)| state),
    )
}
