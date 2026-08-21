use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use super::*;

pub type PluginQueryInvokeFuture =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, PluginOperationFailure>> + Send>>;
pub type PluginQueryHandler =
    Arc<dyn Fn(PluginQueryContext, serde_json::Value) -> PluginQueryInvokeFuture + Send + Sync>;
pub(crate) type ErasedPluginOperationInvokeFuture = Pin<
    Box<dyn Future<Output = Result<ErasedPluginOperationOutcome, PluginOperationFailure>> + Send>,
>;
pub type PluginCommandHandler = Arc<
    dyn Fn(PluginCommandContext, serde_json::Value) -> ErasedPluginOperationInvokeFuture
        + Send
        + Sync,
>;
pub type PluginTaskHandler = Arc<
    dyn Fn(PluginTaskContext, serde_json::Value) -> ErasedPluginOperationInvokeFuture + Send + Sync,
>;
type PluginOperationHandler = Arc<
    dyn Fn(PluginOperationContext, serde_json::Value) -> ErasedPluginOperationInvokeFuture
        + Send
        + Sync,
>;
pub type PluginOperationFuture<T> =
    Pin<Box<dyn Future<Output = Result<T, PluginOperationFailure>> + Send>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionParam {
    Required,
    Optional,
    Forbidden,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginOperationKind {
    Query,
    Command,
    Task,
}

impl PluginOperationKind {
    /// The word this kind is called by in operator-facing failure text.
    ///
    /// The match is exhaustive on purpose: a fourth kind has to answer this
    /// question before it compiles, which is what keeps the dispatch
    /// mismatch below a typed failure rather than a panic.
    fn label(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Command => "command",
            Self::Task => "task",
        }
    }
}

/// A plugin operation as its author describes it, before registration.
///
/// The kind is deliberately absent: it is decided by which
/// [`PluginOperationRegistration`] constructor the spec is handed to, so a
/// registration whose declared kind disagrees with its handler cannot be
/// written down.
#[derive(Clone, Debug)]
pub(crate) struct PluginOperationSpec {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) session_param: SessionParam,
    pub(crate) input_schema: serde_json::Value,
    pub(crate) output_schema: serde_json::Value,
}

/// A registered plugin operation as hosts see it.
///
/// `kind` is an output, not an input: it is stamped by the registration
/// constructor that also wrapped the handler, so the two can never disagree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginOperationDef {
    pub name: String,
    pub description: String,
    kind: PluginOperationKind,
    pub session_param: SessionParam,
    #[serde(default)]
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub output_schema: serde_json::Value,
}

impl PluginOperationDef {
    /// The kind this operation was registered as, and therefore the only
    /// kind of invocation that reaches its handler.
    pub fn kind(&self) -> PluginOperationKind {
        self.kind
    }

    fn from_spec(spec: PluginOperationSpec, kind: PluginOperationKind) -> Self {
        Self {
            name: spec.name,
            description: spec.description,
            kind,
            session_param: spec.session_param,
            input_schema: spec.input_schema,
            output_schema: spec.output_schema,
        }
    }
}

pub trait PluginOperation: Send + Sync + 'static {
    const NAME: &'static str;
    const DESCRIPTION: &'static str;
    const SESSION_PARAM: SessionParam;
    type Args: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
    type Output: Serialize + DeserializeOwned + JsonSchema + Send + 'static;
}

pub trait PluginQuery: PluginOperation {}

pub trait PluginCommand: PluginOperation {}

pub trait PluginTask: PluginOperation {}

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct PluginOperationFailure {
    message: String,
}

