use anyhow::Result;

/// The key the e2e deployment serves its one mock model under: the model
/// every e2e core and its host-started processes run.
pub const E2E_MODEL_KEY: &str = "e2e-mock";

pub fn e2e_model_metadata() -> Result<lash::ModelMetadata> {
    lash::ModelMetadata::builder("e2e-mock")
        .context_window_tokens(200_000)
        .build()
        .map_err(|err| anyhow::anyhow!(err))
}
