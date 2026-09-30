//! The synthetic tools a load cell calls.
//!
//! * `synthetic` waits its callback delay and answers the result the workload
//!   regenerates for its key, offered to the witness's idempotent receiver.
//! * `mark` is the witness a durable body leaves once it has run: a child
//!   process, or a cron subscription's target.
//! * `attach` puts blob `index` of its turn into the calling session and
//!   witnesses the exact bytes it put.

use super::{LoadContext, LoadEvent, WitnessedOperation, WitnessedPhase, record_load_event};
use crate::witness;
use anyhow::{Context, Result};
use lash::tools::{ToolBinding, ToolCall, ToolDefinition, ToolDefinitionBindingExt, ToolOutcome};
use lash_perf::workload::OperationId;
use serde_json::{Value, json};
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

pub const SYNTHETIC_TOOL: &str = "synthetic";
pub const MARK_TOOL: &str = "mark";
pub const ATTACH_TOOL: &str = "attach";

/// The load tools of one worker.
#[derive(Clone)]
pub struct LoadTools {
    load: LoadContext,
    witness: PgPool,
    worker_id: String,
}

impl LoadTools {
    pub fn new(load: LoadContext, witness: PgPool, worker_id: String) -> Self {
        Self {
            load,
            witness,
            worker_id,
        }
    }

    pub async fn synthetic(&self, call: ToolCall<'_>) -> ToolOutcome {
        match self.try_synthetic(&call).await {
            Ok(result) => ToolOutcome::ok(result),
            Err(error) => ToolOutcome::err_fmt(format_args!("{error:#}")),
        }
    }

    pub async fn mark(&self, call: ToolCall<'_>) -> ToolOutcome {
        match self.try_mark(&call).await {
            Ok(result) => ToolOutcome::ok(result),
            Err(error) => ToolOutcome::err_fmt(format_args!("{error:#}")),
        }
    }

    pub async fn attach(&self, call: ToolCall<'_>) -> ToolOutcome {
        match self.try_attach(&call).await {
            Ok(result) => {
                ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(result))
            }
            Err(error) => ToolOutcome::err_fmt(format_args!("{error:#}")),
        }
    }

    async fn try_synthetic(&self, call: &ToolCall<'_>) -> Result<Value> {
        let record = call
            .args
            .get("record")
            .context("synthetic arguments carry a record")?;
        let key = record
            .get("key")
            .and_then(Value::as_str)
            .context("the synthetic record names its key")?;
        let result_bytes = super::whole_number(record.get("result_bytes"))
            .context("the synthetic record names its result size")?;
        let callback_ms = super::whole_number(record.get("callback_ms"))
            .context("the synthetic record names its callback delay")?;
        let (operation, _) = OperationId::parse(key)?;
        let result = self
            .load
            .generator(&operation.run)?
            .tool_result(key, u32::try_from(result_bytes).context("result size")?)?;
        // The remote hop a figments tool callback makes.
        tokio::time::sleep(Duration::from_millis(callback_ms)).await;
        let committed = self
            .commit(call, key, &operation.key(), &serde_json::to_vec(&result)?)
            .await?;
        serde_json::from_slice(&committed).context("decode the committed synthetic result")
    }

    async fn try_mark(&self, call: &ToolCall<'_>) -> Result<Value> {
        let key = call
            .args
            .get("key")
            .and_then(Value::as_str)
            .context("a mark names its key")?;
        // A child's key extends its turn's operation key; a cron emission's
        // key names its schedule.
        let parent = OperationId::parse(key)
            .map(|(operation, _)| operation.key())
            .unwrap_or_else(|_| key.to_owned());
        let response = json!({ "key": key });
        let committed = self
            .commit(call, key, &parent, &serde_json::to_vec(&response)?)
            .await?;
        serde_json::from_slice(&committed).context("decode the committed mark")
    }

    /// Offer this physical attempt to the witness's idempotent receiver and
    /// answer with the committed response, as the e2e harness's witnessed
    /// effects do (FIG-608).
    async fn commit(
        &self,
        call: &ToolCall<'_>,
        logical_key: &str,
        parent: &str,
        response: &[u8],
    ) -> Result<Vec<u8>> {
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let request = serde_json::to_vec(call.args).context("encode the effect request")?;
        let attempt = witness::EffectAttempt {
            attempt_id: &attempt_id,
            logical_key,
            parent_workflow_id: parent,
            call_id: call.context.call_id().as_str(),
            worker_id: &self.worker_id,
            request: &request,
        };
        let (accepted, committed) =
            witness::commit_effect(&self.witness, &attempt, response).await?;
        witness::record_effect_reply(
            &self.witness,
            &attempt_id,
            logical_key,
            accepted,
            &committed,
        )
        .await?;
        Ok(committed)
    }

    async fn try_attach(&self, call: &ToolCall<'_>) -> Result<lash_core::ToolValue> {
        let operation_key = call
            .args
            .get("operation")
            .and_then(Value::as_str)
            .context("an attach names its operation")?;
        let index = super::whole_number(call.args.get("index"))
            .context("an attach names its blob index")?;
        let (operation, _) = OperationId::parse(operation_key)?;
        let generator = self.load.generator(&operation.run)?;
        let plan = generator.plan(operation.actor, operation.ordinal)?;
        let blob = generator.attachment(&plan, usize::try_from(index)?)?;
        let media_type = lash::attachments::MediaType::parse(&blob.media_type)?;
        let filename = format!("{}.png", blob.blob_key.replace('/', "-"));
        let reference = call
            .context
            .attachments()
            .put(
                blob.bytes.clone(),
                lash::attachments::AttachmentCreateMeta::new(
                    media_type,
                    Some(lash::attachments::AttachmentTypeMetadata::image(
                        Some(1),
                        Some(1),
                    )),
                    Some(filename),
                ),
            )
            .await
            .context("put the synthetic blob")?;
        let session_id = call.context.session_id().to_string();
        record_load_event(
            &self.witness,
            LoadEvent {
                run: &operation.run,
                subject: &blob.blob_key,
                operation: WitnessedOperation::Attachment,
                phase: WitnessedPhase::Put,
                observer: &self.worker_id,
                detail: &json!({
                    "operation": operation_key,
                    "index": index,
                    "session_id": session_id,
                    "attachment_id": reference.id.to_string(),
                    "call_id": call.context.call_id().as_str(),
                    "worker_id": self.worker_id,
                }),
                content: Some(&blob.bytes),
            },
        )
        .await?;
        let mut result = BTreeMap::new();
        result.insert(
            "id".to_owned(),
            lash_core::ToolValue::String(reference.id.to_string()),
        );
        result.insert(
            "blob_key".to_owned(),
            lash_core::ToolValue::String(blob.blob_key.clone()),
        );
        result.insert(
            "byte_len".to_owned(),
            lash_core::ToolValue::Number(serde_json::Number::from(reference.byte_len)),
        );
        // Returning the stored attachment keeps it referenced by the turn.
        result.insert(
            "attachment".to_owned(),
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
        );
        Ok(lash_core::ToolValue::Object(result))
    }
}

