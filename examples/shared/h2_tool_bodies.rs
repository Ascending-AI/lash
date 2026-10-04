//! H2's tool bodies, shared by the real-host and fleet scenarios.
//!
//! The adapter supplies the shared out-of-journal barrier callback. This
//! fixture records body delivery separately from the engine's durable X ACK.
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, ensure};
use lash::AwaitEventKey;
use lash::tools::{
    CancelHint, PendingCompletion, StaticToolExecute, StaticToolProvider, ToolAttemptOutcome,
    ToolCall, ToolDeclaration, ToolDefinition, ToolIntent, ToolIntents, ToolOutcome,
    ToolOutcomeDone, ToolProvider,
};
use serde::{Deserialize, Serialize};

/// A body delivery is outside evidence, never a replacement for journal facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDelivery {
    pub label: String,
    pub call_id: lash::ToolCallId,
    pub ordinal: u32,
    pub logical_run: Option<lash::TurnId>,
    pub completion: Option<AwaitEventKey>,
}

pub type BodyStep = Pin<Box<dyn Future<Output = Result<()>> + Send>>;
/// Invoked after the durable outside ledger append, before returning a result.
pub type BodyBarrier = Arc<dyn Fn(ToolDelivery) -> BodyStep + Send + Sync>;

#[derive(Clone, Debug)]
pub enum BodyResult {
    Inline {
        value: serde_json::Value,
        intents: ToolIntents,
    },
    Deferred,
    EmitToReceiver {
        value: serde_json::Value,
        receiver: Arc<OnceLock<lash::ProcessId>>,
        event_type: String,
    },
    EmitEvent {
        value: serde_json::Value,
        process_id: lash::ProcessId,
        event_type: String,
    },
}

/// The file remains owned by the case when a host is killed and reopened.
/// Each line is synced before the body-entered barrier becomes observable.
pub struct ToolBodies {
    plan: BTreeMap<String, BodyResult>,
    deliveries: Mutex<std::fs::File>,
    barrier: BodyBarrier,
}

impl ToolBodies {
    pub fn open(
        path: &Path,
        plan: BTreeMap<String, BodyResult>,
        barrier: BodyBarrier,
    ) -> Result<Self> {
        ensure!(!plan.is_empty(), "a tool scenario needs at least one body");
        ensure!(
            plan.keys().all(|label| !label.is_empty()
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')),
            "fixture labels must be plain tool names"
        );
        Ok(Self {
            plan,
            deliveries: Mutex::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open body ledger {}", path.display()))?,
            ),
            barrier,
        })
    }

    pub fn provider(self) -> Result<Arc<dyn ToolProvider>> {
        let definitions = self
            .plan
            .iter()
            .map(|(label, result)| {
                let definition = ToolDefinition::raw(
                    format!("tool:e2e.h2.{label}"),
                    label,
                    "A controlled body for the named real-host scenario.",
                    serde_json::json!({"type":"object", "properties":{}, "additionalProperties":false}),
                    serde_json::json!({}),
                )?;
                let declaration = match result {
                    BodyResult::Inline { intents, .. } => ToolDeclaration::default()
                        .with_intents(intents.intents.iter().map(ToolIntent::kind)),
                    BodyResult::Deferred => ToolDeclaration::deferring(),
                    BodyResult::EmitEvent { .. } | BodyResult::EmitToReceiver { .. } => ToolDeclaration::default()
                        .with_intents([lash::tools::ToolIntentKind::EmitProcessEvent]),
                };
                Ok(definition.with_declaration(declaration))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(StaticToolProvider::new(definitions, self)))
    }

    async fn attempt(&self, call: ToolCall<'_>) -> Result<ToolAttemptOutcome> {
        ensure!(
            call.args == &serde_json::json!({}),
            "unexpected tool arguments"
        );
        let result = self
            .plan
            .get(call.name())
            .ok_or_else(|| anyhow!("unplanned tool body {}", call.name()))?;
        let completion = match result {
            BodyResult::Deferred => Some(call.context.completion_key()?),
            BodyResult::Inline { .. }
            | BodyResult::EmitEvent { .. }
            | BodyResult::EmitToReceiver { .. } => None,
        };
        let delivery = ToolDelivery {
            label: call.name().to_owned(),
            call_id: call.context.call_id().clone(),
            ordinal: call.context.attempt_number(),
            logical_run: call.context.logical_run(),
            completion,
        };
        let mut line = serde_json::to_vec(&delivery)?;
        line.push(b'\n');
        {
            let mut file = self
                .deliveries
                .lock()
                .map_err(|_| anyhow!("body ledger writer panicked"))?;
            file.write_all(&line)?;
            file.sync_all()?;
        }
        (self.barrier)(delivery).await?;
        let emitted = |value: &serde_json::Value,
                       process_id: &lash::ProcessId,
                       event_type: &String| {
            ToolAttemptOutcome::done(
                ToolOutcomeDone::ok(value.clone()),
                ToolIntents::v3(vec![ToolIntent::EmitProcessEvent(
                    lash::tools::EmitProcessEventIntent {
                        owner: call.context.owner().runtime_owner(),
                        process_id: process_id.clone(),
                        event_type: event_type.clone(),
                        payload: serde_json::json!({"call_id":call.context.call_id(), "value":value}),
                    },
                )]),
            )
        };
        Ok(match result {
            BodyResult::Inline { value, intents } => {
                ToolAttemptOutcome::done(ToolOutcomeDone::ok(value.clone()), intents.clone())
            }
            BodyResult::Deferred => {
                let mut completion = PendingCompletion::new();
                completion.on_cancel = CancelHint::Ignore;
                ToolAttemptOutcome::pending(completion)
            }
            BodyResult::EmitEvent {
                value,
                process_id,
                event_type,
            } => emitted(value, process_id, event_type),
            BodyResult::EmitToReceiver {
                value,
                receiver,
                event_type,
            } => emitted(
                value,
                receiver.get().ok_or_else(|| {
                    anyhow!("intent receiver was not registered before submission")
                })?,
                event_type,
            ),
        })
    }
}

#[async_trait::async_trait]
impl StaticToolExecute for ToolBodies {
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        match self.attempt(call).await {
            Ok(outcome) => outcome,
            Err(error) => {
                ToolOutcome::err_fmt(format_args!("H2 body fixture failed: {error:#}")).into()
            }
        }
    }
}

pub fn deliveries(path: &Path) -> Result<Vec<ToolDelivery>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read body ledger {}", path.display()))?;
    text.lines()
        .map(|line| serde_json::from_str(line).context("decode body delivery"))
        .collect()
}
