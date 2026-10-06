use super::*;

pub(crate) async fn run_generated_workload_for_fixture(
    workload: GeneratedWorkload,
    script_bundle_hash: &str,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    let trace =
        run_generated_workload(workload, script_bundle_hash, &SimShard::FULL.label()).await?;
    if !trace.oracle.is_passed() {
        return Err(FixedScriptRunnerError::Assertion(
            trace.oracle.message.clone(),
        ));
    }
    Ok(trace)
}

/// Execute a generated workload. Generated worlds ran on the Restate server
/// double's scheduler, which is gone; L9f (FIG-5184) rebuilds them on the
/// durable runtime, and until then every generated workload is refused.
pub(super) async fn run_generated_workload(
    workload: GeneratedWorkload,
    _script_bundle_hash: &str,
    _shard_label: &str,
) -> Result<SimulationTrace, FixedScriptRunnerError> {
    Err(FixedScriptRunnerError::Runtime(format!(
        "generated workload `{}` (seed {}) has no world to run on until L9f (FIG-5184) rebuilds generated worlds on the durable runtime",
        workload.profile, workload.seed
    )))
}
