use lash_sansio::ToolCallOutput;
use serde::Serialize;
use tokio::sync::mpsc;

use super::*;

#[derive(Clone)]
pub struct ToolCatalogContext {
    pub session_id: String,
    pub tools: Vec<ToolManifest>,
    pub resolve_contract: Option<lash_sansio::ToolContractResolver>,
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub extensions: PluginExtensions,
}

#[derive(Clone, Debug)]
pub struct PluginAbort {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, Default)]
pub struct TurnPreparation {
    pub messages: crate::MessageSequence,
    pub events: Vec<crate::SessionStreamEvent>,
    pub abort: Option<PluginAbort>,
}

#[derive(Clone)]
pub struct PrepareTurnRequest {
    pub session_id: String,
    pub state: SessionReadView,
    pub messages: crate::MessageSequence,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub turn_context: crate::TurnContext,
}

#[derive(Clone, Debug, Default)]
pub struct CheckpointApplication {
    pub messages: Vec<PluginMessage>,
    pub events: Vec<crate::SessionStreamEvent>,
    pub abort: Option<PluginAbort>,
}

#[derive(Clone, Debug)]
pub struct TurnFinalization {
    pub turn: AssembledTurn,
    pub events: Vec<crate::SessionStreamEvent>,
}

pub(crate) async fn emit_plugin_runtime_events(
    event_tx: &mpsc::Sender<crate::SessionStreamEvent>,
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) {
    for event in plugin_runtime_session_events(plugin_id, events) {
        crate::session_model::send_event(event_tx, event).await;
    }
}

pub(crate) fn plugin_runtime_session_events(
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) -> Vec<crate::SessionStreamEvent> {
    events
        .into_iter()
        .map(|event| crate::SessionStreamEvent::PluginEvent {
            plugin_id: plugin_id.to_string(),
            event,
        })
        .collect()
}

/// The precedence strength of a terminal plugin directive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PluginTerminalStrength {
    SuccessfulShortCircuit,
    DeniedShortCircuit,
    AbortTurn,
}

impl PluginTerminalStrength {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::SuccessfulShortCircuit => "successful_short_circuit",
            Self::DeniedShortCircuit => "denied_short_circuit",
            Self::AbortTurn => "abort_turn",
        }
    }
}

/// An ambient action legal at every plugin-hook boundary.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginDirective {
    CreateSession {
        request: Box<SessionCreateRequest>,
    },
    EmitRuntimeEvents {
        events: Vec<PluginRuntimeEvent>,
    },
    EmitTrace {
        name: String,
        #[serde(default)]
        payload: serde_json::Value,
        #[serde(default)]
        context: Box<lash_trace::TraceContext>,
    },
}

impl PluginDirective {
    pub fn emit_runtime_events(events: Vec<PluginRuntimeEvent>) -> Self {
        Self::EmitRuntimeEvents { events }
    }

    pub fn emit_trace(name: impl Into<String>, payload: serde_json::Value) -> Self {
        Self::EmitTrace {
            name: name.into(),
            payload,
            context: Box::new(lash_trace::TraceContext::default()),
        }
    }
}

/// Payload shared by hook boundaries that may abort the current turn.
#[derive(Clone, Debug, Serialize)]
pub struct AbortTurnDirective {
    pub code: String,
    pub message: String,
}

/// Payload shared by hook boundaries that may enqueue messages.
#[derive(Clone, Debug, Serialize)]
pub struct EnqueueMessagesDirective {
    pub messages: Vec<PluginMessage>,
}

/// Payload for replacing the arguments passed to a tool.
#[derive(Clone, Debug, Serialize)]
pub struct ReplaceToolArgsDirective {
    pub args: serde_json::Value,
}

/// Payload shared by tool-hook boundaries that may replace a tool result.
#[derive(Clone, Debug, Serialize)]
pub struct ShortCircuitToolDirective {
    pub output: ToolCallOutput,
}

