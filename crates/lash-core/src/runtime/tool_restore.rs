//! The one owner of "install a persisted tool snapshot onto a session".
//!
//! Every construction that restores a session's `ToolState` — a cold open, an
//! explicit host restore, a persisted-state install, a resident re-sync — calls
//! [`install_persisted_tool_state`]. It reconciles the snapshot against the
//! live sources, classifies what no source resolved, and delivers that report
//! to the host (as a typed value the caller keeps and as trace evidence).
//!
//! Before FIG-3367 the four sites each called `ToolRegistry::restore_state`
//! themselves and two of them turned the answer into a `tracing::warn!`, one
//! dropped it silently, and one returned it. An orphan at open is the one
//! moment lash knows a session has lost tools, so that knowledge is a typed
//! fact now, not a log line.
//!
//! # Only an open may refuse
//!
//! [`ToolSourcePolicy`] is an *open* policy: opening a session is the host's
//! claim that it can run that session, so `Require` answers that claim with a
//! typed refusal. The three installs onto an already-live runtime — the host's
//! `restore_tool_state`, a persisted-state install, and the resident re-sync —
//! are never refusals. They always tolerate, retain the report and hand it
//! back.
//!
//! That is not a preference, it is what the mutation order allows.
//! [`install_persisted_tool_state`] commits the reconciled surface (through
//! `ToolRegistry::restore_state`) *before* any policy is consulted, so a
//! refusal is a refusal after the registry has already changed. At open that is
//! safe and deliberate: the half-built runtime, its registry included, is
//! dropped with the error and nothing the host can reach ever saw it. On a live
//! runtime the same refusal would leave the session holding a reconciled
//! registry with a stale tool catalog, no refreshed plugin state and no
//! retained report — a worse outcome than the degraded session `Require` exists
//! to prevent, arrived at without anyone asking to open anything.
//!
//! [`ToolRestoreAuthority`] makes that structural rather than a rule three call
//! sites have to remember: a live install cannot name a policy, because the
//! type has nowhere to put one.

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

/// Whether this install is allowed to refuse.
///
/// See the module docs: only an open may, and only because a refused open's
/// runtime is discarded whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolRestoreAuthority {
    /// A session being opened, under the host's tool-source policy.
    Open(ToolSourcePolicy),
    /// An install onto an already-live runtime. It has no policy field on
    /// purpose: this authority cannot refuse.
    LiveInstall,
}

impl ToolRestoreAuthority {
    fn refuses(self, report: &ToolRestoreReport) -> bool {
        match self {
            Self::Open(ToolSourcePolicy::Require) => report.has_lost_members(),
            Self::Open(ToolSourcePolicy::Tolerate) | Self::LiveInstall => false,
        }
    }

    fn trace_fields(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Open(ToolSourcePolicy::Tolerate) => ("open", Some("tolerate")),
            Self::Open(ToolSourcePolicy::Require) => ("open", Some("require")),
            Self::LiveInstall => ("live_install", None),
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
    pub(crate) authority: ToolRestoreAuthority,
    pub(crate) tracing: &'a crate::runtime::RuntimeTracingConfig,
    pub(crate) clock: &'a dyn crate::Clock,
}

impl<'a> ToolRestoreContext<'a> {
    /// The open path, where the host's policy applies.
    pub(crate) fn for_open(
        session_id: &'a SessionId,
        policy: ToolSourcePolicy,
        tracing: &'a crate::runtime::RuntimeTracingConfig,
        clock: &'a dyn crate::Clock,
    ) -> Self {
        Self {
            session_id,
            site: ToolRestoreSite::SessionOpen,
            authority: ToolRestoreAuthority::Open(policy),
            tracing,
            clock,
        }
    }

    /// An install onto a live runtime. It takes no policy, so no live site can
    /// acquire the authority to refuse by passing one.
    pub(crate) fn for_live_install(
        session_id: &'a SessionId,
        site: ToolRestoreSite,
        tracing: &'a crate::runtime::RuntimeTracingConfig,
        clock: &'a dyn crate::Clock,
    ) -> Self {
        debug_assert_ne!(
            site,
            ToolRestoreSite::SessionOpen,
            "an open installs through ToolRestoreContext::for_open"
        );
        Self {
            session_id,
            site,
            authority: ToolRestoreAuthority::LiveInstall,
            tracing,
            clock,
        }
    }
}

/// Install `snapshot` onto `registry` and return the report.
///
/// An open under [`ToolSourcePolicy::Require`] answers a report with lost
/// members as a typed refusal ([`SessionError::ToolSourcesUnavailable`])
/// carrying the report; parked opt-outs and superseded identities never
/// refuse, because neither describes a capability the session lost. A live
/// install never refuses at all — see the module docs for why the mutation
/// order makes that the only safe reading.
pub(crate) fn install_persisted_tool_state(
    registry: &crate::ToolRegistry,
    snapshot: ToolState,
    context: ToolRestoreContext<'_>,
) -> Result<ToolRestoreReport, SessionError> {
    // NOTE: this commits the reconciled surface. Every refusal below is a
    // refusal *after* that mutation, which is why only an open — whose runtime
    // the caller drops with the error — may issue one.
    let report = registry
        .restore_state(snapshot)
        .map_err(|error| SessionError::Protocol(format!("tool restore failed: {error}")))?;
    deliver(&report, &context);
    if context.authority.refuses(&report) {
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
            payload: trace_payload(report, context.site, context.authority),
        },
        context.clock,
    );
}

pub(crate) fn trace_payload(
    report: &ToolRestoreReport,
    site: ToolRestoreSite,
    authority: ToolRestoreAuthority,
) -> serde_json::Value {
    let (authority_label, policy) = authority.trace_fields();
    serde_json::json!({
        "site": site.as_str(),
        // Which reading applied: an `open` consults the host's policy, a
        // `live_install` cannot refuse and carries no policy.
        "authority": authority_label,
        "policy": policy,
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
