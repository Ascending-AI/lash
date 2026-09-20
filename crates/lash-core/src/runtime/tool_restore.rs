//! The one owner of "install a persisted tool snapshot onto a session".
//!
//! Every construction that restores a session's `ToolState` — a cold open, an
//! explicit host restore, a persisted-state install, a resident re-sync — calls
//! [`install_persisted_tool_state`]. It reconciles the snapshot against the
//! live sources, classifies what no source resolved, delivers that report to
//! the host (as a typed value the caller keeps and as trace evidence), and
//! applies the host's [`ToolSourcePolicy`].
//!
//! Before FIG-3367 the four sites each called `ToolRegistry::restore_state`
//! themselves and two of them turned the answer into a `tracing::warn!`, one
//! dropped it silently, and one returned it. An orphan at open is the one
//! moment lash knows a session has lost tools, so that knowledge is a typed
//! fact now, not a log line.

use crate::{SessionError, SessionId, ToolRestoreReport, ToolSourcePolicy, ToolState};

/// Which construction installed the snapshot. It names the site on the trace
/// evidence, so a host reading its traces can tell a cold open from a
/// mid-turn resident re-sync without correlating timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolRestoreSite {
    /// A runtime being built from persisted state (`from_host_state`): every
    /// builder open, resume, managed-child materialisation, queued-work driver
    /// construction and remote-host open funnels through here.
    SessionOpen,
    /// A host-driven `restore_tool_state` on a live runtime.
    HostRestore,
    /// A persisted state envelope installed onto a live runtime.
    PersistedStateInstall,
    /// An invalidated resident session re-syncing from the durable head.
    ResidentReload,
}

impl ToolRestoreSite {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SessionOpen => "session_open",
            Self::HostRestore => "host_restore",
            Self::PersistedStateInstall => "persisted_state_install",
            Self::ResidentReload => "resident_reload",
        }
    }
}

/// Everything the installer needs that is not the snapshot itself.
///
/// The pieces are passed explicitly rather than as `&LashRuntime` because
/// three of the four call sites already hold a mutable borrow of the runtime's
/// session when they install.
pub(crate) struct ToolRestoreContext<'a> {
    pub(crate) session_id: &'a SessionId,
    pub(crate) site: ToolRestoreSite,
    pub(crate) policy: ToolSourcePolicy,
    pub(crate) tracing: &'a crate::runtime::RuntimeTracingConfig,
    pub(crate) clock: &'a dyn crate::Clock,
}

/// Install `snapshot` onto `registry` and return the report.
///
/// Under [`ToolSourcePolicy::Require`] a report with lost members is a typed
/// refusal ([`SessionError::ToolSourcesUnavailable`]) carrying the report.
/// Parked opt-outs and superseded identities never refuse: neither describes a
/// capability the session lost.
pub(crate) fn install_persisted_tool_state(
    registry: &crate::ToolRegistry,
    snapshot: ToolState,
    context: ToolRestoreContext<'_>,
) -> Result<ToolRestoreReport, SessionError> {
    let report = registry
        .restore_state(snapshot)
        .map_err(|error| SessionError::Protocol(format!("tool restore failed: {error}")))?;
    deliver(&report, &context);
    if context.policy == ToolSourcePolicy::Require && report.has_lost_members() {
        return Err(SessionError::ToolSourcesUnavailable {
            session_id: context.session_id.clone(),
            report: Box::new(report),
        });
    }
    Ok(report)
}

/// Deliver the report to the host: warn only for capability loss, and emit the
/// full three-way classification as trace evidence on every install.
fn deliver(report: &ToolRestoreReport, context: &ToolRestoreContext<'_>) {
    if report.has_lost_members() {
        tracing::warn!(
            session_id = %context.session_id,
            site = context.site.as_str(),
            lost_members = ?report.lost_members,
            "session restored without tools a registered source resolves: they \
             remain non-members until their source returns"
        );
    }
    if report.is_clean() {
        return;
    }
    crate::trace::emit_trace(
        &context.tracing.trace_sink,
        &context.tracing.trace_context,
        lash_trace::TraceContext::default().for_session(context.session_id.clone()),
        lash_trace::TraceEvent::Custom {
            name: "tool_restore.report".to_string(),
            payload: trace_payload(report, context.site, context.policy),
        },
        context.clock,
    );
}

pub(crate) fn trace_payload(
    report: &ToolRestoreReport,
    site: ToolRestoreSite,
    policy: ToolSourcePolicy,
) -> serde_json::Value {
    serde_json::json!({
        "site": site.as_str(),
        "policy": match policy {
            ToolSourcePolicy::Tolerate => "tolerate",
            ToolSourcePolicy::Require => "require",
        },
        "generation": report.generation,
        "lost_members": report
            .lost_members
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "parked_opt_outs": report
            .parked_opt_outs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        "superseded_identities": report
            .superseded_identities
            .iter()
            .map(|superseded| {
                serde_json::json!({
                    "retired_id": superseded.retired_id.to_string(),
                    "live_id": superseded.live_id.to_string(),
                    "name": superseded.name,
                })
            })
            .collect::<Vec<_>>(),
    })
}
