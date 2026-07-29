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
pub type PluginCommandInvokeFuture = Pin<
    Box<dyn Future<Output = Result<ErasedPluginCommandOutcome, PluginOperationFailure>> + Send>,
>;
pub type PluginCommandHandler =
    Arc<dyn Fn(PluginCommandContext, serde_json::Value) -> PluginCommandInvokeFuture + Send + Sync>;
pub type PluginTaskInvokeFuture =
    Pin<Box<dyn Future<Output = Result<ErasedPluginTaskOutcome, PluginOperationFailure>> + Send>>;
pub type PluginTaskHandler =
    Arc<dyn Fn(PluginTaskContext, serde_json::Value) -> PluginTaskInvokeFuture + Send + Sync>;
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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginOperationDef {
    pub name: String,
    pub description: String,
    pub kind: PluginOperationKind,
    pub session_param: SessionParam,
    #[serde(default)]
    pub input_schema: serde_json::Value,
    #[serde(default)]
    pub output_schema: serde_json::Value,
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

pub fn plugin_operation_def<Op: PluginOperation>(kind: PluginOperationKind) -> PluginOperationDef {
    PluginOperationDef {
        name: Op::NAME.to_string(),
        description: Op::DESCRIPTION.to_string(),
        kind,
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

impl PluginRuntimeDirective {
    pub fn queue_turn(input: crate::TurnInput) -> Self {
        Self::QueueTurn {
            input,
            source_key: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PluginCommandOutcome<T> {
    pub output: T,
    pub events: Vec<PluginRuntimeEvent>,
    pub directives: Vec<PluginRuntimeDirective>,
}

impl<T> PluginCommandOutcome<T> {
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
pub struct PluginTaskOutcome<T> {
    pub output: T,
    pub events: Vec<PluginRuntimeEvent>,
    pub directives: Vec<PluginRuntimeDirective>,
}

impl<T> PluginTaskOutcome<T> {
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
pub struct PluginCommandReceipt<T> {
    pub output: T,
    pub events: Vec<PluginOwned<PluginRuntimeEvent>>,
    pub pending_turn_inputs: Vec<crate::PendingTurnInput>,
}

#[derive(Clone, Debug)]
pub struct PluginTaskReceipt<T> {
    pub output: T,
    pub events: Vec<PluginOwned<PluginRuntimeEvent>>,
    pub pending_turn_inputs: Vec<crate::PendingTurnInput>,
}

#[derive(Clone, Debug)]
pub(crate) struct ErasedPluginCommandOutcome {
    pub(crate) output: serde_json::Value,
    pub(crate) events: Vec<PluginRuntimeEvent>,
    pub(crate) directives: Vec<PluginRuntimeDirective>,
}

#[derive(Clone, Debug)]
pub(crate) struct ErasedPluginTaskOutcome {
    pub(crate) output: serde_json::Value,
    pub(crate) events: Vec<PluginRuntimeEvent>,
    pub(crate) directives: Vec<PluginRuntimeDirective>,
}

#[derive(Clone)]
pub(crate) struct RegisteredPluginQuery {
    pub(crate) plugin_id: String,
    pub(crate) def: PluginOperationDef,
    pub(crate) handler: PluginQueryHandler,
}

#[derive(Clone)]
pub(crate) struct RegisteredPluginCommand {
    pub(crate) plugin_id: String,
    pub(crate) def: PluginOperationDef,
    pub(crate) handler: PluginCommandHandler,
}

#[derive(Clone)]
pub(crate) struct RegisteredPluginTask {
    pub(crate) plugin_id: String,
    pub(crate) def: PluginOperationDef,
    pub(crate) handler: PluginTaskHandler,
}
