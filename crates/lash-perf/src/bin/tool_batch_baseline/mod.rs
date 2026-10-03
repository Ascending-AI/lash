//! Receipt boundary: session send through consumption, incorporation and scope close.
//! Counts on this tier are structural evidence; its instrumented latency is diagnostic.

#![expect(
    clippy::expect_used,
    reason = "fixed fixture schemas and gates; a violated invariant invalidates the measurement"
)]

use anyhow::{Context as _, ensure};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::llm::types::{LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse};
use lash_core::perf_witness::Collector;
use lash_restate_test::{RestateTestBackend, ServerConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static MEASUREMENT_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Branch {
    Done,
    Retry,
    Deferred,
    Cancel,
    DeclaredStart,
    RaceLoser,
    TurnTransfer,
    ProcessTransfer,
}

#[derive(Default, Serialize)]
pub(super) struct Counts {
    pub total: usize,
    pub by_kind: BTreeMap<String, usize>,
}
impl Counts {
    fn add(&mut self, kind: &str) {
        self.total += 1;
        *self.by_kind.entry(kind.to_owned()).or_default() += 1;
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct Invocation {
    id: String,
    invoked_by_id: Option<String>,
    target_service_name: String,
    target_service_key: Option<String>,
    target_handler_name: String,
    status: String,
    journal_size: usize,
    attempts: usize,
    suspensions: usize,
    endpoint_request_bytes: u64,
    endpoint_response_frames: Vec<(String, usize, u64)>,
    sdk_input_waits: Vec<(u64, Option<u64>)>,
}
#[derive(Serialize)]
struct Entry {
    id: String,
    index: usize,
    entry_type: String,
    name: Option<String>,
    payload_bytes: usize,
    #[serde(rename = "payload_base64", serialize_with = "serialize_payload")]
    data: lash_restate_test::JournalEntryView,
    request_copies: usize,
    output_copies: usize,
    request_decimal_copies: usize,
    output_decimal_copies: usize,
}

fn serialize_payload<S: serde::Serializer>(
    data: &lash_restate_test::JournalEntryView,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_str(&base64::display::Base64Display::new(
        &data.payload,
        &base64::engine::general_purpose::STANDARD,
    ))
}

#[derive(Serialize)]
pub(super) struct Receipt {
    contract: &'static str,
    source_sha: String,
    fixture: Value,
    branch_observation: Value,
    pub source: Counts,
    pub engine: Counts,
    bytes: Value,
    rpc: Value,
    sql: Value,
    waits: Value,
    latency: Value,
    environment: Value,
    pub invocations: Vec<Invocation>,
    journal: Vec<Entry>,
}

#[derive(Clone)]
struct Tool {
    branch: Branch,
    size: usize,
    host: Arc<dyn lash_core::EffectHost>,
    attempts: Arc<AtomicUsize>,
    reached: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
    completions: Arc<Mutex<Vec<lash_core::AwaitEventKey>>>,
    race_probe_completion: Arc<Mutex<Option<lash_core::AwaitEventKey>>>,
    winner_consumed: Arc<tokio::sync::Semaphore>,
}
impl Tool {
    fn definition(&self) -> lash_core::ToolDefinition {
        let mut definition = lash_core::ToolDefinition::raw(
            "tool:cost",
            "cost",
            "Return the controlled payload",
            json!({"type":"object","additionalProperties":true}),
            json!({"type":"object","additionalProperties":true}),
        )
        .expect("fixed schemas");
        if matches!(self.branch, Branch::RaceLoser | Branch::ProcessTransfer) {
            definition = definition
                .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["tools"], "cost"));
        }
        if matches!(self.branch, Branch::Retry) {
            definition.manifest.retry_policy = lash_core::ToolRetryPolicy::safe(2, 1, 1);
        }
        if matches!(
            self.branch,
            Branch::Deferred | Branch::DeclaredStart | Branch::RaceLoser
        ) {
            definition = definition.with_declaration(lash_core::ToolDeclaration::deferring());
        }
        definition
    }
}
#[async_trait::async_trait]
impl lash_core::ToolProvider for Tool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![self.definition().manifest()]
    }
    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "cost").then(|| Arc::new(self.definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if call
            .context
            .session_id()
            .is_ok_and(|id| id.as_str() != "cost")
        {
            return lash_core::ToolOutcome::ok(json!({"output":"x".repeat(self.size)})).into();
        }
        self.reached.add_permits(1);
        match self.branch {
            Branch::Retry if call.context.attempt_number() == 1 => {
                return lash_core::ToolOutcome::retryable_failure(
                    lash_core::ToolFailureClass::External,
                    "controlled_retry",
                    "controlled reported retry",
                    Some(1),
                )
                .into();
            }
            Branch::DeclaredStart => {
                let create = lash_core::SessionCreateRequest::child_session(
                    "cost",
                    lash_core::SessionStartPoint::Empty,
                    lash_core::PluginOptions::default(),
                )
                .with_session_id(lash_core::SessionId::fixture(format!(
                    "child-{}",
                    call.context.call_id()
                )))
                .with_spec(&lash::SessionSpec::new(
                    "mock-model",
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                ))
                .expect("child spec");
                let declaration = lash_core::ProcessStartDeclaration::new(
                    lash_core::ProcessInput::SessionTurn {
                        definition_key: "cost-child-v1".into(),
                        create_request: Box::new(create),
                        turn_input: Box::new(lash::TurnInput::text("process-payload")),
                        result: lash_core::SessionTurnOutcome::FinalValue { schema: None },
                    },
                    lash_core::ProcessOriginator::Session {
                        session_id: call.context.session_id().expect("parent session").clone(),
                        agent_frame_id: Some(
                            call.context.agent_frame_id().expect("parent frame").clone(),
                        ),
                    },
                    lash_core::Lifetime::Detached,
                )
                .with_env_ref(
                    call.context
                        .process_execution_env_ref()
                        .expect("admitted environment"),
                );
                let start = lash_core::DeclaredStart::new(
                    call.context,
                    lash_core::StartProcessIntent {
                        owner: call.context.owner().runtime_owner(),
                        declaration,
                    },
                )
                .expect("declared start");
                return lash_core::ToolOutcome::pending(
                    lash_core::PendingCompletion::new().resolved_by_declared_start(start),
                )
                .into();
            }
            Branch::RaceLoser
                if call.args.get("race_probe").and_then(Value::as_bool) == Some(true) =>
            {
                self.winner_consumed.add_permits(1);
                *self.race_probe_completion.lock().expect("race probe key") =
                    Some(call.context.completion_key().expect("Deferred race probe"));
                return lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into();
            }
            Branch::Deferred | Branch::RaceLoser
                if !matches!(self.branch, Branch::RaceLoser)
                    || call.args.get("position").and_then(Value::as_u64) != Some(0) =>
            {
                let key = call.context.completion_key().expect("declared Deferred");
                self.completions.lock().expect("completion list").push(key);
                return lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into();
            }
            Branch::Cancel => {
                self.release.acquire().await.expect("release gate").forget();
            }
            _ => {}
        }
        lash_core::ToolOutcome::ok(json!({"output":"x".repeat(self.size)})).into()
    }
}

