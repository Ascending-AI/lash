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
use lash::tools::{
    CancelHint, PendingCompletion, StaticToolExecute, StaticToolProvider, ToolAttemptOutcome,
    ToolCall, ToolDeclaration, ToolDefinition, ToolDefinitionBindingExt, ToolIntent, ToolIntents,
    ToolOutcome, ToolOutcomeDone, ToolProvider,
};
use serde::{Deserialize, Serialize};

/// A body delivery is outside evidence, never a replacement for journal facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDelivery {
    pub label: String,
    pub call_id: lash::ToolCallId,
    pub ordinal: u32,
    pub owner: lash::tools::ExecutionOwner,
    pub logical_run: Option<lash::TurnId>,
    pub completion: Option<String>,
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
    /// Answers the handle of the case's bound process.
    Handle {
        process: Arc<OnceLock<lash::ProcessId>>,
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
                let output = match result {
                    BodyResult::Handle { .. } => {
                        serde_json::json!({"x-lash": {"kind": "process_unknown"}})
                    }
                    _ => serde_json::json!({}),
                };
                let definition = ToolDefinition::raw(
                    format!("tool:e2e.h2.{label}"),
                    label,
                    "A controlled body for the named real-host scenario.",
                    serde_json::json!({"type":"object", "properties":{}, "additionalProperties":false}),
                    output,
                )?
                .with_execution(std::time::Duration::from_secs(120));
                let declaration = match result {
                    BodyResult::Inline { intents, .. } => ToolDeclaration::default()
                        .with_intents(intents.intents.iter().map(ToolIntent::kind)),
                    BodyResult::Deferred => ToolDeclaration::deferring(),
                    BodyResult::Handle { .. } => ToolDeclaration::default(),
                };
                // A deferred body parks until the scenario's host resolves
                // it, or the turn that called it ends.
                let definition = if declaration.may_defer {
                    definition.with_park(lash::tools::ParkBound::UntilScopeEnd)
                } else {
                    definition
                };
                Ok(definition
                    .with_tool_binding(lash::tools::ToolBinding::new(["tools"], label))
                    .with_declaration(declaration))
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
            BodyResult::Deferred => Some(call.context.completion_key()?.as_str().to_owned()),
            BodyResult::Inline { .. } | BodyResult::Handle { .. } => None,
        };
        let delivery = ToolDelivery {
            label: call.name().to_owned(),
            call_id: call.context.call_id().clone(),
            ordinal: call.context.attempt_number(),
            owner: call.context.owner().clone(),
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
        Ok(match result {
            BodyResult::Inline { value, intents } => {
                ToolAttemptOutcome::done(ToolOutcomeDone::ok(value.clone()), intents.clone())
            }
            BodyResult::Deferred => {
                let mut completion = PendingCompletion::new();
                completion.on_cancel = CancelHint::Ignore;
                ToolAttemptOutcome::pending(completion)
            }
            BodyResult::Handle { process } => {
                let process = process
                    .get()
                    .ok_or_else(|| anyhow!("no process was bound before submission"))?;
                let handle = lash::process::HandleId::process(process);
                ToolAttemptOutcome::done(
                    ToolOutcomeDone::ok(
                        serde_json::json!({"__handle__": "lash", "id": handle.as_str()}),
                    ),
                    ToolIntents::default(),
                )
            }
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
