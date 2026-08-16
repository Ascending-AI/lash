use sha2::{Digest, Sha256};

use crate::LashlangProcessInput;

pub fn deterministic_lashlang_process_id(
    parent_start_seed: &str,
    start_site: &lashlang::LashlangExecutionCallSite,
    input: &LashlangProcessInput,
) -> Result<String, serde_json::Error> {
    let args = serde_json::to_string(&input.args)?;
    let occurrence = start_site.occurrence.to_string();
    let process_ref = lashlang::process_ref_key(&input.process_ref);
    let mut hasher = Sha256::new();
    for part in [
        "lashlang-process-start:v1",
        parent_start_seed,
        start_site.site.node_id.as_str(),
        occurrence.as_str(),
        input.module_ref.as_str(),
        process_ref.as_str(),
        input.host_requirements_ref.as_str(),
        input.process_name.as_str(),
        args.as_str(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let hash = format!("{:x}", hasher.finalize());
    Ok(format!("process:lashlang:sha256:{hash}"))
}