impl ShortCircuitToolDirective {
    pub fn new(result: ToolOutcome) -> Self {
        Self {
            output: result.into_done_output().unwrap_or_else(|_| {
                ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "pending_tool_short_circuit",
                    "plugin short-circuit directives require completed tool output",
                ))
            }),
        }
    }
}

/// Directives legal from `before_turn` and checkpoint hooks.
#[derive(Clone, Debug, Serialize)]
pub enum TurnPluginDirective {
    Ambient(PluginDirective),
    AbortTurn(AbortTurnDirective),
    EnqueueMessages(EnqueueMessagesDirective),
}

/// Directives legal from `after_turn` hooks.
#[derive(Clone, Debug, Serialize)]
pub enum AfterTurnPluginDirective {
    Ambient(PluginDirective),
    EnqueueMessages(EnqueueMessagesDirective),
}

/// Directives legal from `before_tool_call` hooks.
///
/// Argument replacements take effect immediately; earlier before-tool hooks are reinspected once
/// with the replacement, and another replacement during that bounded pass is rejected with
/// [`PluginError::BeforeToolCallReplacementConflict`]. Reinspection honors denials and aborts
/// only; side effects from the initial pass are not applied again. Terminal directives are joined
/// by restrictiveness: abort beats a denied or cancelled short-circuit, which beats a successful
/// short-circuit. Equal-strength conflicts use plugin ID as a stable tie-breaker, and a single
/// plugin's first-emitted equal-strength terminal wins.
#[derive(Clone, Debug, Serialize)]
// justification: directives are transient public plugin values and the short-circuit output avoids another allocation.
#[allow(clippy::large_enum_variant)]
pub enum BeforeToolCallPluginDirective {
    Ambient(PluginDirective),
    AbortTurn(AbortTurnDirective),
    ReplaceToolArgs(ReplaceToolArgsDirective),
    ShortCircuitTool(ShortCircuitToolDirective),
}

/// Directives legal from `after_tool_call` hooks.
///
/// Successful result replacements reinspect earlier hooks once. That pass honors only denials and
/// aborts, never repeats side effects, and rejects another successful replacement with
/// [`PluginError::AfterToolCallReplacementConflict`]. Terminal directives use the same strength
/// ordering as the before-tool seam, while equal-strength replacements remain first-emitted-wins.
#[derive(Clone, Debug, Serialize)]
// justification: directives are transient public plugin values and the short-circuit output avoids another allocation.
#[allow(clippy::large_enum_variant)]
pub enum AfterToolCallPluginDirective {
    Ambient(PluginDirective),
    AbortTurn(AbortTurnDirective),
    ShortCircuitTool(ShortCircuitToolDirective),
    EnqueueMessages(EnqueueMessagesDirective),
}

macro_rules! impl_ambient_conversion {
    ($($directive:ty),+ $(,)?) => {
        $(
            impl From<PluginDirective> for $directive {
                fn from(value: PluginDirective) -> Self {
                    Self::Ambient(value)
                }
            }
        )+
    };
}

impl_ambient_conversion!(
    TurnPluginDirective,
    AfterTurnPluginDirective,
    BeforeToolCallPluginDirective,
    AfterToolCallPluginDirective,
);

impl From<AbortTurnDirective> for TurnPluginDirective {
    fn from(value: AbortTurnDirective) -> Self {
        Self::AbortTurn(value)
    }
}

impl From<AbortTurnDirective> for BeforeToolCallPluginDirective {
    fn from(value: AbortTurnDirective) -> Self {
        Self::AbortTurn(value)
    }
}

impl From<AbortTurnDirective> for AfterToolCallPluginDirective {
    fn from(value: AbortTurnDirective) -> Self {
        Self::AbortTurn(value)
    }
}

impl From<EnqueueMessagesDirective> for TurnPluginDirective {
    fn from(value: EnqueueMessagesDirective) -> Self {
        Self::EnqueueMessages(value)
    }
}

