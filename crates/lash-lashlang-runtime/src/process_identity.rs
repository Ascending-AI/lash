use lash_sansio::core_support::Blake3DomainHasher;

use crate::LashlangProcessInput;

pub fn deterministic_lashlang_process_id(
    parent_start_seed: &str,
    start_site: &lashlang::LashlangExecutionCallSite,
    input: &LashlangProcessInput,
) -> Result<String, serde_json::Error> {
    let args = serde_json::to_string(&input.args)?;
    let occurrence = start_site.occurrence.to_string();
    let process_ref = lashlang::process_ref_key(&input.process_ref);
    let mut hasher = Blake3DomainHasher::new("lashlang-process-start/v2");
    for part in [
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
    Ok(format!(
        "process:lashlang:v2:blake3:{}",
        hasher.finalize_hex()
    ))
}