impl PluginOperationFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<String> for PluginOperationFailure {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for PluginOperationFailure {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl From<PluginError> for PluginOperationFailure {
    fn from(value: PluginError) -> Self {
        Self::new(value.to_string())
    }
}

pub(crate) fn plugin_operation_spec<Op: PluginOperation>() -> PluginOperationSpec {
    PluginOperationSpec {
        name: Op::NAME.to_string(),
        description: Op::DESCRIPTION.to_string(),
        session_param: Op::SESSION_PARAM,
        input_schema: serde_json::to_value(schemars::schema_for!(Op::Args))
            .unwrap_or_else(|_| serde_json::json!({})),
        output_schema: serde_json::to_value(schemars::schema_for!(Op::Output))
            .unwrap_or_else(|_| serde_json::json!({})),
    }
}

#[derive(Clone)]
pub struct PluginQueryContext {
    pub session_id: Option<String>,
    pub sessions: Arc<dyn SessionReadService>,
    pub processes: Arc<dyn ProcessReadService>,
}

#[derive(Clone)]
pub struct PluginCommandContext {
    pub session_id: Option<String>,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
}

#[derive(Clone)]
pub struct PluginTaskContext {
    pub session_id: Option<String>,
    pub sessions: Arc<dyn SessionStateService>,
    pub session_lifecycle: Arc<dyn SessionLifecycleService>,
    pub session_graph: Arc<dyn SessionGraphService>,
    pub processes: Arc<dyn crate::ProcessService>,
    pub scoped_effect_controller: crate::ScopedEffectController<'static>,
    pub cancellation_token: tokio_util::sync::CancellationToken,
}

#[async_trait::async_trait]
pub trait SessionReadService: Send + Sync {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session snapshots are unavailable in this runtime".to_string(),
        ))
    }

    async fn snapshot_session(&self, _session_id: &str) -> Result<SessionSnapshot, PluginError> {
        Err(PluginError::Session(
            "session lookup is unavailable in this runtime".to_string(),
        ))
    }

    async fn tool_catalog(&self, _session_id: &str) -> Result<Vec<serde_json::Value>, PluginError> {
        Err(PluginError::Session(
            "tool catalogs are unavailable in this runtime".to_string(),
        ))
    }

    async fn shared_tool_catalog(
        &self,
        session_id: &str,
    ) -> Result<Arc<Vec<serde_json::Value>>, PluginError> {
        Ok(Arc::new(self.tool_catalog(session_id).await?))
    }

    async fn tool_state(&self, _session_id: &str) -> Result<crate::ToolState, PluginError> {
        Err(PluginError::Session(
            "tool state is unavailable in this session".to_string(),
        ))
    }
}

