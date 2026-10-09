//! Seeded plans now execute as RLM cells through send() on a served durable
//! node. The receipt names the executed subset; it is not the open-loop fault
//! and maintenance study described by the complete workload specification.
use super::{Args, Case, Meter, Receipt, facade};
use crate::workload::{Generator, OperationId, Workload};
use anyhow::{Context, Result, ensure};
use lash_core::ToolDefinitionBindingExt;
use lash_core::llm::types::LlmContentBlock;
use lash_core::provider::{ProviderOptions, ProviderReliability};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Tools {
    workload: Arc<Workload>,
    contracts: BTreeMap<String, lash_core::ToolDefinition>,
    meter: Meter,
}
impl Tools {
    fn new(workload: Arc<Workload>, meter: &Meter) -> Result<Self> {
        let mut contracts = BTreeMap::new();
        for (name, arguments, result) in [
            (
                "synthetic",
                crate::workload::tool_schema(),
                crate::workload::tool_result_schema(),
            ),
            (
                "attach",
                crate::workload::attach_schema(),
                serde_json::json!({}),
            ),
            (
                "mark",
                crate::workload::mark_schema(),
                serde_json::json!({"type":"object"}),
            ),
        ] {
            let definition = lash_core::ToolDefinition::raw(
                format!("tool:{name}"),
                name,
                "Seeded synthetic boundary workload",
                arguments,
                result,
            )?
            .with_execution(Duration::from_secs(120))
            .with_tool_binding(
                lash_core::ToolBinding::new(["tools"], name).with_authority_type("Tools"),
            );
            contracts.insert(name.into(), definition);
        }
        Ok(Self {
            workload,
            contracts,
            meter: meter.clone(),
        })
    }
    async fn invoke(&self, call: &lash_core::ToolCall<'_>) -> Result<lash_core::ToolCallOutput> {
        let generator = Generator::new(&self.workload, "boundary-seeded")?;
        match call.name() {
            "synthetic" => {
                let key = call.args["record"]["key"]
                    .as_str()
                    .context("synthetic key")?;
                let bytes = call.args["record"]["result_bytes"]
                    .as_u64()
                    .context("result size")? as u32;
                let delay = call.args["record"]["callback_ms"]
                    .as_u64()
                    .context("callback delay")?;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok(lash_core::ToolCallOutput::success(
                    generator.tool_result(key, bytes)?,
                ))
            }
            "attach" => {
                let key = call.args["operation"]
                    .as_str()
                    .context("attachment operation")?;
                let (id, suffix) = OperationId::parse(key)?;
                ensure!(suffix.is_empty(), "attachment operation is not primary");
                let plan = generator.plan(id.actor, id.ordinal)?;
                let attachment = generator.attachment(
                    &plan,
                    call.args["index"].as_u64().context("blob index")? as usize,
                )?;
                let reference = call
                    .context
                    .attachments()
                    .put(
                        attachment.bytes,
                        lash_core::AttachmentCreateMeta::new(
                            attachment.media_type.parse()?,
                            None,
                            Some(attachment.blob_key),
                        ),
                    )
                    .await?;
                Ok(lash_core::ToolCallOutput::success_tool_value(
                    lash_core::ToolValue::Attachment(reference),
                ))
            }
            "mark" => Ok(lash_core::ToolCallOutput::success(
                serde_json::json!({"key": call.args["key"]}),
            )),
            _ => anyhow::bail!("unplanned tool"),
        }
    }
}
#[async_trait::async_trait]
impl lash_core::ToolProvider for Tools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.contracts.values().map(|d| d.manifest()).collect()
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.contracts.get(name).map(|d| Arc::new(d.contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let start = Instant::now();
        let result = self.invoke(&call).await;
        self.meter
            .record(&format!("seeded.tool.{}", call.name()), 1, start);
        match result {
            Ok(output) => lash_core::ToolOutcome::from_output(output).into(),
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

pub(super) async fn run(args: &Args) -> Result<Receipt> {
    let workload = Arc::new(Workload::named(&args.workload)?);
    ensure!(
        args.callers <= workload.spec().sessions as usize,
        "actor population exceeds seeded workload"
    );
    let meter = Meter::default();
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            args.store_dir.join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await?,
    );
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let backend = facade::backend(stores)?;
    let source = workload.clone();
    let calls = meter.clone();
    let mut options = ProviderOptions {
        reliability: ProviderReliability::standard(),
        ..Default::default()
    };
    options.reliability.retry.base_delay_ms = 1;
    options.reliability.retry.max_delay_ms = 1;
    options.reliability.retry.jitter_ms = 0;
    let provider = lash_core::testing::TestProvider::builder()
        .kind("boundary-seeded")
        .options(options)
        .complete(move |request| {
            let source = source.clone();
            let calls = calls.clone();
            async move {
                let start = Instant::now();
                let key = request
                    .messages
                    .iter()
                    .rev()
                    .flat_map(|m| m.blocks.iter().rev())
                    .find_map(|b| {
                        if let LlmContentBlock::Text { text, .. } = b {
                            text.strip_prefix("seeded-operation:")
                                .and_then(|s| s.lines().next())
                        } else {
                            None
                        }
                    })
                    .ok_or_else(|| {
                        lash_core::llm::transport::LlmTransportError::new(
                            "seeded operation missing",
                        )
                    })?;
                let generator = Generator::new(&source, "boundary-seeded").map_err(transport)?;
                let response = generator
                    .admitted_response(&[key.to_owned()], request.scope.attempt.unwrap_or(1))
                    .map_err(transport)?;
                calls.record("seeded.provider.attempt", 1, start);
                if response.retryable {
                    return Err(transport("seeded first-attempt retry before response")
                        .with_kind(lash_core::llm::transport::ProviderFailureKind::Transport)
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                        ));
                }
                let mut elapsed = 0;
                for chunk in response.chunks {
                    tokio::time::sleep(Duration::from_millis(u64::from(
                        chunk.due_ms.saturating_sub(elapsed),
                    )))
                    .await;
                    elapsed = chunk.due_ms;
                }
                Ok(facade::response(response.text))
            }
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let mut plugins = lash::PluginStack::new();
    plugins.push(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
            lash_core::lifetime::session_or_starter,
        ),
    ));
    let core = lash::LashCore::rlm_builder(backend, factory)
        .serve_test_llm_profile(provider, facade::metadata()?)
        .tools(Arc::new(Tools::new(workload.clone(), &meter)?))
        .plugins(plugins)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("boundary"),
            lash::persistence::LeaseIncarnationId::new("seeded"),
        ))?;
    let result = execute(args, &workload, &core, &meter).await;
    core.shutdown().await?;
    let evidence = result?;
    Ok(Receipt::new(
        Case::SeededPlan,
        "facade-rlm-durable-node",
        "sqlite-file-product",
        args.operations * args.callers,
        &meter,
        evidence,
    ))
}
fn transport(error: impl std::fmt::Display) -> lash_core::llm::transport::LlmTransportError {
    lash_core::llm::transport::LlmTransportError::new(error.to_string())
}
async fn execute(
    args: &Args,
    workload: &Workload,
    core: &lash::LashCore,
    meter: &Meter,
) -> Result<serde_json::Value> {
    let mut sessions = Vec::new();
    for actor in 0..args.callers {
        sessions.push(facade::create(core, &format!("seeded-{actor}")).await?);
    }
    let generator = Generator::new(workload, "boundary-seeded")?;
    futures_util::future::try_join_all(sessions.iter().enumerate().map(|(actor, session)| {
        let generator = &generator;
        async move {
            for ordinal in 0..args.operations {
                let start = Instant::now();
                let plan = generator.plan(actor as u64, ordinal as u64)?;
                let payload = generator.materialize(&plan)?;
                meter.record("seeded.plan.materialize", 1, start);
                let start = Instant::now();
                let handle = session.send(lash::TurnInput::text(format!("seeded-operation:{}\n{}\n{}", plan.operation.key(), payload.input, payload.prompt)))
                    .id(lash::TurnId::try_from(plan.operation.key())?).await?;
                meter.record("seeded.send.accept", 1, start);
                let start = Instant::now();
                let output = tokio::time::timeout(Duration::from_secs(120), handle.output()).await??;
                ensure!(matches!(&output.result.outcome, lash::TurnOutcome::Finished(lash::TurnFinish::FinalValue { value })
                    if value["operation"] == plan.operation.key()), "seeded cell failed: {:?}; errors={:?}; calls={:?}", output.result.outcome, output.result.errors, output.result.llm_calls);
                meter.record("seeded.send.settle", 1, start);
                // Queued plans are separate keyed sends; the sequential recipe
                // does not claim to model their active-turn scheduling share.
                for queued in &plan.queued_inputs {
                    let start = Instant::now();
                    let output = session.send(lash::TurnInput::text(format!("seeded-operation:{}", queued.idempotency_key)))
                        .id(lash::TurnId::try_from(queued.idempotency_key.clone())?).output().await?;
                    ensure!(matches!(&output.result.outcome, lash::TurnOutcome::Finished(lash::TurnFinish::FinalValue { value })
                        if value["operation"] == queued.idempotency_key), "queued seeded input did not finish");
                    meter.record("seeded.queued.settle", 1, start);
                }
            }
            anyhow::Ok(())
        }
    })).await?;
    let expected = args.operations * args.callers;
    ensure!(
        meter.count("seeded.send.settle") == expected,
        "seeded execution incomplete"
    );
    Ok(
        serde_json::json!({"seed": workload.spec().seed, "workload_sha256": workload.sha256(),
        "actors": args.callers, "primary_turns": expected, "queued_turns": meter.count("seeded.queued.settle"),
        "executed": ["generated-cell", "tools", "attachment-puts", "child-processes", "queued-inputs"],
        "unmeasured_plan_fields": ["host-processes", "auxiliary-llm", "fault-windows", "maintenance", "arrival-schedule", "observation-load"]}),
    )
}