fn response(parts: Vec<LlmOutputPart>) -> LlmResponse {
    LlmResponse {
        parts,
        ..Default::default()
    }
}

pub(super) async fn measure(
    branch: Branch,
    width: usize,
    size: usize,
    source_sha: &str,
) -> anyhow::Result<Receipt> {
    let _measurement = MEASUREMENT_GATE.lock().await;
    let config = ServerConfig::default().with_cost_receipts();
    let config = if matches!(branch, Branch::ProcessTransfer) {
        config.with_run_effect_budget(10_000)
    } else {
        config
    };
    let backend = if matches!(branch, Branch::TurnTransfer | Branch::ProcessTransfer) {
        lash_restate_test::backend_with_segment_budget(0x4868, config, 1).await?
    } else {
        lash_restate_test::backend(0x4868, config).await?
    };
    let source_backend = backend.lash_backend();
    let tool = Tool {
        branch,
        size,
        host: source_backend.effect_host(),
        attempts: Arc::default(),
        reached: Arc::new(tokio::sync::Semaphore::new(0)),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        completions: Arc::default(),
        race_probe_completion: Arc::default(),
        winner_consumed: Arc::new(tokio::sync::Semaphore::new(0)),
    };
    let model_calls = Arc::new(AtomicUsize::new(0));
    let model_feedback = Arc::new(Mutex::new(String::new()));
    let incorporated = Arc::new(AtomicUsize::new(0));
    let incorporated_material = Arc::new(AtomicUsize::new(0));
    let presentation_bytes = Arc::new(AtomicUsize::new(0));
    let distinct_presentation_bytes = Arc::new(AtomicUsize::new(0));
    let provider = lash_core::testing::TestProvider::builder()
        .kind("tool-cost")
        .complete({
            let model_calls = Arc::clone(&model_calls);
            let model_feedback = Arc::clone(&model_feedback);
            let incorporated = Arc::clone(&incorporated);
            let incorporated_material = Arc::clone(&incorporated_material);
            let presentation_bytes = Arc::clone(&presentation_bytes);
            let distinct_presentation_bytes = Arc::clone(&distinct_presentation_bytes);
            move |request: LlmRequest| {
                let model_ordinal = model_calls.fetch_add(1, Ordering::SeqCst);
                if model_ordinal > 0 && matches!(branch, Branch::RaceLoser | Branch::ProcessTransfer) {
                    *model_feedback.lock().expect("fixture feedback") =
                        serde_json::to_string(&request.messages.iter().rev().take(2).collect::<Vec<_>>())
                            .expect("fixture messages");
                }
                let parent = request.session_id().as_str() == "cost";
                let results = request
                    .messages
                    .iter()
                    .flat_map(|message| message.blocks.iter())
                    .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
                    .count();
                if parent {
                    incorporated.store(results, Ordering::SeqCst);
                    let semantic = json!({"output":"x".repeat(size)}).to_string();
                    let texts = request.messages.iter().flat_map(|message| message.blocks.iter()).filter_map(|block| {
                        if let LlmContentBlock::ToolResult {content,..}=block {Some(content)} else {None}
                    }).flatten().filter_map(|part| { if let lash::messages::ModelToolReturnPart::Text {text}=part {Some(text)} else {None} }).collect::<Vec<_>>();
                    presentation_bytes.store(texts.iter().map(|text| text.len()).sum(), Ordering::SeqCst);
                    distinct_presentation_bytes.store(texts.iter().filter(|text| text.as_str()!=semantic).map(|text| text.len()).sum(), Ordering::SeqCst);

                    incorporated_material.store(request.messages.iter().flat_map(|message| message.blocks.iter()).filter(|block| {
                        matches!(block, LlmContentBlock::ToolResult {content,..} if content.iter().any(|part| matches!(part, lash::messages::ModelToolReturnPart::Text {text} if text.contains(&"x".repeat(size)))))
                    }).count(), Ordering::SeqCst);
                }
                async move {
                    Ok::<_, lash_core::llm::transport::LlmTransportError>(if matches!(branch, Branch::ProcessTransfer) && model_ordinal>0 {
                        response(vec![LlmOutputPart::Text{text:"consumed".into(),response_meta:None}])
                    } else if matches!(branch, Branch::ProcessTransfer) {
                        let body = format!("const body=async()=>{{await tools.cost({{request:{request}}}); return await tools.cost({{request:{request}}});}};", request=json!("q".repeat(size)));
                        let starts = (0..width).map(|index| format!("const handle{index}=await processes.start({{definition:body}});")).collect::<Vec<_>>().join("\n");
                        let answers = (0..width).map(|index| format!("await handle{index}")).collect::<Vec<_>>().join(",");
                        response(vec![LlmOutputPart::Text {text:format!("<typescript>\n{body}\n{starts}\nfinish([{answers}]);\n</typescript>"),response_meta:None}])

                    } else if matches!(branch, Branch::RaceLoser) {
                        let calls = (0..width).map(|position| format!("tools.cost({{position:{position},request:{}}})", json!("q".repeat(size)))).collect::<Vec<_>>().join(",");
                        response(vec![LlmOutputPart::Text { text: format!("<typescript>\nconst pending=[{calls}];\nconst winner=await Promise.race(pending);\nawait tools.cost({{race_probe:true}});\nfinish(winner);\n</typescript>"), response_meta:None }])
                    } else if results == 0 {
                        response(
                            (0..if parent { width } else { 1 })
                                .map(|index| LlmOutputPart::ToolCall {
                                    call_id: format!("call-{index}"),
                                    tool_name: "cost".into(),
                                    input_json: json!({"request":"q".repeat(size)}).to_string(),
                                    replay: None,
                                })
                                .collect(),
                        )
                    } else {
                        response(vec![LlmOutputPart::Text {
                            text: if parent { "consumed".into() } else { "x".repeat(size) },
                            response_meta: None,
                        }])
                    })
                }
            }
        })
        .build()
        .into_handle();
    let builder = if matches!(branch, Branch::RaceLoser | Branch::ProcessTransfer) {
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(lash_protocol_rlm::TypescriptDialect),
            &source_backend,
        );
        let factory = if matches!(branch, Branch::ProcessTransfer) {
            let mut config = factory.worker_service().config().clone();
            config.max_queue_items = 2 * width + 4;
            factory.with_worker_service(lash_vm_client::service::Service::new(config))
        } else {
            factory
        };
        lash::LashCore::rlm_builder(source_backend, factory)
    } else {
        lash::LashCore::standard_builder(source_backend)
    };
    let builder = if matches!(branch, Branch::ProcessTransfer) {
        builder.plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
    } else {
        builder
    };
    let core = builder
        .output_retention(lash::attachments::OutputRetentionPolicy {
            inline_limit_bytes: 128 * 1024 * 1024,
            witness_bytes: 4096,
        })
        .commit_budget(lash::CommitBudget::bounded(128 * 1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .serve_test_llm_profile(
            provider,
            lash_core::LlmProfileMetadata::builder("mock-model")
                .context_window_tokens(32_000_000)
                .build()?,
        )
        .tools(Arc::new(tool.clone()) as Arc<dyn lash_core::ToolProvider>)
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "tool-cost",
            "receipt",
        ))?;
    backend.install_process_worker(lash_core_worker::DurableProcessWorker::new(
        core.durable_process_worker_config()?,
    )?);
    let spec = lash::SessionSpec::new(
        "mock-model",
        lash::TurnBudget::Unbounded,
        lash::MaxToolCalls::new(1024),
    );
    let spec = if matches!(branch, Branch::RaceLoser | Branch::ProcessTransfer) {
        spec.plugin_options(lash_core::PluginOptions::typed(
            lash_protocol_rlm::RLM_PROTOCOL_PLUGIN_ID,
            lash::rlm::RlmCreateExtras {
                final_answer_format: Some(lash::rlm::RlmFinalAnswerFormat::RawFinalValue),
                ..Default::default()
            },
        )?)
    } else {
        spec
    };
    core.session("cost")
        .create(lash::SessionCreation::root(spec))
        .await?;
    let session = core.session("cost").open().await?;
    backend.server().settle().await;
    let before: BTreeSet<String> = backend
        .server()
        .invocations()
        .into_iter()
        .map(|view| view.id)
        .collect();
    let http_before = backend.server().stats().http_requests.len();
    let collector = Collector::install_with_sql_receipts()?;
    let started = Instant::now();
    let handle = session
        .send(lash::TurnInput::text("execute the controlled round"))
        .id("cost-run")
        .await?;
    let admitted_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut surviving_losers = 0;
    if matches!(
        branch,
        Branch::Deferred | Branch::Cancel | Branch::RaceLoser
    ) {
        let barriers = tokio::time::timeout(Duration::from_secs(60), async {
            if matches!(branch, Branch::RaceLoser) {
                tool.winner_consumed
                    .acquire()
                    .await
                    .expect("winner consumed")
                    .forget();
            }
            for _ in 0..width + usize::from(matches!(branch, Branch::RaceLoser)) {
                tool.reached.acquire().await.expect("reach gate").forget();
            }
        });
        barriers.await.context("all controlled attempts start")?;
        if matches!(branch, Branch::Cancel) {
            handle.cancel().reason("controlled cancellation").await?;
            tool.release.add_permits(width);
        } else {
            backend.server().settle().await;
            let keys = std::mem::take(&mut *tool.completions.lock().expect("completion keys"));
            let expected = if matches!(branch, Branch::RaceLoser) {
                width - 1
            } else {
                width
            };
            ensure!(
                keys.len() == expected,
                "every controlled loser or deferred attempt remains pending"
            );
            if matches!(branch, Branch::RaceLoser) {
                surviving_losers = keys.len();
            }
            for key in keys {
                tool.host
                    .resolve_await_event(
                        &key,
                        lash_core::Resolution::Ok(json!({"output":"x".repeat(size)})),
                    )
                    .await?;
            }
            if matches!(branch, Branch::RaceLoser) {
                backend.server().settle().await;
                let key = tool
                    .race_probe_completion
                    .lock()
                    .expect("race probe key")
                    .take()
                    .expect("armed race drain gate");
                tool.host
                    .resolve_await_event(
                        &key,
                        lash_core::Resolution::Ok(json!({"output":"x".repeat(size)})),
                    )
                    .await?;
            }
        }
    }
    let guard_refusal = async {
        backend.server().settle().await;
        let mut refusals = Vec::new();
        for view in backend.server().invocations() {
            for entry in backend.server().journal(&view.id).unwrap_or_default() {
                if entry.ty != lash_restate_test::protocol::MessageType::OutputCommand {
                    continue;
                }
                let frame = lash_restate_test::protocol::Frame::new(entry.ty, entry.payload);
                let output = frame
                    .decode::<lash_restate_test::protocol::generated::OutputCommandMessage>()
                    .expect("server journal output");
                if let Some(
                    lash_restate_test::protocol::generated::output_command_message::Result::Failure(
                        failure,
                    ),
                ) = output.result
                    && failure.code == 400
                    && failure
                        .message
                        .contains("JSON decode nodes limit 1000000 exceeded by 1000001")
                {
                    refusals.push(json!({"id":view.id,"target":view.target,"code":failure.code,"message":failure.message}));
                }
            }
        }
        if refusals.is_empty() {
            std::future::pending::<Vec<Value>>().await
        } else {
            refusals
        }
    };
    let (output, refusals) = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(120), handle.output()) => (Some(result.with_context(|| format!("controlled Run completion; feedback {}",model_feedback.lock().expect("fixture feedback")))??), Vec::new()),
        refusals = guard_refusal => (None, refusals),
    };
    let consumed_ms = started.elapsed().as_secs_f64() * 1000.0;
    backend.server().settle().await;
    let finished_ms = started.elapsed().as_secs_f64() * 1000.0;
    let work = collector.snapshot();
    drop(collector);
    let http_requests = backend
        .server()
        .stats()
        .http_requests
        .into_iter()
        .skip(http_before)
        .collect::<Vec<_>>();
    let (invocations, journal) = tree(&backend, &before, size).await?;
    let mut source = Counts::default();
    let mut engine = Counts::default();
    let mut payload_bytes = 0;
    for entry in &journal {
        engine.add(&entry.entry_type);
        payload_bytes += entry.payload_bytes;
        match entry.entry_type.as_str() {
            "CallCommand"
            | "OneWayCallCommand"
            | "RunCommand"
            | "SleepCommand"
            | "AwakeableCommand"
            | "CompleteAwakeableCommand" => source.add(&entry.entry_type),
            _ => {}
        }
    }
    let sql_rows: Vec<_> = work
        .sql_receipts
        .iter()
        .map(|row| json!({"sql":row.sql,"connection_worker":row.connection_worker,"expanded_bytes":row.expanded_bytes}))
        .collect();
    let attempts = tool.attempts.load(Ordering::SeqCst);
    let mut results = incorporated.load(Ordering::SeqCst);
    if let Some(output) = &output {
        if matches!(branch, Branch::RaceLoser) {
            ensure!(
                matches!(&output.result.outcome, lash::TurnOutcome::Finished(lash::TurnFinish::FinalValue {value}) if value["output"]=="x".repeat(size)),
                "race incorporates the successful winner, observed {:?}; feedback {}",
                output.result.outcome,
                model_feedback.lock().expect("fixture feedback")
            );
            results = 1;
        } else if matches!(branch, Branch::ProcessTransfer) {
            ensure!(
                matches!(&output.result.outcome, lash::TurnOutcome::Finished(lash::TurnFinish::FinalValue {value}) if value.as_array().is_some_and(|rows| rows.len()==width)),
                "RLM consumes every controlled result, observed {:?}; feedback {}",
                output.result.outcome,
                model_feedback.lock().expect("fixture feedback")
            );
            if let lash::TurnOutcome::Finished(lash::TurnFinish::FinalValue { value }) =
                &output.result.outcome
            {
                ensure!(
                    value
                        .as_array()
                        .expect("checked result array")
                        .iter()
                        .all(|row| row.to_string().contains(&"x".repeat(size))),
                    "RLM incorporates successful controlled material"
                );
                results = width;
            }
        } else if matches!(branch, Branch::Cancel) {
            ensure!(
                output.result.outcome.cancellation().is_some(),
                "Run reports typed cancellation"
            );
        } else {
            if size == 32 {
                ensure!(
                    incorporated_material.load(Ordering::SeqCst) == width,
                    "all small tool results contain successful material"
                );
            }
            ensure!(
                results == width,
                "consuming model incorporated {results} of {width} results"
            );
        }
    } else {
        ensure!(
            matches!(branch, Branch::Done) && size >= 1_000_000 && !refusals.is_empty(),
            "only the recorded predecessor node-limit refusal has a prefix boundary"
        );
    }
    let complete = output.is_some();
    let outcome = output
        .as_ref()
        .map(|output| format!("{:?}", output.result.outcome));
    let suspensions: usize = invocations
        .iter()
        .map(|invocation| invocation.suspensions)
        .sum();
    let request_bytes: u64 = invocations
        .iter()
        .map(|invocation| invocation.endpoint_request_bytes)
        .sum();
    let response_bytes: usize = invocations
        .iter()
        .flat_map(|invocation| &invocation.endpoint_response_frames)
        .map(|(_, bytes, _)| bytes)
        .sum();
    Ok(Receipt {
        contract: "lash.tool-cost.controlled.v1",
        source_sha: source_sha.to_owned(),
        fixture: json!({"branch":branch,"width":width,"payload_bytes":size,"seed":0x4868,"request_marker":"q","output_marker":"x","producer":if matches!(branch,Branch::RaceLoser|Branch::ProcessTransfer) {"rlm"} else {"standard"},"samples":1}),
        branch_observation: json!({"attempts":attempts,"incorporated":results,"incorporated_material":incorporated_material.load(Ordering::SeqCst),"model_calls":model_calls.load(Ordering::SeqCst),"outcome":outcome,"boundary_complete":complete,"codec_refusals":refusals,"historical_source_hypothesis":14+19*width,"target_source_budget":1+3*width,"target_comparison_boundary":"tool route; derived from complete raw receipt by tool_cost_census","no_declared_intents":!matches!(branch,Branch::DeclaredStart|Branch::ProcessTransfer),"surviving_losers_after_winner":surviving_losers,"no_attachments":true,"shared_setup_excluded":true}),
        bytes: json!({"canonical_request":width*size*if matches!(branch,Branch::ProcessTransfer) {2} else {1},"canonical_output":width*size*if matches!(branch,Branch::ProcessTransfer) {2} else {1},"distinct_presentation":distinct_presentation_bytes.load(Ordering::SeqCst),"presented_to_model":presentation_bytes.load(Ordering::SeqCst),"canonical_boundary":"request/output marker bytes; JSON wrappers and physical projections are counted separately","journal_protobuf_payload":payload_bytes,"endpoint_request_framed":request_bytes,"endpoint_response_framed":response_bytes,"transport_boundary":"Endpoint::handle body bytes, including replay, ACKs and framing; excludes HTTP headers and network framing","request_payload_occurrences":journal.iter().map(|row|row.request_copies).sum::<usize>(),"output_payload_occurrences":journal.iter().map(|row|row.output_copies).sum::<usize>(),"request_decimal_occurrences":journal.iter().map(|row|row.request_decimal_copies).sum::<usize>(),"output_decimal_occurrences":journal.iter().map(|row|row.output_decimal_copies).sum::<usize>()}),
        rpc: json!({"http_requests":http_requests,"endpoint_streams":invocations.iter().map(|row|row.attempts).sum::<usize>(),"logical_call_commands":source.by_kind.get("CallCommand").copied().unwrap_or(0),"logical_send_commands":source.by_kind.get("OneWayCallCommand").copied().unwrap_or(0)}),
        sql: json!({"statements":work.sql_statements,"by_verb":work.sql_statements_by_verb,"application_transactions":work.sql_statements_by_verb["begin"],"expanded_statement_bytes":work.sql_receipts.iter().map(|row|row.expanded_bytes).sum::<usize>(),"rows":sql_rows,"engine_storage":"not SQL on the server double"}),
        waits: json!({"invocation_suspensions":suspensions,"endpoint_attempts":invocations.iter().map(|row|row.attempts).sum::<usize>(),"critical_path":"SDK input starvation intervals on the opener; parallel descendant intervals are retained separately"}),
        latency: json!({"send_to_admission_ms":admitted_ms,"send_to_consumption_ms":if complete {Some(consumed_ms)} else {None},"full_run_through_scope_close_ms":if complete {Some(finished_ms)} else {None},"send_to_refusal_prefix_ms":if complete {None} else {Some(finished_ms)},"boundary_complete":complete,"errors":if complete {0} else {refusals.len()},"timeouts":0,"role":"instrumented double diagnostic; no live latency acceptance claim"}),
        environment: json!({"tier":"in-process Restate server double","store":"SQLite memory","protocol":format!("{:?}",backend.server().config().protocol),"tool_group_width":width,"run_concurrency":1,"process_vm_queue_items":if matches!(branch,Branch::ProcessTransfer) {Some(2*width+4)} else {None},"cache":"fresh server, core and session; admitted session setup excluded","loadavg":std::fs::read_to_string("/proc/loadavg").unwrap_or_default(),"cpu_psi":std::fs::read_to_string("/proc/pressure/cpu").unwrap_or_default(),"logical_cpus":std::thread::available_parallelism().map_or(1,usize::from)}),
        source,
        engine,
        invocations,
        journal,
    })
}

