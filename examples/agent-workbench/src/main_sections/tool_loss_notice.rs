//! Rendering tool loss to the workbench's user (FIG-3367, FIG-5134).
//!
//! A run reports which persisted tools no live source resolves when its
//! transition restored the session's tool state. This is the workbench's
//! answer to that report: the one class that means a capability is gone
//! becomes a chat row the user reads.

use super::*;
use lash::SessionId;

impl AppState {
    /// Show the user which of this session's tools a run could not find.
    ///
    /// The run reports tool loss as a typed value on its output (FIG-5134);
    /// the workbench renders the one class that means a capability is gone.
    /// The row carries a deterministic id derived from the lost ids, so the
    /// identified product event dedupes it: every run built on the same
    /// sources reports the same loss, and a chat that repeated the warning
    /// per run would be unreadable.
    pub(crate) fn render_tool_loss(
        &self,
        session_id: &SessionId,
        report: &lash::tools::ToolRestoreReport,
    ) {
        if !report.has_lost_members() {
            return;
        }
        let lost = report
            .lost_members
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        self.trace_for_session(
            session_id,
            "session.tool_loss",
            json!({
                "session_id": session_id,
                "lost_members": lost,
                "parked_opt_outs": report
                    .parked_opt_outs
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
            }),
        );
        self.push_message_with_id_for_session(
            session_id,
            format!("workbench-tool-loss:{}", lost.join(",")),
            "system",
            format!(
                "Tools unavailable in this session: {}. No source registered here \
                 resolves them; they return when their source does.",
                lost.join(", ")
            ),
        );
    }
}
