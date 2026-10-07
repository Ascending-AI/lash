//! Tool hooks compose as transforms, then checks (ADR 0128).
//!
//! For one admitted call the sequence is fixed. Argument transforms chain
//! once, in recorded registration order. The arguments are validated and the
//! bound provider prepares the call. Every before-check then inspects that
//! one immutable prepared call, and their replies reduce by strength:
//! `AbortRun > Deny/Cancel > Cached > Allow`, ties broken by UTF-8 plugin id
//! and then callback key. After the body (or a cached success), result
//! transforms chain over the original result and the preceding candidate,
//! then every after-check inspects the one final candidate and reduces the
//! same way. A check never replaces a value, and a transform never decides.
//!
//! The recorded shapes and the reducer are S01's
//! [`lash_core_store::tool_run::tool_hooks`]; these are the callback-facing
//! values the runtime reduces with it.

use std::sync::Arc;

pub use lash_core_store::tool_run::{
    AttemptOrdinal, CheckRank, RankedVerdict, ToolHookOccurrence, ToolHookPhase,
};
use lash_core_store::tool_run::{AttributedVerdict, CheckRecord};

use super::*;

/// What every tool hook of one call sees: the call's fixed identity, the
/// configuration it runs under, and the arguments the model sent.
///
/// The context lends no writable service: a tool hook reads, and changes
/// nothing but the value it returns.
#[derive(Clone)]
pub struct ToolHookContext {
    /// Who the call runs for: a session, or a process runtime.
    pub owner: crate::RuntimeOwner,
    /// The durable identity of the call, fixed at admission.
    pub call_id: crate::ToolCallId,
    pub tool_id: crate::ToolId,
    pub tool_name: String,
    /// The plugin configuration this hook runs under (FIG-4379): the running
    /// run's admitted configuration and its revision, a process's captured
    /// one, or the head's outside a run — never today's head inside a run.
    pub plugin_config: super::AdmittedPluginConfig,
    pub argument_projection: crate::ToolArgumentProjectionPolicy,
    pub turn_context: crate::TurnContext,
    pub(crate) sessions: Arc<dyn SessionStateService>,
}

impl ToolHookContext {
    /// A snapshot of the session the call runs in; a process runtime has
    /// none and is refused.
    pub async fn session_snapshot(&self) -> Result<SessionSnapshot, PluginError> {
        self.sessions
            .snapshot_session(require_session_owner(&self.owner, "hook_session_snapshot")?)
            .await
    }
}

/// The admitted, provider-prepared call: what executes, sealed. A check
/// reads it; nothing can change it after the checks run.
#[derive(Clone, Debug)]
pub struct PreparedCallReadView(Arc<crate::PreparedToolCall>);

impl PreparedCallReadView {
    pub(crate) fn new(prepared: crate::PreparedToolCall) -> Self {
        Self(Arc::new(prepared))
    }

    pub fn call_id(&self) -> &crate::ToolCallId {
        &self.0.call_id
    }

    pub fn provider_call_id(&self) -> Option<&str> {
        self.0.provider_call_id.as_deref()
    }

    pub fn tool_id(&self) -> &crate::ToolId {
        &self.0.tool_id
    }

    pub fn tool_name(&self) -> &str {
        &self.0.tool_name
    }

    /// The final arguments the body executes with.
    pub fn args(&self) -> &serde_json::Value {
        &self.0.args
    }

    /// The payload the provider sealed at preparation.
    pub fn prepared_payload(&self) -> &serde_json::Value {
        &self.0.prepared_payload
    }

    /// The sealed call, for the body to execute.
    pub(crate) fn into_prepared(self) -> crate::PreparedToolCall {
        Arc::unwrap_or_clone(self.0)
    }
}

/// An argument transform's input: the arguments the model sent, and the
/// arguments the preceding transform returned (the model's, for the first).
#[derive(Clone)]
pub struct ToolArgsTransformInput {
    pub context: ToolHookContext,
    pub original: Arc<serde_json::Value>,
    pub current: serde_json::Value,
}

