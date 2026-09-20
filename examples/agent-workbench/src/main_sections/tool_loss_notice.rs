//! Rendering tool loss to the workbench's user (FIG-3367).
//!
//! An open reports which persisted tools no live source resolves. This is the
//! workbench's answer to that report: the one class that means a capability is
//! gone becomes a chat row the user reads.

use super::*;
use lash::SessionId;

impl AppState {
    /// Show the user which of this session's tools the open could not find.
    ///
    /// The open reports tool loss as a typed value (FIG-3367); the workbench
    /// renders the one class that means a capability is gone. The row carries a
    /// deterministic id derived from the lost ids, so the identified product
    /// event dedupes it: every route opens the session, and a chat that
    /// repeated the warning per request would be unreadable.
    pub(crate) async fn render_tool_loss(
        &self,
        session_id: &SessionId,
        session: &lash::LashSession,
    ) {
        let Some(report) = session.tool_restore_report().await else {
            return;
        };
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
