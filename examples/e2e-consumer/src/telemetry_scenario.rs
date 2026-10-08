//! S34's opt-in retry/transfer workload. Controls release actual bodies;
//! they neither drive a turn nor write a journal command.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path;
use axum::routing::{get, post};
use axum::{Json, Router};
use lash::direct::LlmOutputPart;
use lash::plugins::{
    PluginDeclaration, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    SessionPlugin,
};
use lash::provider::{LlmContentBlock, LlmRequest, LlmResponse, ProviderHandle};
use lash::sync::MutexExt as _;
use lash::tools::{
    ExecutionPolicy, StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall,
    ToolDefinition, ToolFailure, ToolFailureClass, ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BodyReceipt {
    pub phase: u32,
    pub call_id: Option<String>,
    pub ordinal: Option<u32>,
    pub run: Option<String>,
    pub scope: Option<String>,
}

struct Controls {
    gates: [Semaphore; 3],
    entered: Mutex<Vec<BodyReceipt>>,
    changed: Notify,
}

impl Controls {
    fn new() -> Self {
        Self {
            gates: std::array::from_fn(|_| Semaphore::new(0)),
            entered: Mutex::new(Vec::new()),
            changed: Notify::new(),
        }
    }

    async fn enter(&self, receipt: BodyReceipt) -> anyhow::Result<()> {
        let phase = receipt.phase;
        self.entered.lock_recover().push(receipt);
        self.changed.notify_waiters();
        self.gates[(phase - 1) as usize].acquire().await?.forget();
        Ok(())
    }

    async fn wait(&self, phase: u32) -> anyhow::Result<BodyReceipt> {
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let changed = self.changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if let Some(receipt) = self
                    .entered
                    .lock_recover()
                    .iter()
                    .find(|receipt| receipt.phase == phase)
                    .cloned()
                {
                    return receipt;
                }
                changed.await;
            }
        })
        .await
        .map_err(Into::into)
    }
}

#[derive(Clone)]
pub struct Scenario(Arc<Controls>);

impl Default for Scenario {
    fn default() -> Self {
        Self(Arc::new(Controls::new()))
    }
}

impl Scenario {
    pub fn provider(&self) -> ProviderHandle {
        let controls = self.0.clone();
        lash::testing::TestProvider::builder()
            .kind("s34-telemetry")
            .complete(move |request| {
                let controls = controls.clone();
                async move {
                    if text(&request).contains("S34 inner body") {
                        return Ok(answer("S34 inner answer"));
                    }
                    let has_result = request.messages.iter().any(|message| {
                        message
                            .blocks
                            .iter()
                            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                    });
                    if has_result {
                        controls
                            .enter(BodyReceipt {
                                phase: 3,
                                call_id: None,
                                ordinal: None,
                                run: None,
                                scope: None,
                            })
                            .await
                            .map_err(|error| {
                                lash::provider::LlmTransportError::new(error.to_string())
                            })?;
                        Ok(answer("S34 one logical answer"))
                    } else {
                        Ok(LlmResponse {
                            terminal_reason: lash::direct::LlmTerminalReason::ToolUse,
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "s34-provider-correlation".into(),
                                tool_name: "s34_retry".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }],
                            ..Default::default()
                        })
                    }
                }
            })
            .build()
            .into_handle()
    }

    pub fn plugin(&self) -> Arc<dyn PluginFactory> {
        Arc::new(TelemetryPlugin(self.0.clone()))
    }

    pub fn router(&self, stores: Arc<dyn lash::StoreSet>) -> Router {
        let controls = self.0.clone();
        let releases = self.0.clone();
        let receipts = self.0.clone();
        Router::new()
            .route(
                "/control/s34/head/{session}",
                get(move |Path(session): Path<String>| {
                    let stores = stores.clone();
                    async move {
                        let head = stores
                            .session_store_factory()
                            .load_session_head_meta(
                                &lash::SessionId::parse(session).map_err(super::api_error)?,
                            )
                            .await
                            .map_err(|error| {
                                (
                                    axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                    error.to_string(),
                                )
                            })?
                            .ok_or_else(|| {
                                (
                                    axum::http::StatusCode::NOT_FOUND,
                                    "session head absent".into(),
                                )
                            })?;
                        Ok::<_, (axum::http::StatusCode, String)>(Json(json!({
                            "session_id":head.session_id,
                            "schema_version":head.schema_version,
                            "head_revision":head.head_revision,
                            "checkpoint_ref":head.checkpoint_ref,
                            "leaf_node_id":head.leaf_node_id
                        })))
                    }
                }),
            )
            .route(
                "/control/s34/wait/{phase}",
                get(move |Path(phase): Path<u32>| {
                    let controls = controls.clone();
                    async move {
                        controls.wait(phase).await.map(Json).map_err(|error| {
                            (axum::http::StatusCode::REQUEST_TIMEOUT, error.to_string())
                        })
                    }
                }),
            )
            .route(
                "/control/s34/release/{phase}",
                post(move |Path(phase): Path<usize>| {
                    let controls = releases.clone();
                    async move {
                        if let Some(gate) = phase
                            .checked_sub(1)
                            .and_then(|phase| controls.gates.get(phase))
                        {
                            gate.add_permits(1);
                            Ok(Json(json!({"released":phase})))
                        } else {
                            Err(axum::http::StatusCode::BAD_REQUEST)
                        }
                    }
                }),
            )
            .route(
                "/control/s34/receipts",
                get(move || {
                    let controls = receipts.clone();
                    async move { Json(controls.entered.lock_recover().clone()) }
                }),
            )
    }
}