/// A before-check's input: the arguments the model sent, and the one
/// prepared call every check inspects.
#[derive(Clone)]
pub struct ToolArgsCheckInput {
    pub context: ToolHookContext,
    pub original_args: Arc<serde_json::Value>,
    pub prepared: PreparedCallReadView,
}

/// A tool result as hooks see it. The body's Run control and intents are
/// outside it: a transform cannot rewrite them and a cache cannot forge them.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResultCandidate {
    pub outcome: crate::ToolCallOutcome,
    pub view: Option<crate::ToolView>,
    pub projection_value: Option<serde_json::Value>,
    /// What a host shows of the call; never rendered to a model.
    pub display: Option<crate::ToolDisplay>,
}

impl ToolResultCandidate {
    pub(crate) fn split(output: crate::ToolCallOutput) -> (Self, Option<crate::ToolControl>) {
        let crate::ToolCallOutput {
            outcome,
            control,
            view,
            projection_value,
            display,
        } = output;
        (
            Self {
                outcome,
                view,
                projection_value,
                display,
            },
            control,
        )
    }

    pub(crate) fn into_output(self, control: Option<crate::ToolControl>) -> crate::ToolCallOutput {
        crate::ToolCallOutput {
            outcome: self.outcome,
            control,
            view: self.view,
            projection_value: self.projection_value,
            display: self.display,
        }
    }
}

/// A result transform's input: the immutable original result and the
/// candidate the preceding transform returned (the original, for the first).
#[derive(Clone)]
pub struct ToolResultTransformInput {
    pub context: ToolHookContext,
    pub occurrence: ToolHookOccurrence,
    pub prepared: PreparedCallReadView,
    pub original: Arc<ToolResultCandidate>,
    pub current: ToolResultCandidate,
}

/// An after-check's input: the one final candidate every check inspects.
#[derive(Clone)]
pub struct ToolResultCheckInput {
    pub context: ToolHookContext,
    pub occurrence: ToolHookOccurrence,
    pub prepared: PreparedCallReadView,
    pub original: Arc<ToolResultCandidate>,
    pub final_result: Arc<ToolResultCandidate>,
}

/// A successful result a before-check supplies in place of the body. It is
/// data only: it carries no intents, resolver, process start or Run control,
/// and it runs through the result transforms and after-checks like a body
/// result.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedToolSuccess {
    pub value: crate::ToolValue,
    pub view: Option<crate::ToolView>,
    pub projection_value: Option<serde_json::Value>,
    pub display: Option<crate::ToolDisplay>,
}

impl CachedToolSuccess {
    pub fn new(value: crate::ToolValue) -> Self {
        Self {
            value,
            view: None,
            projection_value: None,
            display: None,
        }
    }

    fn into_candidate(self) -> ToolResultCandidate {
        ToolResultCandidate {
            outcome: crate::ToolCallOutcome::Success(self.value),
            view: self.view,
            projection_value: self.projection_value,
            display: self.display,
        }
    }
}

/// A before-check's one verdict over the prepared call.
#[derive(Clone, Debug, PartialEq)]
pub enum BeforeToolDecision {
    Allow,
    /// Serve this success instead of running the body.
    Cached(CachedToolSuccess),
    /// Fail the call.
    Deny(crate::ToolFailure),
    /// Cancel the call.
    Cancel(crate::ToolCancellation),
    /// Fail the call and stop the owning logical Run.
    AbortRun(PluginAbort),
}

/// An after-check's one verdict over the final result. It has no variant
/// that carries a result: an after-check cannot replace one, and recovery
/// belongs in a result transform.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum AfterToolDecision {
    #[default]
    Allow,
    Deny(crate::ToolFailure),
    Cancel(crate::ToolCancellation),
    AbortRun(PluginAbort),
}

