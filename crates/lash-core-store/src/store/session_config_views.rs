//! The session config views a runtime state yields: the sticky config a
//! commit writes, the execution view the running run executes under, the
//! snapshot the run's spec resolved against, and the recorded policy an
//! observer of the session reads.

/// The sticky config a commit writes to the session head: the config under
/// a recorded run view when one is installed, else the execution view.
pub fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    match state.authority.run_view() {
        Some(view) => view.sticky.clone(),
        None => execution_session_config_from_state(state),
    }
}

/// The configuration the running run was admitted under: the snapshot its
/// spec resolved against (FIG-3838), or the execution view when no recorded
/// run view is installed. A queued run's continuation is checked against it.
pub fn root_snapshot_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    match state.authority.run_view() {
        Some(view) => view.run.base().clone(),
        None => execution_session_config_from_state(state),
    }
}

/// The config used by the running run, including its recorded execution view.
pub fn execution_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    let mut config = crate::PersistedSessionConfig::from_policy(
        &state.policy,
        state.authority.tool_access.clone(),
    );
    config.plugin_config = state.authority.plugin_config.clone();
    config.prompt_plan = state.authority.prompt_plan.clone();
    config.config_revision = state.config_revision;
    config
}

/// The session's recorded policy: the resident policy, with the sticky
/// config's values in place of a recorded run view's while one is
/// installed. An observer of the session reads this, so a run's per-run
/// overrides never show as the session's (FIG-4529).
pub fn recorded_session_policy_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::SessionPolicy {
    let mut policy = state.policy.clone();
    if let Some(view) = state.authority.run_view() {
        crate::session_state::apply_persisted_config_to_policy(&mut policy, &view.sticky);
    }
    policy
}