/// Every load tool's name.
pub const TOOL_NAMES: [&str; 3] = [SYNTHETIC_TOOL, MARK_TOOL, ATTACH_TOOL];

type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutcome> + Send + 'a>>;

/// Run load tool `call` on `tools`, or refuse it on a worker that serves no
/// load run.
pub fn execute<'a>(tools: Option<&'a LoadTools>, call: ToolCall<'a>) -> ToolFuture<'a> {
    match (tools, call.name()) {
        (Some(tools), SYNTHETIC_TOOL) => Box::pin(tools.synthetic(call)),
        (Some(tools), MARK_TOOL) => Box::pin(tools.mark(call)),
        (Some(tools), ATTACH_TOOL) => Box::pin(tools.attach(call)),
        (_, name) => {
            let name = name.to_owned();
            Box::pin(async move {
                ToolOutcome::err_fmt(format_args!(
                    "load tool `{name}` needs {} on this worker",
                    super::LOAD_WORKLOAD_ENV
                ))
            })
        }
    }
}

/// The load tools' catalog entries, bound under `tools`.
pub fn definitions() -> Vec<ToolDefinition> {
    [
        (
            "tool:synthetic",
            SYNTHETIC_TOOL,
            lash_perf::workload::tool_schema(),
            lash_perf::workload::tool_result_schema(),
        ),
        (
            "tool:mark",
            MARK_TOOL,
            lash_perf::workload::mark_schema(),
            json!({
                "type": "object",
                "properties": { "key": { "type": "string" } },
                "required": ["key"],
                "additionalProperties": false
            }),
        ),
        (
            "tool:attach",
            ATTACH_TOOL,
            lash_perf::workload::attach_schema(),
            json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "blob_key": { "type": "string" },
                    "byte_len": { "type": "integer" }
                },
                "required": ["id", "blob_key", "byte_len"]
            }),
        ),
    ]
    .into_iter()
    .map(|(id, name, input_schema, output_schema)| {
        ToolDefinition::raw(
            id,
            name,
            "Synthetic FIG-3790 load tool.",
            input_schema,
            output_schema,
        )
        .with_tool_binding(ToolBinding::new(["tools"], name))
    })
    .collect()
}