impl RankedVerdict for BeforeToolDecision {
    fn rank(&self) -> CheckRank {
        match self {
            Self::Allow => CheckRank::Allow,
            Self::Cached(_) => CheckRank::CachedSuccess,
            Self::Deny(_) | Self::Cancel(_) => CheckRank::DenyOrCancel,
            Self::AbortRun(_) => CheckRank::AbortRun,
        }
    }
}

impl RankedVerdict for AfterToolDecision {
    fn rank(&self) -> CheckRank {
        match self {
            Self::Allow => CheckRank::Allow,
            Self::Deny(_) | Self::Cancel(_) => CheckRank::DenyOrCancel,
            Self::AbortRun(_) => CheckRank::AbortRun,
        }
    }
}

/// An after-check's reply: its verdict, plus the runtime events it declares
/// and the commands it returns against its own plugin state namespace. The runtime applies those once the check phase
/// completes, whatever the verdict; the commands publish with the check's
/// recorded decision (K10).
#[derive(Clone, Debug, Default)]
pub struct AfterToolContributions {
    pub verdict: AfterToolDecision,
    pub events: Vec<PluginRuntimeEvent>,
    pub state: super::StateCommands,
}

impl From<AfterToolDecision> for AfterToolContributions {
    fn from(verdict: AfterToolDecision) -> Self {
        Self {
            verdict,
            ..Self::default()
        }
    }
}

pub type ToolArgsTransformHook =
    Arc<dyn Fn(ToolArgsTransformInput) -> PluginFuture<serde_json::Value> + Send + Sync>;
pub type ToolArgsCheckHook =
    Arc<dyn Fn(ToolArgsCheckInput) -> PluginFuture<BeforeToolDecision> + Send + Sync>;
pub type ToolResultTransformHook =
    Arc<dyn Fn(ToolResultTransformInput) -> PluginFuture<ToolResultCandidate> + Send + Sync>;
pub type ToolResultCheckHook =
    Arc<dyn Fn(ToolResultCheckInput) -> PluginFuture<AfterToolContributions> + Send + Sync>;

/// One after-check's declared events, attributed.
pub(crate) struct AttributedContributions {
    pub(crate) plugin_id: String,
    pub(crate) events: Vec<PluginRuntimeEvent>,
}

/// Every after-check reply of one occurrence: the reduced verdicts, the
/// contributions they declared, and the state commands they proposed, in
/// recorded callback order.
pub(crate) struct ResultChecks {
    pub(crate) record: CheckRecord<AfterToolDecision>,
    pub(crate) contributions: Vec<AttributedContributions>,
    pub(crate) proposals: Vec<super::Proposal>,
}

/// The failure a check's own error becomes: restrictive, never an Allow.
pub(crate) fn failed_check(
    phase: ToolHookPhase,
    callback: &PluginCallbackIdentity,
    error: &PluginError,
) -> crate::ToolFailure {
    let mut failure = crate::ToolFailure::runtime(
        crate::ToolFailureClass::Internal,
        match phase {
            ToolHookPhase::ArgsCheck => "tool_args_check_failed",
            _ => "tool_result_check_failed",
        },
        format!(
            "tool check `{}` of plugin `{}` failed: {error}",
            callback.key, callback.owner.plugin
        ),
    );
    failure.source = crate::ToolFailureSource::Plugin;
    failure
}

/// The failure a transform's error becomes: the chain has no valid value to
/// continue from, so the call fails.
pub(crate) fn failed_transform(
    phase: ToolHookPhase,
    callback: &PluginCallbackIdentity,
    error: &PluginError,
) -> crate::ToolFailure {
    let mut failure = crate::ToolFailure::runtime(
        crate::ToolFailureClass::Internal,
        match phase {
            ToolHookPhase::ArgsTransform => "tool_args_transform_failed",
            _ => "tool_result_transform_failed",
        },
        format!(
            "tool transform `{}` of plugin `{}` failed: {error}",
            callback.key, callback.owner.plugin
        ),
    );
    failure.source = crate::ToolFailureSource::Plugin;
    failure
}

