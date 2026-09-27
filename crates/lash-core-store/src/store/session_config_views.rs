//! The session config views a runtime state yields: the sticky config a
//! commit writes, the execution view the running root runs under, and the
//! snapshot the root's spec resolved against.

/// The sticky config a commit writes to the session head: the config under
/// a recorded root view when one is installed, else the execution view.
pub fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    if let Some(config) = &state.authority.committed_config {
        return (**config).clone();
    }
    execution_session_config_from_state(state)
}

/// The configuration the running root was admitted under: the snapshot its
/// spec resolved against (FIG-3838), or the execution view when no recorded
/// root view is installed. A queued run's continuation is checked against it.
pub fn root_snapshot_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    match &state.authority.root_snapshot {
        Some(snapshot) => (**snapshot).clone(),
        None => execution_session_config_from_state(state),
    }
}

/// The config used by the running root, including its recorded execution view.
pub fn execution_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    let mut config = crate::PersistedSessionConfig::from(&state.policy);
    config.tool_access = state.authority.tool_access.clone();
    config.subagent = state.authority.subagent.clone();
    config.protocol_turn_options = Some(state.protocol_turn_options.clone());
    config.config_revision = state.config_revision;
    config
}
