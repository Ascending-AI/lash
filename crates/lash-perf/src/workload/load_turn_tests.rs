//! In-process replays of `smoke-v1` load turns (FIG-4255).
//!
//! A load turn is seeded per actor and ordinal, so a turn that failed on the
//! kind topology fails the same way here: the same served cell, prompt layer
//! and input, the same session history (every earlier turn of the turn's
//! session, in order) and the load tools' own catalog shapes, with no
//! topology beneath them. Only the durable witness the kind driver records is
//! left out: a tool here answers what the workload regenerates for its key.

use super::{Generator, OperationId, TurnPlan, Workload};
use lash::TurnInput;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolBinding, ToolCall, ToolDefinition,
    ToolDefinitionBindingExt as _, ToolOutcome,
};
use lash_core::llm::types::{LlmOutputPart, LlmResponse};
use lash_core::testing::TestProvider;
use lash_sansio::sync::MutexExt as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const RUN: &str = "fig-4255";
/// Enough regenerations for a cell whose start is refused to stop the turn,
/// and more than a cell whose start registers ever needs.
const TURN_BUDGET: usize = 3;

/// The cell the provider serves: the served response of the turn in flight,
/// regenerated whole on every provider call as the kind mock provider does.
/// It also keeps the last failed-start feedback a regeneration request
/// carried back to the model, so a failing replay names its refusal.
#[derive(Clone, Default)]
struct ServedCell {
    text: Arc<Mutex<String>>,
    unrealized_feedback: Arc<Mutex<Option<String>>>,
}

impl ServedCell {
    fn serve(&self, text: String) {
        *self.text.lock_recover() = text;
    }

    fn unrealized_feedback(&self) -> Option<String> {
        self.unrealized_feedback.lock_recover().clone()
    }

    fn provider(&self) -> TestProvider {
        let served = self.clone();
        TestProvider::builder()
            .kind("fig-4255-load-replay")
            .complete(move |request| {
                let seen = format!("{request:?}");
                if let Some(at) = seen.rfind("did not register a process") {
                    *served.unrealized_feedback.lock_recover() =
                        Some(seen[at..].chars().take(320).collect());
                }
                let text = served.text.lock_recover().clone();
                async move {
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text,
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            })
            .build()
    }
}

/// The load tools with the kind worker's catalog shapes; `mark` keeps every
/// key a child process marked.
#[derive(Clone)]
struct ReplayTools {
    workload: Arc<Workload>,
    marks: Arc<Mutex<Vec<String>>>,
}

impl ReplayTools {
    fn definitions() -> Vec<ToolDefinition> {
        [
            (
                "tool:synthetic",
                "synthetic",
                super::tool_schema(),
                super::tool_result_schema(),
            ),
            (
                "tool:mark",
                "mark",
                super::mark_schema(),
                json!({
                    "type": "object",
                    "properties": { "key": { "type": "string" } },
                    "required": ["key"],
                    "additionalProperties": false
                }),
            ),
            (
                "tool:attach",
                "attach",
                super::attach_schema(),
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

    fn generator(&self) -> Generator<'_> {
        Generator::new(&self.workload, RUN).expect("the replay run id is valid")
    }

    fn synthetic(&self, call: &ToolCall<'_>) -> anyhow::Result<Value> {
        let record = call.args.get("record").expect("a synthetic record");
        let key = record["key"].as_str().expect("a synthetic key");
        let result_bytes = record["result_bytes"].as_u64().expect("a result size");
        self.generator()
            .tool_result(key, u32::try_from(result_bytes)?)
    }

    fn mark(&self, call: &ToolCall<'_>) -> Value {
        let key = call.args["key"].as_str().expect("a mark key").to_owned();
        self.marks.lock_recover().push(key.clone());
        json!({ "key": key })
    }

    async fn attach(&self, call: &ToolCall<'_>) -> anyhow::Result<lash_core::ToolValue> {
        let operation = call.args["operation"]
            .as_str()
            .expect("an attach operation");
        let index = call.args["index"].as_u64().expect("an attach index");
        let (operation, _) = OperationId::parse(operation)?;
        let generator = self.generator();
        let plan = generator.plan(operation.actor, operation.ordinal)?;
        let blob = generator.attachment(&plan, usize::try_from(index)?)?;
        let reference = call
            .context
            .attachments()
            .put(
                blob.bytes.clone(),
                lash::attachments::AttachmentCreateMeta::new(
                    lash::attachments::MediaType::parse(&blob.media_type)?,
                    Some(lash::attachments::AttachmentTypeMetadata::image(
                        Some(1),
                        Some(1),
                    )),
                    Some(format!("{}.png", blob.blob_key.replace('/', "-"))),
                ),
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
        result.insert(
            "attachment".to_owned(),
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(reference)),
        );
        Ok(lash_core::ToolValue::Object(result))
    }
}

#[async_trait::async_trait]
impl StaticToolExecute for ReplayTools {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let outcome = match call.name() {
            "synthetic" => match self.synthetic(&call) {
                Ok(result) => ToolOutcome::ok(result),
                Err(error) => ToolOutcome::err_fmt(format_args!("{error:#}")),
            },
            "mark" => ToolOutcome::ok(self.mark(&call)),
            "attach" => match self.attach(&call).await {
                Ok(result) => {
                    ToolOutcome::from_output(lash_core::ToolCallOutput::success_tool_value(result))
                }
                Err(error) => ToolOutcome::err_fmt(format_args!("{error:#}")),
            },
            other => ToolOutcome::err_fmt(format_args!("no load tool `{other}`")),
        };
        outcome.into()
    }
}

/// Replays every turn of `actor/ordinal`'s session up to and including it,
/// and asserts each finishes its cell and every child start it planned
/// registered and ran.
async fn replay_session_through(actor: u64, ordinal: u64) {
    let workload = Arc::new(Workload::smoke_v1().expect("smoke-v1 parses"));
    let generator = Generator::new(&workload, RUN).expect("the replay run id is valid");
    let rotate_after = u64::from(workload.spec().rotate_after_turns);
    let first = ordinal - ordinal % rotate_after;

    let restate = lash_restate_test::backend(0x4255, lash_restate_test::ServerConfig::default())
        .await
        .expect("the Restate test double");
    let backend = restate.lash_backend();
    let served = ServedCell::default();
    let tools = ReplayTools {
        workload: Arc::clone(&workload),
        marks: Arc::default(),
    };
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build()
            .with_lashlang_abilities(lash::rlm::LashlangAbilities::default().with_sleep()),
        std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .serve_test_model(
            served.provider().into_handle(),
            lash::ModelMetadata::builder("e2e-mock")
                .context_window_tokens(200_000)
                .build()
                .expect("the load model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .tools(Arc::new(StaticToolProvider::new(
            ReplayTools::definitions(),
            tools.clone(),
        )))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "fig-4255",
            "fig-4255-replay",
        ))
        .expect("the replay core");
    restate.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process-worker config"),
        )
        .expect("the core's process worker"),
    );
    let session_id = lash::SessionId::from(format!("fig-4255-{actor}-{first}"));
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "e2e-mock",
            lash::TurnBudget::bounded(TURN_BUDGET),
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .expect("the replayed session is created");
    let session = core
        .session(session_id)
        .open()
        .await
        .expect("the replayed session opens");

    for turn in first..=ordinal {
        let plan = generator.plan(actor, turn).expect("the turn plan");
        let operation = plan.operation.key();
        served.serve(
            generator
                .provider_response(&plan, 1)
                .expect("the served cell")
                .text,
        );
        let output = session
            .send(TurnInput::text(format!(
                "Run the synthetic load turn. load_turn={operation}\n{}\n{}",
                generator
                    .record(actor, turn, "input", plan.input_bytes)
                    .expect("the turn input"),
                load_context(&generator, &plan)
            )))
            .output()
            .await;
        let output = output.unwrap_or_else(|error| {
            panic!(
                "{operation} (prompt {} bytes, {} child starts) must finish its cell: {error:?}",
                plan.prompt_bytes,
                plan.child_processes.len()
            )
        });
        assert_eq!(
            output.final_value(),
            Some(&json!({ "synthetic": true, "operation": operation })),
            "{operation} (prompt {} bytes, {} child starts) must finish its cell; \
             it stopped as {:?} after the feedback {:?}",
            plan.prompt_bytes,
            plan.child_processes.len(),
            output.status(),
            served.unrealized_feedback(),
        );
        for index in 0..plan.child_processes.len() {
            let key = format!("{operation}/child/{index}");
            assert!(
                tools.marks.lock_recover().contains(&key),
                "child start {key} must register and run its body"
            );
        }
    }
}

