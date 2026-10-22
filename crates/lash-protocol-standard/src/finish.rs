//! `finish`, the control tool a `TerminalRequired` standard session is
//! offered (DESIGN §2): [`lash_core::finish_tool`] over any JSON object, so
//! its call ends the turn with its whole input, and returns nothing. A host
//! that wants a typed answer offers its own finish tool, which takes this
//! one's place ([`lash_core::suppress_default_finish`]).

/// The operation the default finish is performed as: what a turn it ended
/// records as its finishing tool. The model calls it by its Standard-mode
/// name, `control__finish`.
pub(crate) const FINISH_OPERATION: &str = "control.finish";

/// The default finish's id.
#[expect(clippy::expect_used, reason = "a fixed canonical operation")]
pub(crate) fn finish_tool_id() -> lash_vocabulary::tool_contract::ToolId {
    lash_core::finish_tool_id(
        &lash_vocabulary::tool_contract::IntoOperation::into_operation(FINISH_OPERATION)
            .expect("a canonical operation"),
    )
}

/// The provider of the default finish, registered in every standard
/// session: it offers `finish` only to a session whose turns only a control
/// call ends. A provider's tool input is a JSON object, so the answer is any
/// object, whole.
#[expect(
    clippy::expect_used,
    reason = "a fixed object schema and canonical operation are admitted"
)]
pub(crate) fn finish_tool_provider(
    termination: lash_core::TerminationMode,
) -> lash_core::FinishToolProvider {
    if termination.prose_ends_turn() {
        return lash_core::FinishToolProvider::new([]);
    }
    let definition = lash_core::finish_tool(
        FINISH_OPERATION,
        "End the turn with this call's arguments, whole, as its answer. Call it on its own once every other call has returned; it returns nothing. A prose reply does not end this turn.",
        lash_vocabulary::json_schema::JsonSchema::admit(serde_json::json!({ "type": "object" }))
            .expect("a fixed object schema is admitted"),
    )
    .expect("a canonical operation");
    lash_core::FinishToolProvider::new([definition])
}
