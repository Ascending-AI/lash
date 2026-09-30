use super::json_to_flow_value;
use std::collections::BTreeSet;

pub(super) fn process_handle_names(globals: &lashlang::Record) -> BTreeSet<String> {
    globals
        .iter()
        .filter_map(|(name, value)| {
            value
                .as_record()
                .is_some_and(lashlang::is_process_handle)
                .then_some(name.to_string())
        })
        .collect()
}

pub(super) fn apply_global_defaults(
    rlm: &mut lash_vm_client::RemoteState,
    patch: &lash_rlm_types::RlmGlobalsPatchPluginBody,
    protected_names: &BTreeSet<String>,
) -> Result<Vec<String>, String> {
    if patch.set_default.is_empty() {
        return Ok(Vec::new());
    }
    for key in patch.set_default.keys() {
        if is_reserved_global_name(key) || protected_names.contains(key) {
            return Err(format!(
                "`{key}` is a read-only projected host binding; choose a different Lashlang variable name for `set_default`"
            ));
        }
    }
    let before = rlm
        .binding_names()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    rlm.defaults(
        patch
            .set_default
            .iter()
            .map(|(key, value)| (key.clone(), json_to_flow_value(value.clone())))
            .collect(),
        protected_names.clone(),
    )?;
    Ok(rlm
        .binding_names()
        .filter(|name| !before.contains(*name))
        .map(str::to_string)
        .collect())
}

pub(super) fn is_reserved_global_name(key: &str) -> bool {
    key == "history"
}