/// The turn's synthetic context, exactly as the kind worker sends it: after
/// the turn's input, since a run states no prompt (FIG-4589).
fn load_context(generator: &Generator<'_>, plan: &TurnPlan) -> String {
    let id = &plan.operation;
    format!(
        "## Synthetic load context\n\n{}",
        generator.text(id.actor, id.ordinal, "prompt", plan.prompt_bytes)
    )
}

fn run_on_stack(test: impl std::future::Future<Output = ()> + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("the test runtime")
                .block_on(test);
        })
        .expect("the test thread")
        .join()
        .expect("the replay finishes");
}

/// `2/9` failed every run of the kind smoke with `process_start_unrealized`:
/// its 128 KiB project instructions rode the declared start's execution
/// environment and pushed the attempt's intent batch over its byte budget.
#[test]
fn smoke_turn_2_9_child_start_registers_under_its_128_kib_prompt() {
    let workload = Workload::smoke_v1().expect("smoke-v1 parses");
    let plan = Generator::new(&workload, RUN)
        .and_then(|generator| generator.plan(2, 9))
        .expect("the turn plan");
    assert_eq!(plan.prompt_bytes, 131_072, "2/9 plans the largest prompt");
    assert_eq!(plan.child_processes.len(), 1, "2/9 plans one child start");
    run_on_stack(replay_session_through(2, 9));
}

/// `1/10` serves the same cell shape as `2/9` under a 2 KiB prompt and always
/// passed: the control that the prompt, not the cell, decides.
#[test]
fn smoke_turn_1_10_child_start_registers_under_its_2_kib_prompt() {
    let workload = Workload::smoke_v1().expect("smoke-v1 parses");
    let plan = Generator::new(&workload, RUN)
        .and_then(|generator| generator.plan(1, 10))
        .expect("the turn plan");
    assert_eq!(plan.prompt_bytes, 2_048, "1/10 plans the smallest prompt");
    assert_eq!(plan.child_processes.len(), 1, "1/10 plans one child start");
    run_on_stack(replay_session_through(1, 10));
}
