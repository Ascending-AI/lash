use std::sync::Arc;

#[derive(Clone)]
pub struct ProtocolBuildInput {
    pub tool_catalog: Arc<crate::ToolCatalog>,
    pub plugin_extensions: crate::PluginExtensions,
    pub trigger_events: crate::TriggerEventCatalog,
    pub extra_prompt_contributions: Vec<crate::PromptContribution>,
    /// The fleet's writer-version table, resolved from the `F` the session's
    /// store recorded (FIG-3796): preamble builders hand it to the turn
    /// machine so drivers stamp durable envelopes at the versions the fleet
    /// writes.
    pub writer_formats: Arc<dyn lash_sansio::WriterFormats>,
}

/// The [`lash_sansio::WriterFormats`] a session installs: resolves each
/// surface's writer version through its store's recorded fleet format
/// (FIG-3796).
pub struct FleetWriterFormats(pub crate::FleetFormat);

impl lash_sansio::WriterFormats for FleetWriterFormats {
    fn writer_version(&self, constant: &'static str, build_newest: u32) -> u32 {
        self.0
            .writer_version(crate::store::SurfaceFormat::of(constant, build_newest))
    }
}
