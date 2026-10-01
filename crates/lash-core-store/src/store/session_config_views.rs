//! The session config views a runtime state yields: the sticky config a
//! commit writes, the execution view the running root runs under, the
//! snapshot the root's spec resolved against, and the recorded policy an
//! observer of the session reads.

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
    config.plugin_config = state.authority.plugin_config.clone();
    config.config_revision = state.config_revision;
    config
}

/// The session's recorded policy: the resident policy, with the sticky
/// config's values in place of a recorded root view's while one is
/// installed. An observer of the session reads this, so a root's per-run
/// overrides never show as the session's (FIG-4529).
pub fn recorded_session_policy_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::SessionPolicy {
    let mut policy = state.policy.clone();
    if let Some(config) = &state.authority.committed_config {
        crate::session_state::apply_persisted_config_to_policy(&mut policy, config);
    }
    policy
}
