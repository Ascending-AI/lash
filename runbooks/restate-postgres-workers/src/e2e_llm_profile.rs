use anyhow::Result;

/// The key the e2e deployment serves its one mock model under: the model
/// every e2e core and its host-started processes run.
pub const E2E_PROFILE_KEY: &str = "e2e-mock";

/// The spec the e2e harness creates every session from: its own default,
/// since a core keeps none.
pub fn e2e_session_spec() -> lash::SessionSpec {
    lash::SessionSpec::new(
        E2E_PROFILE_KEY,
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    )
}

pub fn e2e_llm_profile_metadata() -> Result<lash::LlmProfileMetadata> {
    lash::LlmProfileMetadata::builder("e2e-mock")
        .context_window_tokens(200_000)
        .build()
        .map_err(|err| anyhow::anyhow!(err))
}