impl From<EnqueueMessagesDirective> for AfterTurnPluginDirective {
    fn from(value: EnqueueMessagesDirective) -> Self {
        Self::EnqueueMessages(value)
    }
}

impl From<EnqueueMessagesDirective> for AfterToolCallPluginDirective {
    fn from(value: EnqueueMessagesDirective) -> Self {
        Self::EnqueueMessages(value)
    }
}

impl From<ReplaceToolArgsDirective> for BeforeToolCallPluginDirective {
    fn from(value: ReplaceToolArgsDirective) -> Self {
        Self::ReplaceToolArgs(value)
    }
}

impl From<ShortCircuitToolDirective> for BeforeToolCallPluginDirective {
    fn from(value: ShortCircuitToolDirective) -> Self {
        Self::ShortCircuitTool(value)
    }
}

impl From<ShortCircuitToolDirective> for AfterToolCallPluginDirective {
    fn from(value: ShortCircuitToolDirective) -> Self {
        Self::ShortCircuitTool(value)
    }
}

fn short_circuit_terminal_strength(output: &ToolCallOutput) -> PluginTerminalStrength {
    if output.is_success() {
        PluginTerminalStrength::SuccessfulShortCircuit
    } else {
        PluginTerminalStrength::DeniedShortCircuit
    }
}

impl BeforeToolCallPluginDirective {
    pub fn short_circuit(result: ToolOutcome) -> Self {
        Self::ShortCircuitTool(ShortCircuitToolDirective::new(result))
    }

    pub(crate) fn replacement_args(&self) -> Option<&serde_json::Value> {
        match self {
            Self::ReplaceToolArgs(directive) => Some(&directive.args),
            Self::Ambient(_) | Self::AbortTurn(_) | Self::ShortCircuitTool(_) => None,
        }
    }

    pub(crate) fn terminal_strength(&self) -> Option<PluginTerminalStrength> {
        match self {
            Self::AbortTurn(_) => Some(PluginTerminalStrength::AbortTurn),
            Self::ShortCircuitTool(directive) => {
                Some(short_circuit_terminal_strength(&directive.output))
            }
            Self::Ambient(_) | Self::ReplaceToolArgs(_) => None,
        }
    }
}

impl AfterToolCallPluginDirective {
    pub fn short_circuit(result: ToolOutcome) -> Self {
        Self::ShortCircuitTool(ShortCircuitToolDirective::new(result))
    }

    pub(crate) fn successful_replacement(&self) -> Option<ToolOutcome> {
        match self {
            Self::ShortCircuitTool(directive) if directive.output.is_success() => {
                Some(ToolOutcome::from_output(directive.output.clone()))
            }
            Self::ShortCircuitTool(_)
            | Self::Ambient(_)
            | Self::AbortTurn(_)
            | Self::EnqueueMessages(_) => None,
        }
    }

    pub(crate) fn terminal_strength(&self) -> Option<PluginTerminalStrength> {
        match self {
            Self::AbortTurn(_) => Some(PluginTerminalStrength::AbortTurn),
            Self::ShortCircuitTool(directive) => {
                Some(short_circuit_terminal_strength(&directive.output))
            }
            Self::Ambient(_) | Self::EnqueueMessages(_) => None,
        }
    }
}

pub(crate) enum AmbientDirectiveAction {
    EmitRuntimeEvents {
        plugin_id: String,
        events: Vec<PluginRuntimeEvent>,
    },
    None,
}

pub(crate) enum AmbientDirectiveError {
    CreateSession(String),
    EmitTrace(PluginError),
}

impl AmbientDirectiveError {
    pub(crate) fn into_plugin_error(self) -> PluginError {
        match self {
            Self::CreateSession(message) => PluginError::Session(message),
            Self::EmitTrace(error) => error,
        }
    }

    pub(crate) fn message(&self) -> String {
        match self {
            Self::CreateSession(message) => message.clone(),
            Self::EmitTrace(error) => error.to_string(),
        }
    }
}

