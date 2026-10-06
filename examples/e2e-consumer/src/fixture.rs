//! Host-owned controls stay outside the engine journal. Only the engine
//! invokes the tool and task bodies; HTTP observers cannot drive a turn.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use lash::direct::LlmOutputPart;
use lash::plugins::{
    FormatVersion, PluginDeclaration, PluginError, PluginFactory, PluginOperation,
    PluginOperationOutcome, PluginRegistrar, PluginSessionContext, PluginTask, SessionParam,
    SessionPlugin,
};
use lash::provider::{LlmContentBlock, LlmRole};
use lash::provider::{LlmRequest, LlmResponse, ProviderHandle};
use lash::sync::MutexExt as _;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolAttemptOutcome, ToolCall, ToolDefinition,
    ToolOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

#[derive(Clone, Debug, Serialize)]
pub struct BodyReceipt {
    pub key: String,
    pub call_id: Option<String>,
    pub attempt: Option<u32>,
}

#[derive(Default)]
pub struct Controls {
    gates: Mutex<BTreeMap<String, Arc<Notify>>>,
    entered: Mutex<Vec<BodyReceipt>>,
    fulfilled_tasks: Mutex<BTreeSet<(Option<lash::SessionId>, String)>>,
}

impl Controls {
    pub fn receipts(&self) -> Vec<BodyReceipt> {
        self.entered.lock_recover().clone()
    }

    pub fn release(&self, key: &str) -> bool {
        let gates = self.gates.lock_recover();
        if let Some(gate) = gates.get(key) {
            gate.notify_one();
            true
        } else {
            false
        }
    }

    async fn enter(&self, receipt: BodyReceipt, cancel: &lash::CancellationToken) {
        let gate = {
            let mut gates = self.gates.lock_recover();
            gates
                .entry(receipt.key.clone())
                .or_insert_with(|| Arc::new(Notify::new()))
                .clone()
        };
        // A visible receipt implies the gate exists. Notify keeps a release
        // delivered before the body begins awaiting it.
        self.entered.lock_recover().push(receipt);
        tokio::select! {
            _ = gate.notified() => {},
            _ = cancel.cancelled() => {},
        }
    }
}

pub fn provider() -> ProviderHandle {
    lash::testing::TestProvider::builder()
        .kind("external-consumer-contract")
        .complete(|request| async move { Ok(response(&request)) })
        .build()
        .into_handle()
}

fn response(request: &LlmRequest) -> LlmResponse {
    // Restrict the result search to this turn's last user message. Previous
    // turns' tool replies cannot suppress a fresh tool call.
    let start = request
        .messages
        .iter()
        .rposition(|message| {
            message.role == LlmRole::User
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, LlmContentBlock::Text { .. }))
        })
        .unwrap_or(0);
    let text = request.messages[start..]
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .next()
        .unwrap_or("echo");
    let completed = request.messages[start..].iter().any(|message| {
        message
            .blocks
            .iter()
            .any(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
    });
    let part = if completed {
        LlmOutputPart::Text {
            text: format!("echo:{text}"),
            response_meta: None,
        }
    } else {
        LlmOutputPart::ToolCall {
            call_id: "provider-echo".into(),
            tool_name: "echo".into(),
            input_json: json!({"text": text, "hold": text.starts_with("hold:")}).to_string(),
            replay: None,
        }
    };
    LlmResponse {
        parts: vec![part],
        terminal_reason: if completed {
            lash::direct::LlmTerminalReason::Stop
        } else {
            lash::direct::LlmTerminalReason::ToolUse
        },
        ..LlmResponse::default()
    }
}

pub struct EchoTask;

impl PluginOperation for EchoTask {
    const NAME: &'static str = "consumer.echo";
    const DESCRIPTION: &'static str = "An explicitly completed host task";
    const SESSION_PARAM: SessionParam = SessionParam::Required;
    type Args = String;
    type Output = String;
    type Error = String;
    const ERROR_TYPE: &'static str = "consumer.echo";
    /// The fixture operation returns a Serde string as its typed error.
    /// version_surface = "coexist"
    /// version_guard(items(Error))
    const ERROR_VERSION: FormatVersion = FormatVersion::ONE;
    fn error_class(_: &Self::Error) -> lash::plugins::PluginFailureClass {
        lash::plugins::PluginFailureClass::Terminal
    }
}
impl PluginTask for EchoTask {}

pub struct ConsumerPlugin(pub Arc<Controls>);

impl PluginFactory for ConsumerPlugin {
    fn id(&self) -> &'static str {
        "external-consumer"
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(Self(self.0.clone())))
    }
}

impl lash::plugins::PluginDefinition for ConsumerPlugin {
    fn declaration() -> PluginDeclaration {
        PluginDeclaration::initial("external-consumer")
    }
}

impl SessionPlugin for ConsumerPlugin {
    fn id(&self) -> &'static str {
        "external-consumer"
    }
    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        let definition = ToolDefinition::raw(
            "consumer:echo", "echo", "Echo the admitted text",
            json!({"type":"object", "properties":{"text":{"type":"string"}, "hold":{"type":"boolean"}}, "required":["text","hold"], "additionalProperties":false}),
            json!({"type":"string"}),
        ).map_err(|error| PluginError::Session(error.to_string()))?;
        reg.tools().provider(Arc::new(StaticToolProvider::new(
            vec![definition],
            Echo(self.0.clone()),
        )))?;
        let controls = self.0.clone();
        reg.operations()
            .typed_task::<EchoTask, _, _>(move |ctx, text| {
                let controls = controls.clone();
                async move {
                    // A task body runs again when its journal resumes. The
                    // external hold belongs to the operation, so replay must
                    // observe its release rather than consume a new permit.
                    let operation = (
                        ctx.session_id.clone(),
                        ctx.scoped_effect_controller.scope_id().to_owned(),
                    );
                    if text.starts_with("hold:")
                        && !controls.fulfilled_tasks.lock_recover().contains(&operation)
                    {
                        controls
                            .enter(
                                BodyReceipt {
                                    key: text.clone(),
                                    call_id: None,
                                    attempt: None,
                                },
                                &ctx.cancellation_token,
                            )
                            .await;
                        controls.fulfilled_tasks.lock_recover().insert(operation);
                    }
                    Ok(PluginOperationOutcome::new(text))
                }
            })?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct EchoArgs {
    text: String,
    hold: bool,
}

struct Echo(Arc<Controls>);

#[lash::async_trait]
impl StaticToolExecute for Echo {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let args = match serde_json::from_value::<EchoArgs>(call.args.clone()) {
            Ok(args) => args,
            Err(error) => return ToolOutcome::err_fmt(error).into(),
        };
        if args.hold {
            let stop = call
                .context
                .cancellation_token()
                .cloned()
                .unwrap_or_default();
            self.0
                .enter(
                    BodyReceipt {
                        key: args.text.clone(),
                        call_id: Some(call.context.call_id().to_string()),
                        attempt: Some(call.context.attempt_number()),
                    },
                    &stop,
                )
                .await;
        } else {
            self.0.entered.lock_recover().push(BodyReceipt {
                key: args.text.clone(),
                call_id: Some(call.context.call_id().to_string()),
                attempt: Some(call.context.attempt_number()),
            });
        }
        ToolOutcome::ok(Value::String(args.text)).into()
    }
}