fn text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn answer(text: &str) -> LlmResponse {
    LlmResponse {
        terminal_reason: lash::direct::LlmTerminalReason::Stop,
        parts: vec![LlmOutputPart::Text {
            text: text.into(),
            response_meta: None,
        }],
        usage: lash::direct::LlmUsage {
            input_tokens: 7,
            output_tokens: 3,
            ..Default::default()
        },
        ..Default::default()
    }
}

struct TelemetryPlugin(Arc<Controls>);

impl PluginFactory for TelemetryPlugin {
    fn id(&self) -> &'static str {
        "s34-telemetry"
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self(self.0.clone())))
    }
}

impl lash::plugins::PluginDefinition for TelemetryPlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial("s34-telemetry")
    }
}

impl SessionPlugin for TelemetryPlugin {
    fn id(&self) -> &'static str {
        "s34-telemetry"
    }
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        let definition = ToolDefinition::raw(
            "s34:retry",
            "s34_retry",
            "One reported retry under one admitted call",
            json!({"type":"object","additionalProperties":false}),
            json!({"type":"string"}),
        )
        .map_err(|error| PluginError::Session(error.to_string()))?
        .with_execution(std::time::Duration::from_secs(120))
        .with_execution_policy(ExecutionPolicy::repeatable(
            std::num::NonZeroU32::MIN.saturating_add(1),
            0,
            0,
        ));
        registrar
            .tools()
            .provider(Arc::new(StaticToolProvider::new(
                vec![definition],
                TelemetryTool(self.0.clone()),
            )))?;
        Ok(())
    }
}

struct TelemetryTool(Arc<Controls>);

#[lash::async_trait]
impl StaticToolExecute for TelemetryTool {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let ordinal = call.context.attempt_number();
        if !(1..=2).contains(&ordinal) {
            return ToolOutcome::err_fmt("unexpected S34 tool body ordinal").into();
        }
        if let Err(error) = self
            .0
            .enter(BodyReceipt {
                phase: ordinal,
                call_id: Some(call.context.call_id().to_string()),
                ordinal: Some(ordinal),
                run: call.context.logical_run().map(|run| run.to_string()),
                scope: Some(call.context.execution_scope_id().to_owned()),
            })
            .await
        {
            return ToolOutcome::err_fmt(error).into();
        }
        let completion = call
            .context
            .direct_completions()
            .complete(
                lash::direct::DirectRequest::text("S34 inner body"),
                "consumer",
            )
            .await;
        match completion {
            Err(error) => ToolOutcome::err_fmt(error).into(),
            Ok(completion) if ordinal == 1 => {
                // The completed model result remains recorded usage data even
                // when this spending tool reports a retryable failure.
                let failure = ToolFailure::with_suggested_delay(
                    ToolFailureClass::Io,
                    "s34_retry",
                    format!("retry after {}", completion.text),
                    Some(0),
                );
                ToolOutcome::failure(failure).into()
            }
            Ok(completion) => ToolOutcome::ok(Value::String(completion.text)).into(),
        }
    }
}