async fn tree(
    backend: &RestateTestBackend,
    before: &BTreeSet<String>,
    size: usize,
) -> anyhow::Result<(Vec<Invocation>, Vec<Entry>)> {
    let admin =
        lash_restate::RestateAdminClient::new(lash_restate::RestateConnection::with_transport(
            backend.server().ingress_url(),
            backend.server().transport(),
        ));
    let invocations: Vec<Invocation> = admin.query_json("SELECT * FROM sys_invocation").await?;
    // The fixture has a private server; every post-admission invocation belongs
    // to this Run, including external Deferred resolution and retirement.
    let invocations: Vec<_> = invocations
        .into_iter()
        .filter(|row| !before.contains(&row.id))
        .collect();
    let captured: BTreeSet<_> = invocations.iter().map(|row| row.id.clone()).collect();
    let population: BTreeSet<_> = backend
        .server()
        .invocations()
        .into_iter()
        .filter(|view| !before.contains(&view.id))
        .map(|view| view.id)
        .collect();
    ensure!(
        captured == population,
        "admin census retains the entire new invocation population"
    );
    ensure!(
        invocations.iter().all(|row| row
            .invoked_by_id
            .as_ref()
            .is_none_or(|id| captured.contains(id))),
        "all invocation ancestors are retained"
    );
    let mut journal = Vec::new();
    let request_marker = "q".repeat(size);
    let output_marker = "x".repeat(size);
    let request_decimal_marker = format!("{}113", "113,".repeat(size - 1));
    let output_decimal_marker = format!("{}120", "120,".repeat(size - 1));
    for invocation in &invocations {
        for (index, data) in backend
            .server()
            .journal(&invocation.id)
            .context("retained journal")?
            .into_iter()
            .enumerate()
        {
            let text = String::from_utf8_lossy(&data.payload);
            let request_copies = text.matches(&request_marker).count();
            let output_copies = text.matches(&output_marker).count();
            let request_decimal_copies = text.matches(&request_decimal_marker).count();
            let output_decimal_copies = text.matches(&output_decimal_marker).count();
            journal.push(Entry {
                id: invocation.id.clone(),
                index,
                entry_type: format!("{:?}", data.ty),
                name: data.name.clone(),
                payload_bytes: data.payload.len(),
                data,
                request_copies,
                output_copies,
                request_decimal_copies,
                output_decimal_copies,
            });
        }
    }
    Ok((invocations, journal))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn emit(receipt: &Receipt) {
        use std::io::Write as _;
        let gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, gzip);
        serde_json::to_writer(&mut buffered, receipt).expect("receipt JSON");
        buffered.flush().expect("receipt stream");
        let gzip = buffered.into_inner().expect("receipt buffer");
        println!("COST_RECEIPT_GZIP_BASE64 {}", {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(gzip.finish().expect("receipt gzip"))
        });
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn complete_cost_receipt_includes_descendants_and_incorporation() {
        let receipt = measure(
            Branch::Done,
            1,
            32,
            "e4735b406286c266e6ef2965242634afcd35d6a5",
        )
        .await
        .expect("receipt");
        assert!(
            receipt
                .invocations
                .iter()
                .any(|row| row.target_service_name.starts_with("EffectGroupDispatch")),
            "the invocation tree must include the dispatch, not just the opener"
        );
        assert!(receipt.engine.total > receipt.invocations[0].journal_size);
        assert!(
            receipt.source.total > 4,
            "four-record target must fail on the old route"
        );
        emit(&receipt);
    }

    async fn done_bucket(size: usize) {
        for width in [1, 2, 16] {
            if size == 32 && width == 1 {
                continue;
            }
            let receipt = measure(
                Branch::Done,
                width,
                size,
                "e4735b406286c266e6ef2965242634afcd35d6a5",
            )
            .await
            .expect("Done receipt");
            assert_eq!(receipt.branch_observation["attempts"], width);
            if size == 1048576 {
                assert_eq!(receipt.branch_observation["boundary_complete"], false);
                assert!(
                    receipt.branch_observation["codec_refusals"]
                        .as_array()
                        .is_some_and(|rows| rows.iter().any(|row| row["target"]
                            .as_str()
                            .is_some_and(|target| target.starts_with("EffectGroupPayload/"))))
                );
            } else {
                assert_eq!(receipt.branch_observation["boundary_complete"], true);
            }
            emit(&receipt);
        }
    }
    async fn branch_bucket(branch: Branch) {
        for width in [1, 2, 16] {
            let receipt = measure(
                branch,
                width,
                32,
                "e4735b406286c266e6ef2965242634afcd35d6a5",
            )
            .await
            .expect("branch receipt");
            match branch {
                Branch::Retry => assert_eq!(receipt.branch_observation["attempts"], 2 * width),
                Branch::RaceLoser => assert_eq!(
                    receipt.branch_observation["surviving_losers_after_winner"],
                    width - 1
                ),
                Branch::TurnTransfer => assert!(
                    receipt
                        .invocations
                        .iter()
                        .filter(|row| row.target_service_name.starts_with("LashTurn")
                            && row.target_handler_name == "run")
                        .count()
                        > 1,
                    "actual turn successor"
                ),
                Branch::ProcessTransfer => assert!(
                    receipt
                        .invocations
                        .iter()
                        .filter(
                            |row| row.target_service_name.starts_with("LashProcessWorkflow")
                                && row.target_handler_name == "run"
                        )
                        .count()
                        > width,
                    "actual process successor"
                ),
                Branch::DeclaredStart => assert_eq!(
                    receipt
                        .invocations
                        .iter()
                        .filter(
                            |row| row.target_service_name.starts_with("LashProcessWorkflow")
                                && row.target_handler_name == "run"
                        )
                        .count(),
                    width
                ),
                _ => {}
            }
            emit(&receipt);
        }
    }
    macro_rules! bucket_test {
        ($name:ident, $work:expr) => {
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                $work.await;
            }
        };
    }
    bucket_test!(done_small, done_bucket(32));
    bucket_test!(done_eight_kib, done_bucket(8192));
    bucket_test!(done_two_fifty_six_kib, done_bucket(262144));
    bucket_test!(done_sixty_four_kib, done_bucket(65536));
    bucket_test!(done_one_mib, done_bucket(1048576));
    bucket_test!(reported_retry, branch_bucket(Branch::Retry));
    bucket_test!(external_deferred, branch_bucket(Branch::Deferred));
    bucket_test!(declared_process_start, branch_bucket(Branch::DeclaredStart));
    bucket_test!(run_cancellation, branch_bucket(Branch::Cancel));
    bucket_test!(surviving_race_loser, branch_bucket(Branch::RaceLoser));
    bucket_test!(turn_continuation, branch_bucket(Branch::TurnTransfer));
    bucket_test!(process_continuation, branch_bucket(Branch::ProcessTransfer));
}
