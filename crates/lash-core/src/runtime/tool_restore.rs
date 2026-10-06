//! The one owner of "install a persisted tool snapshot onto a session", and of
//! the run-time [`ToolSourcePolicy`] check.
//!
//! Every construction that restores a session's `ToolState` — a run's plugin
//! transition building the session's capabilities, a host restore command, a
//! persisted-state install, a resident re-sync — calls
//! [`install_persisted_tool_state`]. It reconciles the snapshot against the
//! live sources, classifies what no source resolved, and delivers that report
//! (as a typed value the caller keeps and as trace evidence). The runtime
//! keeps the report until its next turn reports it as
//! `TurnEvent::ToolRestoreReported` (FIG-5134).
//!
//! # Installs never refuse; a turn run's transition does
//!
//! An install commits the reconciled surface (through
//! `ToolRegistry::restore_state`) before anything could consult a policy, so
//! a refusal there would be a refusal after the registry had already changed.
//! No install refuses.
//!
//! An open builds no capabilities (FIG-4857), so it has nothing to restore and
//! nothing to refuse. [`ToolSourcePolicy::Require`] is a run policy: a turn
//! run's recorded plugin transition calls [`require_tool_sources`] on the
//! capabilities it built, before it publishes anything, and refuses the run
//! with the typed `RuntimeErrorCode::ToolSourcesUnavailable` when the restore
//! would lose a member. That reads the registry without changing it, so the
//! refused run leaves no durable write, restores no protocol session and
//! emits no `SessionRestored`.

use crate::{SessionId, ToolRestoreReport, ToolSourcePolicy, ToolState};

/// Which construction installed the snapshot. It names the site on the trace
/// evidence, so a host reading its traces can tell a run's construction from
/// a mid-turn resident re-sync without correlating timestamps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolRestoreSite {
    /// A session's capabilities built from persisted state: a run's plugin
    /// transition, or a construction whose head carries a recorded
    /// publication.
    SessionConstruction,
    /// A host's restore command applied on the command run.
    HostRestore,
    /// A persisted state envelope installed onto a live runtime.
    PersistedStateInstall,
    /// An invalidated resident session re-syncing from the durable head.
    ResidentReload,
}

impl ToolRestoreSite {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SessionConstruction => "session_construction",
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
    pub(crate) tracing: &'a crate::trace::TraceRuntime,
}

impl<'a> ToolRestoreContext<'a> {
    pub(crate) fn new(
        session_id: &'a SessionId,
        site: ToolRestoreSite,
        tracing: &'a crate::trace::TraceRuntime,
    ) -> Self {
        Self {
            session_id,
            site,
            tracing,
        }
    }
}

/// Install `snapshot` onto `registry` and return the report. It never
/// refuses for an unresolved id: see the module docs.
pub(crate) fn install_persisted_tool_state(
    registry: &crate::ToolRegistry,
    snapshot: ToolState,
    context: ToolRestoreContext<'_>,
) -> Result<ToolRestoreReport, crate::SessionError> {
    let report = registry
        .restore_state(snapshot)
        .map_err(|error| crate::SessionError::Protocol(format!("tool restore failed: {error}")))?;
    deliver(&report, &context);
    Ok(report)
}

/// The run-time [`ToolSourcePolicy`]: under `Require`, refuse when restoring
/// `snapshot` over `registry`'s sources would lose a member. The registry is
/// read, never changed. Parked opt-outs and superseded identities never
/// refuse, because neither describes a capability the session lost.
pub(crate) fn require_tool_sources(
    policy: ToolSourcePolicy,
    registry: &crate::ToolRegistry,
    snapshot: Option<&ToolState>,
    session_id: &SessionId,
) -> Result<(), crate::RuntimeEffectControllerError> {
    let (ToolSourcePolicy::Require, Some(snapshot)) = (policy, snapshot) else {
        return Ok(());
    };
    let report = registry.preview_restore(snapshot).map_err(|error| {
        crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::Plugin,
            format!("tool restore preview failed: {error}"),
        )
    })?;
    if report.has_lost_members() {
        return Err(
            crate::RuntimeEffectControllerError::tool_sources_unavailable(session_id, report),
        );
    }
    Ok(())
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
    // A restore rebuilds this process's resident tool surface: each one is
    // its own event, whichever attempt runs it.
    context.tracing.unreplayed(None).observe(|| {
        (
            lash_trace::TraceContext::default().for_session(context.session_id.clone()),
            lash_trace::TraceEvent::Custom {
                name: "tool_restore.report".to_string(),
                payload: trace_payload(report, context.site),
            },
        )
    });
}

pub(crate) fn trace_payload(
    report: &ToolRestoreReport,
    site: ToolRestoreSite,
) -> serde_json::Value {
    serde_json::json!({
        "site": site.as_str(),
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
