//! `finish`, the control tool a `TerminalRequired` standard session is
//! offered (DESIGN §2): [`lash_core::finish_tool`] over any JSON object, so
//! its call ends the turn with its whole input, and returns nothing. A host
//! that wants a typed answer offers its own finish tool, which takes this
//! one's place ([`lash_core::suppress_default_finish`]).

/// The name the model calls `finish` by: what a turn it ended records as
/// its finishing tool.
pub(crate) const FINISH_TOOL_NAME: &str = "finish";

/// The default finish's id.
pub(crate) fn finish_tool_id() -> lash_core::ToolId {
    lash_core::finish_tool_id(FINISH_TOOL_NAME)
}

/// The default finish: a provider's tool input is a JSON object, so the
/// answer is any object, whole.
#[expect(clippy::expect_used, reason = "a fixed object schema is admitted")]
pub(crate) fn finish_tool_provider() -> lash_core::FinishToolProvider {
    let mut definition = lash_core::finish_tool(
        FINISH_TOOL_NAME,
        lash_core::JsonSchema::admit(serde_json::json!({ "type": "object" }))
            .expect("a fixed object schema is admitted"),
    );
    definition.manifest.description = "End the turn with this call's arguments, whole, as its answer. Call it on its own once every other call has returned; it returns nothing. A prose reply does not end this turn.".to_owned();
    lash_core::FinishToolProvider::new([definition])
}