/// The call result of a terminal check verdict: Deny and Cancel fail only
/// the call, AbortRun also carries the Run control, derived from the same
/// reply.
fn terminal_output(plugin_id: &str, verdict: TerminalVerdict<'_>) -> crate::ToolCallOutput {
    match verdict {
        TerminalVerdict::Deny(failure) => crate::ToolCallOutput::failure(failure.clone()),
        TerminalVerdict::Cancel(cancellation) => {
            crate::ToolCallOutput::cancelled(cancellation.clone())
        }
        TerminalVerdict::AbortRun(abort) => {
            let mut failure = crate::ToolFailure::runtime(
                crate::ToolFailureClass::Execution,
                abort.code.clone(),
                abort.message.clone(),
            );
            failure.source = crate::ToolFailureSource::Plugin;
            crate::ToolCallOutput::failure(failure).with_control(crate::ToolControl::AbortRun {
                code: abort.failure_code(plugin_id),
                message: abort.message.clone(),
            })
        }
    }
}

enum TerminalVerdict<'a> {
    Deny(&'a crate::ToolFailure),
    Cancel(&'a crate::ToolCancellation),
    AbortRun(&'a PluginAbort),
}

/// What admission does with a call, from its reduced before-checks.
pub(crate) enum BeforeSelection {
    /// Run the body.
    Execute,
    /// Serve this cached success through the result phase; no body runs.
    Cached(ToolResultCandidate),
    /// The call's terminal result; no body and no result phase.
    Terminal(crate::ToolCallOutput),
}

pub(crate) fn before_selection(record: &CheckRecord<BeforeToolDecision>) -> BeforeSelection {
    let Some(AttributedVerdict { callback, verdict }) = record.winner() else {
        return BeforeSelection::Execute;
    };
    let plugin_id = callback.owner.plugin.as_str();
    match verdict {
        BeforeToolDecision::Allow => BeforeSelection::Execute,
        BeforeToolDecision::Cached(cached) => {
            BeforeSelection::Cached(cached.clone().into_candidate())
        }
        BeforeToolDecision::Deny(failure) => {
            BeforeSelection::Terminal(terminal_output(plugin_id, TerminalVerdict::Deny(failure)))
        }
        BeforeToolDecision::Cancel(cancellation) => BeforeSelection::Terminal(terminal_output(
            plugin_id,
            TerminalVerdict::Cancel(cancellation),
        )),
        BeforeToolDecision::AbortRun(abort) => {
            BeforeSelection::Terminal(terminal_output(plugin_id, TerminalVerdict::AbortRun(abort)))
        }
    }
}

/// The final output of a call from its final candidate, the body's own Run
/// control, and its reduced after-checks. Only an Allow keeps the candidate
/// and the body's control.
pub(crate) fn after_resolution(
    candidate: ToolResultCandidate,
    control: Option<crate::ToolControl>,
    record: &CheckRecord<AfterToolDecision>,
) -> crate::ToolCallOutput {
    let Some(AttributedVerdict { callback, verdict }) = record.winner() else {
        return candidate.into_output(control);
    };
    let plugin_id = callback.owner.plugin.as_str();
    match verdict {
        AfterToolDecision::Allow => candidate.into_output(control),
        AfterToolDecision::Deny(failure) => {
            terminal_output(plugin_id, TerminalVerdict::Deny(failure))
        }
        AfterToolDecision::Cancel(cancellation) => {
            terminal_output(plugin_id, TerminalVerdict::Cancel(cancellation))
        }
        AfterToolDecision::AbortRun(abort) => {
            terminal_output(plugin_id, TerminalVerdict::AbortRun(abort))
        }
    }
}

/// The attributed terminal replies a reduction did not select, as
/// structured composition evidence, in reduction order.
pub(crate) fn displaced_terminals<V: RankedVerdict>(
    record: &CheckRecord<V>,
) -> Vec<&AttributedVerdict<V>> {
    record
        .replies()
        .iter()
        .skip(1)
        .filter(|reply| reply.verdict.rank() > CheckRank::Allow)
        .collect()
}
