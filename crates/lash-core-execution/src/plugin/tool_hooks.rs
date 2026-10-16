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
    pub config: super::OwnerConfigView,
    pub argument_projection: crate::ToolArgumentProjectionPolicy,
    pub turn_context: crate::TurnContext,
    pub(crate) check_phase: super::ControlPhaseDeadline,
    pub(crate) clock: Arc<dyn crate::Clock>,
    pub(crate) sessions: Arc<dyn SessionStateService>,
    /// Where this hook stands in the trace: under the call's tool span,
    /// bound from the registered hook that runs and revoked when it returns.
    pub trace: crate::trace::ExtensionTrace,
}

impl ToolHookContext {
    /// This context with `trace` as the hook's capability.
    pub(crate) fn with_trace(&self, trace: crate::trace::ExtensionTrace) -> Self {
        Self {
            trace,
            ..self.clone()
        }
    }

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
}

impl ToolResultCandidate {
    pub(crate) fn split(output: crate::ToolCallOutput) -> (Self, Option<crate::ToolControl>) {
        let crate::ToolCallOutput {
            outcome,
            control,
            view,
            projection_value,
        } = output;
        (
            Self {
                outcome,
                view,
                projection_value,
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
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(remote = "Self")]
pub struct CachedToolSuccess {
    #[serde(serialize_with = "lash_sansio::durable_serde::serialize")]
    pub value: crate::ToolValue,
    pub view: Option<crate::ToolView>,
    pub projection_value: Option<serde_json::Value>,
}

impl lash_sansio::DurableSerialize for CachedToolSuccess {
    fn serialize_durable<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        Self::serialize(self, serializer)
    }
}

impl<'de> serde::Deserialize<'de> for CachedToolSuccess {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::deserialize(deserializer)
    }
}

impl CachedToolSuccess {
    pub fn new(value: crate::ToolValue) -> Self {
        Self {
            value,
            view: None,
            projection_value: None,
        }
    }

    fn into_candidate(self) -> ToolResultCandidate {
        ToolResultCandidate {
            outcome: crate::ToolCallOutcome::Success(self.value),
            view: self.view,
            projection_value: self.projection_value,
        }
    }
}

/// A before-check's one verdict over the prepared call.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", content = "value", rename_all = "snake_case")]
pub enum BeforeToolDecision {
    Allow,
    /// Serve this success instead of running the body.
    Cached(#[serde(serialize_with = "lash_sansio::durable_serde::serialize")] CachedToolSuccess),
    /// Fail the call.
    Deny(#[serde(serialize_with = "lash_sansio::durable_serde::serialize")] crate::ToolFailure),
    /// Cancel the call.
    Cancel(
        #[serde(serialize_with = "lash_sansio::durable_serde::serialize")] crate::ToolCancellation,
    ),
    /// Fail the call and stop the owning logical Run.
    AbortRun(PluginAbort),
}

/// An after-check's one verdict over the final result. It has no variant
/// that carries a result: an after-check cannot replace one, and recovery
/// belongs in a result transform.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", content = "value", rename_all = "snake_case")]
pub enum AfterToolDecision {
    #[default]
    Allow,
    Deny(#[serde(serialize_with = "lash_sansio::durable_serde::serialize")] crate::ToolFailure),
    Cancel(
        #[serde(serialize_with = "lash_sansio::durable_serde::serialize")] crate::ToolCancellation,
    ),
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
            callback.key, callback.owner
        ),
    );
    failure.source = crate::ToolFailureSource::Plugin;
    if let PluginError::Runtime(error) = error
        && let Some(crate::RuntimeErrorCause::ControlPhaseExpired { expiry }) = &error.cause
    {
        failure = failure.with_cause(crate::ToolFailureCause::ControlPhaseExpired {
            expiry: expiry.clone(),
        });
    }
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
            callback.key, callback.owner
        ),
    );
    failure.source = crate::ToolFailureSource::Plugin;
    if let PluginError::Runtime(error) = error
        && let Some(crate::RuntimeErrorCause::ControlPhaseExpired { expiry }) = &error.cause
    {
        failure = failure.with_cause(crate::ToolFailureCause::ControlPhaseExpired {
            expiry: expiry.clone(),
        });
    }
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

fn expiry_evidence(failure: &crate::ToolFailure) -> Option<crate::ToolValue> {
    matches!(
        failure.cause.as_deref(),
        Some(crate::ToolFailureCause::ControlPhaseExpired { .. })
    )
    .then(|| failure.raw.clone())
    .flatten()
}

pub(crate) fn before_expiry_evidence(
    record: &CheckRecord<BeforeToolDecision>,
) -> Option<crate::ToolValue> {
    record
        .replies()
        .iter()
        .find_map(|reply| match &reply.verdict {
            BeforeToolDecision::Deny(failure) => expiry_evidence(failure),
            _ => None,
        })
}

pub(crate) fn after_expiry_evidence(
    record: &CheckRecord<AfterToolDecision>,
) -> Option<crate::ToolValue> {
    record
        .replies()
        .iter()
        .find_map(|reply| match &reply.verdict {
            AfterToolDecision::Deny(failure) => expiry_evidence(failure),
            _ => None,
        })
}

/// Keep the selected error and Run control, with every original verdict as
/// the recorded evidence of the timed-out phase, even if expiry lost reduction.
pub(crate) fn with_expiry_evidence(
    mut output: crate::ToolCallOutput,
    evidence: Option<crate::ToolValue>,
) -> crate::ToolCallOutput {
    if let Some(evidence) = evidence {
        match &mut output.outcome {
            crate::ToolCallOutcome::Failure(failure) => failure.raw = Some(evidence),
            crate::ToolCallOutcome::Cancelled(cancellation) => cancellation.raw = Some(evidence),
            crate::ToolCallOutcome::Success(_) => {}
        }
    }
    output
}

pub(crate) fn before_selection(record: &CheckRecord<BeforeToolDecision>) -> BeforeSelection {
    let Some(AttributedVerdict { callback, verdict }) = record.winner() else {
        return BeforeSelection::Execute;
    };
    let plugin_id = callback.owner.as_str();
    let selected = match verdict {
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
    };
    match selected {
        BeforeSelection::Terminal(output) => {
            BeforeSelection::Terminal(with_expiry_evidence(output, before_expiry_evidence(record)))
        }
        selected => selected,
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
    let plugin_id = callback.owner.as_str();
    let output = match verdict {
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
    };
    with_expiry_evidence(output, after_expiry_evidence(record))
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