#[async_trait::async_trait]
pub trait ProcessReadService: Send + Sync {
    async fn list_visible(
        &self,
        _session_id: &str,
        _mode: crate::ProcessListMode,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::ProcessRecord>, PluginError> {
        Err(PluginError::Session(
            "process inspection is unavailable in this runtime".to_string(),
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginRuntimeDirective {
    QueueTurn {
        input: crate::TurnInput,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_key: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct PluginOperationOutcome<T> {
    pub output: T,
    pub events: Vec<PluginRuntimeEvent>,
    pub directives: Vec<PluginRuntimeDirective>,
}

impl<T> PluginOperationOutcome<T> {
    pub fn new(output: T) -> Self {
        Self {
            output,
            events: Vec::new(),
            directives: Vec::new(),
        }
    }

    pub fn with_events(mut self, events: Vec<PluginRuntimeEvent>) -> Self {
        self.events = events;
        self
    }

    pub fn with_directives(mut self, directives: Vec<PluginRuntimeDirective>) -> Self {
        self.directives = directives;
        self
    }
}

#[derive(Clone, Debug)]
pub struct PluginOperationReceipt<T> {
    pub output: T,
    pub events: Vec<PluginOwned<PluginRuntimeEvent>>,
    pub pending_turn_inputs: Vec<crate::PendingTurnInput>,
}

#[derive(Clone, Debug)]
pub(crate) struct ErasedPluginOperationOutcome {
    pub(crate) output: serde_json::Value,
    pub(crate) events: Vec<PluginRuntimeEvent>,
    pub(crate) directives: Vec<PluginRuntimeDirective>,
}

impl ErasedPluginOperationOutcome {
    pub(crate) fn new(output: serde_json::Value) -> Self {
        Self {
            output,
            events: Vec::new(),
            directives: Vec::new(),
        }
    }
}

pub(crate) enum PluginOperationContext {
    Query(PluginQueryContext),
    Command(PluginCommandContext),
    Task(PluginTaskContext),
}

impl PluginOperationContext {
    /// The kind of registration this context can drive.
    fn kind(&self) -> PluginOperationKind {
        match self {
            Self::Query(_) => PluginOperationKind::Query,
            Self::Command(_) => PluginOperationKind::Command,
            Self::Task(_) => PluginOperationKind::Task,
        }
    }
}

/// The typed failure a registration returns when it is handed a context of
/// the wrong kind.
///
/// The stamped-kind invariant makes that unreachable today; returning a
/// failure rather than panicking keeps a future kind's dispatch bug a failed
/// operation instead of an aborted turn.
fn mismatched_operation_context(
    expected: PluginOperationKind,
    actual: PluginOperationKind,
) -> ErasedPluginOperationInvokeFuture {
    Box::pin(async move {
        Err(PluginOperationFailure::new(format!(
            "{} registration invoked with a {} context",
            expected.label(),
            actual.label()
        )))
    })
}

#[derive(Clone)]
pub(crate) struct PluginOperationRegistration {
    def: PluginOperationDef,
    handler: PluginOperationHandler,
}

impl PluginOperationRegistration {
    pub(crate) fn query(spec: PluginOperationSpec, handler: PluginQueryHandler) -> Self {
        Self {
            def: PluginOperationDef::from_spec(spec, PluginOperationKind::Query),
            handler: Arc::new(move |ctx, args| match ctx {
                PluginOperationContext::Query(ctx) => {
                    let future = handler(ctx, args);
                    Box::pin(async move { future.await.map(ErasedPluginOperationOutcome::new) })
                        as ErasedPluginOperationInvokeFuture
                }
                other @ (PluginOperationContext::Command(_) | PluginOperationContext::Task(_)) => {
                    mismatched_operation_context(PluginOperationKind::Query, other.kind())
                }
            }),
        }
    }

    pub(crate) fn command(spec: PluginOperationSpec, handler: PluginCommandHandler) -> Self {
        Self {
            def: PluginOperationDef::from_spec(spec, PluginOperationKind::Command),
            handler: Arc::new(move |ctx, args| match ctx {
                PluginOperationContext::Command(ctx) => handler(ctx, args),
                other @ (PluginOperationContext::Query(_) | PluginOperationContext::Task(_)) => {
                    mismatched_operation_context(PluginOperationKind::Command, other.kind())
                }
            }),
        }
    }

    pub(crate) fn task(spec: PluginOperationSpec, handler: PluginTaskHandler) -> Self {
        Self {
            def: PluginOperationDef::from_spec(spec, PluginOperationKind::Task),
            handler: Arc::new(move |ctx, args| match ctx {
                PluginOperationContext::Task(ctx) => handler(ctx, args),
                other @ (PluginOperationContext::Query(_) | PluginOperationContext::Command(_)) => {
                    mismatched_operation_context(PluginOperationKind::Task, other.kind())
                }
            }),
        }
    }

    pub(crate) fn def(&self) -> &PluginOperationDef {
        &self.def
    }

    pub(crate) fn invoke(
        &self,
        ctx: PluginOperationContext,
        args: serde_json::Value,
    ) -> ErasedPluginOperationInvokeFuture {
        (self.handler)(ctx, args)
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredPluginOperation {
    plugin_id: String,
    operation: PluginOperationRegistration,
}

impl RegisteredPluginOperation {
    pub(crate) fn new(plugin_id: String, operation: PluginOperationRegistration) -> Self {
        Self {
            plugin_id,
            operation,
        }
    }

    pub(crate) fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub(crate) fn def(&self) -> &PluginOperationDef {
        self.operation.def()
    }

    pub(crate) fn invoke(
        &self,
        ctx: PluginOperationContext,
        args: serde_json::Value,
    ) -> ErasedPluginOperationInvokeFuture {
        self.operation.invoke(ctx, args)
    }
}