pub(crate) async fn interpret_ambient_directive(
    emitted: PluginOwned<PluginDirective>,
    session_lifecycle: &Arc<dyn SessionLifecycleService>,
    session_graph: &Arc<dyn SessionGraphService>,
) -> Result<AmbientDirectiveAction, AmbientDirectiveError> {
    match emitted.value {
        PluginDirective::CreateSession { request } => {
            session_lifecycle
                .create_session(*request)
                .await
                .map_err(|error| AmbientDirectiveError::CreateSession(error.to_string()))?;
            Ok(AmbientDirectiveAction::None)
        }
        PluginDirective::EmitRuntimeEvents { events } => {
            Ok(AmbientDirectiveAction::EmitRuntimeEvents {
                plugin_id: emitted.plugin_id,
                events,
            })
        }
        PluginDirective::EmitTrace {
            name,
            payload,
            context,
        } => {
            session_graph
                .emit_trace_event(
                    *context,
                    lash_trace::TraceEvent::Custom {
                        name: format!("plugin.{}.{}", emitted.plugin_id, name),
                        payload,
                    },
                )
                .await
                .map_err(AmbientDirectiveError::EmitTrace)?;
            Ok(AmbientDirectiveAction::None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_strength_ordering_and_variants() {
        assert!(
            PluginTerminalStrength::SuccessfulShortCircuit
                < PluginTerminalStrength::DeniedShortCircuit
        );
        assert!(PluginTerminalStrength::DeniedShortCircuit < PluginTerminalStrength::AbortTurn);

        let before_cases: Vec<(
            BeforeToolCallPluginDirective,
            Option<PluginTerminalStrength>,
        )> = vec![
            (
                BeforeToolCallPluginDirective::AbortTurn(AbortTurnDirective {
                    code: "test".into(),
                    message: "abort".into(),
                }),
                Some(PluginTerminalStrength::AbortTurn),
            ),
            (
                BeforeToolCallPluginDirective::ShortCircuitTool(ShortCircuitToolDirective {
                    output: ToolCallOutput::success(serde_json::json!("ok")),
                }),
                Some(PluginTerminalStrength::SuccessfulShortCircuit),
            ),
            (
                BeforeToolCallPluginDirective::ShortCircuitTool(ShortCircuitToolDirective {
                    output: ToolCallOutput::failure(crate::ToolFailure::runtime(
                        crate::ToolFailureClass::Internal,
                        "err",
                        "denied",
                    )),
                }),
                Some(PluginTerminalStrength::DeniedShortCircuit),
            ),
            (
                BeforeToolCallPluginDirective::Ambient(PluginDirective::CreateSession {
                    request: Box::new(SessionCreateRequest::root(
                        SessionStartPoint::Empty,
                        PluginOptions::default(),
                    )),
                }),
                None,
            ),
            (
                BeforeToolCallPluginDirective::ReplaceToolArgs(ReplaceToolArgsDirective {
                    args: serde_json::json!({}),
                }),
                None,
            ),
            (
                BeforeToolCallPluginDirective::Ambient(PluginDirective::EmitTrace {
                    name: "trace".into(),
                    payload: serde_json::json!({}),
                    context: Box::default(),
                }),
                None,
            ),
        ];

        for (directive, expected_strength) in before_cases {
            assert_eq!(directive.terminal_strength(), expected_strength);
        }

        let after_cases = [
            AfterToolCallPluginDirective::AbortTurn(AbortTurnDirective {
                code: "test".into(),
                message: "abort".into(),
            }),
            AfterToolCallPluginDirective::ShortCircuitTool(ShortCircuitToolDirective {
                output: ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "err",
                    "denied",
                )),
            }),
        ];
        let expected = [
            Some(PluginTerminalStrength::AbortTurn),
            Some(PluginTerminalStrength::DeniedShortCircuit),
        ];
        for (directive, expected_strength) in after_cases.into_iter().zip(expected) {
            assert_eq!(directive.terminal_strength(), expected_strength);
        }
    }
}
