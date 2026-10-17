//! The model-answer receipt laws (FIG-6230): every durable turn-model call
//! commits the provider's outcome as its receipt, `model.answer`, and awaits
//! its acknowledgement before any stream hook is finished, any response
//! hook runs, or anything is admitted, launched or published from it; every
//! derivation, on the node that sent the call or on another, runs from the
//! receipt. A durably recorded response is never purchased again.
//!
//! A host creates a session on a core and sends it an input; the simulated
//! nodes A and B run its turn over the production store and the production
//! session activation. The model counts its sends across both nodes and
//! answers each send differently, so a call bought again shows. A plugin's
//! stream hook, stream-finished hook and response hook record what they are
//! handed, and can hold forever on their first run, so a node is killed
//! inside them.
//!
//! - **ACKNOWLEDGED-ANSWER-SURVIVES-HOOK-CRASH:** A is killed while its
//!   response hook runs, or loses its `turn.commit`, or loses the receipt's
//!   acknowledgement: the turn answers A's purchase, derived again from the
//!   receipt with byte-identical input, with one provider send. The live
//!   derivation is handed what the receipt reads back, never what only the
//!   live outcome held (FIG-6263): the provider's diagnostic is absent on
//!   both.
//! - **EMPTY-PLAN-HAS-THE-SAME-RECEIPT:** a call with no response hooks has
//!   a receipt too: A loses its `turn.commit`, and B, whose build installs a
//!   response hook, answers the raw receipt, never runs the new hook, sends
//!   nothing and charges the usage once.
//! - **RAW-RECEIPT-GATES-HOOKS:** while the receipt's acknowledgement is
//!   delayed, no response hook or stream finish runs and nothing commits
//!   after it. A node paused before the receipt enters the store leaves the
//!   call unanswered: the documented ambiguity, so the successor sends it
//!   again and derives nothing from the lost receipt.
//! - **CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER:** an RLM turn whose answer is
//!   a cell, killed while its response is derived, at the cell's first save
//!   before it enters the store, after it commits, and, for a cell that
//!   never saves, at the next `model.start`: one send per call, the paid
//!   cell, its `Once` write made at most once.
//! - **RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS:** an RLM turn killed while
//!   its stream finishes, after a complete and after a protocol-aborted
//!   stream, resumes on a node of another core, whose stream hooks never
//!   saw the stream: it derives the uncut run's response with no send.
//! - **STREAM-FINISH-IS-OWED-FOR-EVERY-REASON (FIG-6263):** a stream
//!   finish hook is owed and run for the stream's terminal reason: a
//!   complete stream, a finish-only plugin whose response plan is empty,
//!   and a stream the provider failed. A node that installs no stream
//!   hook owes no finish read and derives the receipt; a node that cannot
//!   finish a failed stream never claims it, and one that can finishes it
//!   with the provider's error.
//! - **CANCEL-AT-THE-RECEIPT-ENDS-AS-THE-UNCUT-RUN (FIG-6263):** an
//!   `Immediate` and an `AfterStep` cancel, for a hooked, a finish-only and
//!   a failed stream, cut at the receipt, inside its stream finish, inside
//!   its derivation and at the next phase, end as the uncut run: the
//!   status, the honoured step, the tool's writes, one send, one published
//!   model-call record and the stream's finish reason. An `AfterStep`
//!   cancel derives the paid step; an `Immediate` one stops before its
//!   tool. The feed half is in `tool_crash_laws.rs`
//!   (`a_native_call_cut_at_its_receipt_streams_its_activity_and_commits_one_outcome`).

// Test code.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::provider::{LlmTransportError, TransportRetryVerdict};
use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockEvent, StreamBlockIdentity,
    StreamBlockKind,
};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, LlmUsage, ToolCall, ToolOutcome};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel};
use lash_durable_test::{
    Fault, Matrix, Script, SimClock, SimNodes, SimNodesConfig, Stored, Tripwire,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::backend::BackendParts;
use lash_execution_contracts::backend::StoreSet;
use lash_execution_contracts::projection::NoProjectionProviders;
use lash_protocol_types::SessionId;
use lash_protocol_types::sync::MutexExt as _;
use serde_json::{Value, json};

const SESSION: &str = "model-answer-receipt";
const MODEL: &str = "model-answer-receipt-model";
const PLUGIN: &str = "receipt-law";
/// What the model answers each send of the turn's first call, in order.
const ANSWERS: [&str; 3] = [
    "the paid answer A",
    "the answer B a second purchase buys",
    "the answer C a third purchase buys",
];
/// What the response hook appends.
const HOOKED: &str = " [hooked]";
/// The usage the first call reports; the final scripted call omits usage.
const USAGE: LlmUsage = LlmUsage {
    input_tokens: 11,
    output_tokens: 7,
    cache_read_input_tokens: 0,
    cache_write_input_tokens: 0,
    reasoning_output_tokens: 0,
    unknown_calls: 0,
};
/// The host tool a turn's answer may call, declared `Once`.
const WRITE: &str = "write";
/// How far a law runs a node before it calls the turn stalled.
const HORIZON_MS: u64 = 600_000;

fn session() -> SessionId {
    SessionId::try_from(SESSION.to_owned()).unwrap()
}

fn actor() -> ActorKey {
    ActorKey::session(SESSION).unwrap()
}

/// Which hook holds forever on its first run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    /// The response hook.
    Derivation,
    /// The stream-finished hook, ending a stream for its recorded reason.
    Finish,
}

/// The outside world every node and core shares.
#[derive(Default)]
struct World {
    /// Provider sends of the turn's first call.
    sends: AtomicUsize,
    hold_second: std::sync::atomic::AtomicBool,
    stream_addresses: Mutex<Vec<lash::plugins::ModelCallAddress>>,
    response_addresses: Mutex<Vec<lash::plugins::ModelCallAddress>>,
    finished_addresses: Mutex<Vec<(Reason, lash::plugins::ModelCallAddress)>>,
    /// Every provider send.
    all_sends: AtomicUsize,
    /// What each response-hook run was handed: the response and the stream
    /// state, as JSON, and how many receipts were acknowledged by then.
    derived: Mutex<Vec<(Value, usize)>>,
    /// The reason of each stream finish that ended a stream, never a reset
    /// between streams, and how many receipts were acknowledged by then.
    finished: Mutex<Vec<(Reason, usize)>>,
    /// The hook that holds on its next run.
    hold: Mutex<Option<Hold>>,
    /// Whether a hook is held now.
    held: AtomicUsize,
    /// Receipt acknowledgements their owners received.
    acknowledged: AtomicUsize,
    hold_tail: AtomicUsize,
    /// Commits that entered the store after a receipt and before its
    /// acknowledgement reached its owner.
    early: Mutex<Vec<String>>,
    /// The arguments of every `write` body entry.
    writes: Mutex<Vec<Value>>,
    /// Notified at each `write`: an accepted cell's provider streams its
    /// tail once the cell ran.
    written: tokio::sync::Notify,
}

impl World {
    fn sends(&self) -> usize {
        self.sends.load(Ordering::SeqCst)
    }

    fn derived(&self) -> Vec<(Value, usize)> {
        self.derived.lock_recover().clone()
    }

    fn writes(&self) -> Vec<Value> {
        self.writes.lock_recover().clone()
    }

    /// The reason the last stream finish ended its stream for.
    fn last_finish(&self) -> Option<Reason> {
        self.finished
            .lock_recover()
            .last()
            .map(|(reason, _)| *reason)
    }

    fn held(&self) -> bool {
        self.held.load(Ordering::SeqCst) > 0
    }

    /// Hold forever if `hook` is the one to hold, once.
    async fn hold_if(&self, hook: Hold) {
        let holds = {
            let mut hold = self.hold.lock_recover();
            (*hold == Some(hook)).then(|| hold.take()).is_some()
        };
        if holds {
            self.held.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    }
}

/// What the model answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Text, streamed in three deltas: [`ANSWERS`] by send.
    Text,
    RetriedText,
    MixedCalls,
    /// A call to [`WRITE`] beside text, then `done`.
    Tool,
    /// A TypeScript cell that calls [`WRITE`] with its send's marker, then
    /// `done`.
    Cell,
    /// A TypeScript cell that only prints, then `done`.
    PrintingCell,
    /// The [`Answer::Cell`] cell, accepted by the cell mask at its close
    /// tag while the provider still generates: the provider streams its
    /// trailing text and reports [`USAGE`] only after the cell ran.
    AbortedCell,
    /// Two streamed deltas, then a provider failure no retry follows.
    Failed,
    /// Two calls; the second retries once before answering.
    RetryTool,
}

impl Answer {
    fn code(self) -> bool {
        matches!(
            self,
            Self::Cell | Self::PrintingCell | Self::AbortedCell | Self::MixedCalls
        )
    }
}

/// The model over `world`.
fn model(world: Arc<World>, answer: Answer) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("model-answer-receipt")
        .requires_streaming(true)
        .options(lash_core::provider::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(Some(2))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..Default::default()
        })
        .generation_retry_guarantee(
            if matches!(
                answer,
                Answer::RetryTool | Answer::RetriedText | Answer::MixedCalls
            ) {
                lash_core::provider::GenerationRetryGuarantee::Idempotent
            } else {
                lash_core::provider::GenerationRetryGuarantee::None
            },
        )
        .complete(move |request: LlmRequest| {
            let world = Arc::clone(&world);
            async move {
                let send_index = world.all_sends.fetch_add(1, Ordering::SeqCst);
                if answer == Answer::MixedCalls {
                    return mixed_call(&request, send_index).await;
                }
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                if answered {
                    if matches!(answer, Answer::RetryTool) && send_index == 1 {
                        stream(&request, &["retrying"]);
                        return Err(LlmTransportError::new("retry")
                            .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
                    }
                    return Ok(streamed(&request, &["do", "ne"]));
                }
                let send = world.sends.fetch_add(1, Ordering::SeqCst);
                if answer == Answer::RetriedText && request.scope.attempt == Some(1) {
                    return Err(LlmTransportError::new("retry before response")
                        .with_kind(lash_core::ProviderFailureKind::Transport)
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
                }
                let marker = ANSWERS[send.min(ANSWERS.len() - 1)];
                let mut response = match answer {
                    Answer::Failed => {
                        stream(&request, &["the ", "paid a"]);
                        return Err(
                            LlmTransportError::new("the provider failed after streaming")
                                .with_retry_verdict(TransportRetryVerdict::NotRetryable),
                        );
                    }
                    Answer::MixedCalls => unreachable!("the mixed history is handled above"),
                    Answer::Text | Answer::RetriedText => {
                        let (head, tail) = marker.split_at(4);
                        let (middle, tail) = tail.split_at(6);
                        streamed(&request, &[head, middle, tail])
                    }
                    Answer::Tool | Answer::RetryTool => {
                        let mut response = streamed(&request, &["writing ", marker]);
                        response.parts.push(LlmOutputPart::ToolCall {
                            call_id: "call-write".to_owned(),
                            tool_name: WRITE.to_owned(),
                            input_json: json!({ "marker": marker }).to_string(),
                            replay: None,
                        });
                        response
                    }
                    Answer::Cell => streamed(&request, &cell(marker)),
                    Answer::PrintingCell => streamed(
                        &request,
                        &[
                            "Printing.\n<typescript>\n",
                            &format!("console.log({marker:?});\n"),
                            "</typescript>",
                        ],
                    ),
                    Answer::AbortedCell => {
                        let mut pieces = cell(marker);
                        stream(&request, &pieces);
                        // The mask accepts the cell at its close tag and the
                        // cell runs from what streamed; the provider's tail
                        // drains off the turn's path (FIG-6433).
                        world.written.notified().await;
                        if world.hold_tail.load(Ordering::SeqCst) > 0 {
                            std::future::pending::<()>().await;
                        }
                        let tail = "\nTrailing prose the mask cuts.".to_owned();
                        stream(&request, std::slice::from_ref(&tail));
                        pieces.push(tail);
                        LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: pieces.concat(),
                                response_meta: None,
                            }],
                            ..LlmResponse::default()
                        }
                    }
                };
                response.usage = USAGE;
                response.terminal_reason = lash_core::LlmTerminalReason::Stop;
                // Provider detail a durable record never keeps (FIG-4087):
                // a derivation is handed the receipt's response, never it.
                response.terminal_diagnostic = Some("provider detail".to_owned());
                Ok(response)
            }
        })
        .build()
        .into_handle()
}

/// The pieces of a cell calling [`WRITE`] with `marker`.
fn cell(marker: &str) -> Vec<String> {
    vec![
        "Running a cell.\n<typescript>\n".to_owned(),
        format!("await tools.write({{ marker: {marker:?} }});\n"),
        format!("console.log({marker:?});\n"),
        "</typescript>".to_owned(),
    ]
}

/// Stream `pieces` as deltas of one text block.
fn stream(request: &LlmRequest, pieces: &[impl AsRef<str>]) {
    if let Some(stream) = request.stream_events.as_ref() {
        for piece in pieces {
            stream.send(LlmStreamEvent::Block(StreamBlockEvent::Delta {
                kind: StreamBlockKind::AssistantText,
                block: StreamBlockIdentity::new("text:0", 0),
                text: piece.as_ref().to_owned(),
            }));
        }
    }
}

/// `pieces`, streamed and answered as one text part.
fn streamed(request: &LlmRequest, pieces: &[impl AsRef<str>]) -> LlmResponse {
    stream(request, pieces);
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: pieces.iter().map(AsRef::as_ref).collect(),
            response_meta: None,
        }],
        ..LlmResponse::default()
    }
}

/// Which of the law plugin's hooks a core installs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hooks {
    /// A stream hook alone: the call's response plan is empty.
    StreamOnly,
    /// A stream hook, a stream-finished hook and a response hook that
    /// derives with its state.
    All,
    /// A stream hook and a stream-finished hook: the response plan is
    /// empty, and only the stream's finish derives from the receipt.
    FinishOnly,
    /// A stream hook and a response hook that reads no stream state.
    StreamAndResponse,
    /// A response hook that reads no stream state, alone.
    ResponseOnly,
}

impl Hooks {
    fn stream(self) -> bool {
        self != Self::ResponseOnly
    }

    fn finishes(self) -> bool {
        matches!(self, Self::All | Self::FinishOnly)
    }

    fn response(self) -> bool {
        matches!(
            self,
            Self::All | Self::StreamAndResponse | Self::ResponseOnly
        )
    }
}

type Reason = lash::plugins::AssistantStreamFinishReason;

/// The law plugin over `world`. Its stream hook keeps the chunks it was
/// handed in this core's memory: the state a node's stream hooks hold.
fn plugin(world: &Arc<World>, hooks: Hooks) -> Arc<dyn lash::plugins::PluginFactory> {
    let chunks = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&chunks);
    let streaming = Arc::clone(world);
    let mut spec = lash::plugins::PluginSpec::new();
    if hooks.stream() {
        spec = spec.with_assistant_stream(
            lash::hook_key!("chunks"),
            Arc::new(move |ctx| {
                streaming
                    .stream_addresses
                    .lock_recover()
                    .push(ctx.address.clone());
                seen.lock_recover().push(ctx.chunk.clone());
                Box::pin(async move {
                    Ok(lash::plugins::AssistantStreamTransform {
                        chunk: ctx.chunk,
                        ..Default::default()
                    })
                })
            }),
        );
    }
    if hooks.finishes() {
        let finishing = Arc::clone(world);
        spec = spec.with_assistant_stream_finished(
            lash::hook_key!("finish"),
            Arc::new(move |ctx| {
                let world = Arc::clone(&finishing);
                let held = std::mem::take(&mut *chunks.lock_recover());
                Box::pin(async move {
                    world
                        .finished_addresses
                        .lock_recover()
                        .push((ctx.reason, ctx.address));
                    if ctx.reason == Reason::AttemptReset {
                        return Ok(None);
                    }
                    world
                        .finished
                        .lock_recover()
                        .push((ctx.reason, world.acknowledged.load(Ordering::SeqCst)));
                    world.hold_if(Hold::Finish).await;
                    if !matches!(ctx.reason, Reason::Complete | Reason::Aborted) {
                        return Ok(None);
                    }
                    Ok(Some(json!(held)))
                })
            }),
        );
    }
    if hooks.response() {
        let deriving = Arc::clone(world);
        spec = spec.with_assistant_response(
            lash::hook_key!("suffix"),
            hooks.finishes().then(|| lash::hook_key!("finish")),
            Arc::new(move |ctx| {
                let world = Arc::clone(&deriving);
                Box::pin(async move {
                    world
                        .response_addresses
                        .lock_recover()
                        .push(ctx.address.clone());
                    let handed = json!({
                        "response": serde_json::to_value(&ctx.response).unwrap(),
                        "diagnostic": ctx.response.terminal_diagnostic,
                        "stream_state": ctx.stream_state,
                    });
                    world
                        .derived
                        .lock_recover()
                        .push((handed, world.acknowledged.load(Ordering::SeqCst)));
                    if !world.hold_second.load(Ordering::SeqCst)
                        || !ctx.response.full_text().starts_with("writing ")
                    {
                        world.hold_if(Hold::Derivation).await;
                    }
                    // A cell's text is the protocol's to parse: only prose
                    // is marked.
                    let mut response = ctx.response;
                    if let Some(LlmOutputPart::Text { text, .. }) = response
                        .parts
                        .iter_mut()
                        .rev()
                        .find(|part| matches!(part, LlmOutputPart::Text { .. }))
                        && !text.contains("<typescript>")
                    {
                        text.push_str(HOOKED);
                    }
                    Ok(lash::plugins::AssistantResponseTransform {
                        response,
                        events: Vec::new(),
                    })
                })
            }),
        );
    }
    Arc::new(lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(PLUGIN),
        spec,
    ))
}

/// The host tool [`WRITE`]: each body entry reaches the world.
struct Writes {
    world: Arc<World>,
}

#[async_trait::async_trait]
impl StaticToolExecute for Writes {
    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.world.writes.lock_recover().push(call.args.clone());
        self.world.written.notify_one();
        ToolOutcome::ok(json!({ "wrote": call.args })).into()
    }
}

fn write_tool() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        WRITE,
        WRITE,
        "A host write the receipt laws count.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": { "marker": { "type": "string" } }
        }),
        json!({ "type": "object" }),
    )
    .expect("the tool's schemas")
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(ExecutionPolicy::Once)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], WRITE))
}

/// One core over `backend`: the protocol `answer` needs, the model over
/// `world` and the law plugin with `hooks`.
#[allow(
    clippy::too_many_arguments,
    reason = "a law's core and its shared world"
)]
fn core(
    backend: &Backend,
    clock: &Arc<SimClock>,
    world: &Arc<World>,
    live_replay: &Arc<lash::observe::InMemoryLiveReplayStore>,
    answer: Answer,
    hooks: Hooks,
    name: &str,
) -> lash::LashCore {
    let builder = if answer.code() {
        let factory = lash::rlm::RlmProtocolPluginFactory::new(
            lash::rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                .build(),
            lash::rlm::CellDialect::typescript(),
        )
        .with_worker_service(sim::workers(clock));
        lash::LashCore::rlm_builder(backend.clone(), factory)
    } else {
        lash::LashCore::standard_builder(backend.clone())
    };
    builder
        .plugin(plugin(world, hooks))
        .live_replay_store(Arc::clone(live_replay) as _)
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(if answer == Answer::MixedCalls {
            let total = Duration::from_secs(10);
            lash::ExecutionBudgets::new(lash_core::ExecutionBudgetsConfig {
                model_total: total,
                stop_grace: Duration::from_millis(50),
                provider: lash_core::ProviderAttemptLimits::new(total, total, total, 2).unwrap(),
                ..lash_core::ExecutionBudgetsConfig::recommended()
            })
            .unwrap()
        } else {
            lash::ExecutionBudgets::recommended()
        })
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(
            model(Arc::clone(world), answer),
            lash_core::LlmProfileMetadata::builder(MODEL)
                .cache_retention(lash_core::provider::CacheRetention::Short)
                .context_window_tokens(200_000)
                .build()
                .expect("the model's metadata"),
        )
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::new(StaticToolProvider::new(
                vec![write_tool()],
                Writes {
                    world: Arc::clone(world),
                },
            )),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(name),
            lash::persistence::LeaseIncarnationId::new(format!("{name}-boot")),
        ))
        .expect("the core builds")
}

fn activation(backend: &Backend, core: &lash::LashCore) -> Arc<dyn Activation> {
    Arc::new(SessionActivation::new(
        backend.clone(),
        lash::testing::session_turn_services(core),
        Arc::new(Tripwire::new()) as _,
    ))
}

/// How a law's deployment is built.
struct Setup {
    answer: Answer,
    /// The hooks of the core every node runs, and of the core node B runs
    /// when it is not that one.
    hooks: Hooks,
    b_hooks: Option<Hooks>,
    postgres: bool,
}

impl Setup {
    fn new(answer: Answer, hooks: Hooks) -> Self {
        Self {
            answer,
            hooks,
            b_hooks: None,
            postgres: false,
        }
    }

    /// Node B runs a core of its own, with `hooks`: another build, whose
    /// hooks never saw what A streamed.
    fn b_on_its_own_core(mut self, hooks: Hooks) -> Self {
        self.b_hooks = Some(hooks);
        self
    }
}

/// One session served by sim nodes over SQLite in memory.
struct Deployment {
    backend: Backend,
    clock: Arc<SimClock>,
    nodes: SimNodes,
    world: Arc<World>,
    /// What every node published to the session's live feed.
    live_replay: Arc<lash::observe::InMemoryLiveReplayStore>,
    core: lash::LashCore,
    _b_core: Option<lash::LashCore>,
    durable: lash::DurableSession,
    _database: Option<lash_postgres_store::testing::IsolatedDatabase>,
}

impl Deployment {
    /// The deployment of `setup`, its fault rules armed by `arm`.
    async fn new(setup: Setup, arm: impl FnOnce(&Script)) -> Self {
        let clock = SimClock::new();
        let (stores, isolated): (Arc<dyn StoreSet>, _) = if setup.postgres {
            sim::wait_out_renders(&clock);
            let url = lash_postgres_store::testing::required_database_url();
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .expect("PostgreSQL store");
            let stores = lash_postgres_store::PostgresStoreSet::with_clock_for_testing(
                &storage,
                Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
                clock.clone(),
            );
            lash_execution_contracts::testing::process_execution_env_fixture(
                stores.process_env_store().as_ref(),
            )
            .await;
            (Arc::new(stores), Some(isolated))
        } else {
            (Arc::new(sim::memory(Arc::clone(&clock)).await), None)
        };
        let database = stores.durable_store();
        let backend = Backend::assemble(BackendParts {
            stores,
            settings: sim::settings(),
            engines: Vec::new(),
            providers: Arc::new(NoProjectionProviders),
            formats: lash::formats::actor_state_surfaces(),
        })
        .expect("the backend assembles");
        let world = Arc::new(World::default());
        let live_replay = Arc::new(lash::observe::InMemoryLiveReplayStore::new(
            lash::observe::InMemoryLiveReplayStoreConfig::standard(),
        ));
        let core = core(
            &backend,
            &clock,
            &world,
            &live_replay,
            setup.answer,
            setup.hooks,
            "receipt-a",
        );
        let script = Script::new();
        let acknowledged = Arc::clone(&world);
        script.observe_actor_replies(move |label, _, reply| {
            if label == CommitLabel::MODEL_ANSWER && reply.is_ok() {
                acknowledged.acknowledged.fetch_add(1, Ordering::SeqCst);
            }
        });
        let early = Arc::clone(&world);
        let receipts = Arc::new(AtomicUsize::new(0));
        script.observe_actor_receipts(move |label, _, _| {
            if label == CommitLabel::MODEL_ANSWER {
                receipts.fetch_add(1, Ordering::SeqCst);
            } else if receipts.load(Ordering::SeqCst) > early.acknowledged.load(Ordering::SeqCst) {
                early.early.lock_recover().push(label.as_str().to_owned());
            }
        });
        arm(&script);
        let nodes = SimNodes::new(
            database,
            Arc::clone(&clock),
            script,
            SimNodesConfig {
                lease: Matrix::test_lease(),
                decodes: backend.formats().decodes(),
                serves: lash::testing::node_serves(&core),
                max_active: 4,
            },
            activation(&backend, &core),
        );
        let b_core = setup.b_hooks.map(|hooks| {
            let b_core = self::core(
                &backend,
                &clock,
                &world,
                &live_replay,
                setup.answer,
                hooks,
                "receipt-b",
            );
            nodes.activate_on("b", activation(&backend, &b_core));
            nodes.serve_on("b", lash::testing::node_serves(&b_core));
            b_core
        });
        let durable = core
            .session(session())
            .create(lash::SessionCreation::root(
                lash::SessionSpec::new(
                    MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(8),
                )
                .no_progress_budget(lash_core::NoProgressBudget::bounded(12)),
            ))
            .await
            .expect("the session is created");
        Self {
            backend,
            clock,
            nodes,
            world,
            live_replay,
            core,
            _b_core: b_core,
            _database: isolated,
            durable,
        }
    }

    /// Send the turn's input and start node A.
    async fn send(&self) -> lash::SendHandle {
        let handle = self
            .durable
            .send(lash::TurnInput::text("answer the law"))
            .await
            .expect("the turn's input is sent");
        self.nodes.start("a");
        handle
    }

    /// Step the deployment until `done` holds.
    async fn until(&self, what: &str, done: impl Fn(&Self) -> bool) {
        let horizon = self.clock.logical_ms() + HORIZON_MS;
        self.nodes.quiesce().await;
        while !done(self) {
            let stepped = self.nodes.step().await;
            if self.clock.logical_ms() >= horizon || (stepped.is_none() && !done(self)) {
                panic!(
                    "{what}: the deployment stalled:\n{}",
                    self.nodes.script().rendered_trace()
                );
            }
        }
    }

    /// Kill A and start B.
    async fn take_over(&self) {
        self.nodes.kill("a");
        self.nodes.quiesce().await;
        self.nodes.start("b");
    }

    /// Step until the session is idle: its turn ended.
    async fn settle(&self) {
        let horizon = self.clock.logical_ms() + HORIZON_MS;
        loop {
            self.nodes.quiesce().await;
            if matches!(
                self.nodes.database().actor(&actor()).await,
                Ok(Some(snapshot)) if snapshot.state == ActorState::Idle
            ) {
                break;
            }
            assert!(
                self.clock.logical_ms() < horizon && self.nodes.step().await.is_some(),
                "the turn did not end:\n{}",
                self.nodes.script().rendered_trace()
            );
        }
        self.nodes.quiesce().await;
    }

    /// The committed transcript: role and text per message.
    async fn transcript(&self) -> Vec<(String, String)> {
        self.durable
            .read()
            .await
            .expect("the session reads")
            .expect("the committed head")
            .messages()
            .iter()
            .map(|message| {
                let text = message
                    .parts
                    .iter()
                    .filter(|part| {
                        matches!(
                            part.kind(),
                            lash_core::PartKind::Text | lash_core::PartKind::Prose
                        )
                    })
                    .map(|part| part.content().into_owned())
                    .collect::<String>();
                (format!("{:?}", message.role), text)
            })
            .collect()
    }

    /// The committed head's token usage.
    async fn usage(&self) -> LlmUsage {
        self.durable
            .read()
            .await
            .expect("the session reads")
            .expect("the committed head")
            .token_usage()
            .clone()
    }

    /// The turn activity the session's live feed published after `cursor`:
    /// what any host following it is handed, once each.
    async fn published(&self, cursor: &lash_core::SessionCursor) -> Vec<lash_core::TurnEvent> {
        use lash::observe::{LiveReplayStore as _, SessionObservationEventPayload};
        let Ok(lash_core::LiveReplayOutcome::Replayed(events)) =
            self.live_replay.replay_after_cursor(cursor).await
        else {
            panic!("the session's live feed holds its turn");
        };
        events
            .iter()
            .filter_map(|event| match &event.payload {
                SessionObservationEventPayload::TurnActivity(activity) => {
                    Some(activity.event.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// The sealed model-call records the session's live feed published
    /// after `cursor`.
    async fn published_calls(&self, cursor: &lash_core::SessionCursor) -> usize {
        self.published(cursor)
            .await
            .iter()
            .filter(|event| matches!(event, lash_core::TurnEvent::ModelCallRecorded { .. }))
            .count()
    }

    /// The writes of `label` that committed.
    fn committed(&self, label: CommitLabel) -> usize {
        self.nodes
            .script()
            .trace()
            .iter()
            .filter(|write| write.point.label == label && write.committed())
            .count()
    }

    /// Whether a rule cut a write of `label`.
    fn cut(&self, label: CommitLabel) -> bool {
        self.nodes
            .script()
            .cuts()
            .iter()
            .any(|cut| cut.point.label == label)
    }

    fn end(self) {
        self.nodes.kill("a");
        self.nodes.kill("b");
        drop(self.durable);
        drop(self.core);
    }
}

/// The last assistant message of `transcript`.
fn answered(transcript: &[(String, String)]) -> String {
    transcript
        .iter()
        .rev()
        .find(|(role, _)| role == "Assistant")
        .map(|(_, text)| text.clone())
        .unwrap_or_default()
}

/// Every derivation was handed byte-identical input, there were `runs` of
/// them, and each ran after its receipt's acknowledgement.
fn assert_derived_alike(world: &World, runs: usize) {
    let derived = world.derived();
    assert_eq!(derived.len(), runs, "response-hook runs: {derived:#?}");
    for (handed, acknowledged) in &derived {
        assert_eq!(
            handed, &derived[0].0,
            "every derivation is handed the receipt's response and stream state"
        );
        assert!(
            *acknowledged >= 1,
            "a derivation ran before its receipt was acknowledged: {derived:#?}"
        );
    }
    assert_eq!(
        derived[0].0["stream_state"],
        json!(["the ", "paid a", "nswer A"]),
        "the stream state is the stream hooks' finish over the chunks A streamed"
    );
    assert_eq!(
        derived[0].0["diagnostic"],
        Value::Null,
        "a derivation is handed the receipt's response, never what only the live answer held"
    );
}

/// ACKNOWLEDGED-ANSWER-SURVIVES-HOOK-CRASH: A is killed inside the response
/// hook, after the receipt was acknowledged. B derives again from the
/// receipt: one send, the hook handed byte-identical input twice, and the
/// turn answers A's purchase.
#[tokio::test]
async fn an_answer_whose_hook_was_killed_is_derived_again_from_its_receipt() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |_| {}).await;
    *deployment.world.hold.lock_recover() = Some(Hold::Derivation);
    let _handle = deployment.send().await;
    deployment
        .until("A's response hook holds", |deployment| {
            deployment.world.held()
        })
        .await;
    assert_eq!(
        deployment.committed(CommitLabel::MODEL_ANSWER),
        1,
        "the receipt committed before the hook ran"
    );
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    assert_derived_alike(&deployment.world, 2);
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("{}{HOOKED}", ANSWERS[0])
    );
    deployment.end();
}

/// ACKNOWLEDGED-ANSWER-SURVIVES-HOOK-CRASH: the derivation answered on A,
/// which dies before its `turn.commit` enters the store. B derives again
/// from the receipt.
#[tokio::test]
async fn an_answer_derived_before_a_lost_turn_commit_is_derived_again_from_its_receipt() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |script| {
        script.cut_on("a", CommitLabel::TURN_COMMIT, 1, Fault::Abort);
    })
    .await;
    let _handle = deployment.send().await;
    deployment
        .until("A dies at its turn commit", |deployment| {
            deployment.cut(CommitLabel::TURN_COMMIT)
        })
        .await;
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    assert_derived_alike(&deployment.world, 2);
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("{}{HOOKED}", ANSWERS[0])
    );
    deployment.end();
}

/// ACKNOWLEDGED-ANSWER-SURVIVES-HOOK-CRASH: the receipt commits and its
/// acknowledgement is lost. Nothing derives from the unacknowledged receipt
/// on that attempt; the next reconciles from the store, finds the receipt
/// and derives from it.
#[tokio::test]
async fn a_receipt_whose_acknowledgement_was_lost_is_reconciled_from_the_store() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |script| {
        script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::AckHidden);
    })
    .await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    assert!(
        deployment.cut(CommitLabel::MODEL_ANSWER),
        "the receipt's acknowledgement was lost:\n{}",
        deployment.nodes.script().rendered_trace()
    );
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    assert_eq!(deployment.committed(CommitLabel::MODEL_ANSWER), 1);
    let derived = deployment.world.derived();
    assert_eq!(derived.len(), 1, "only the reconciled receipt is derived");
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("{}{HOOKED}", ANSWERS[0])
    );
    deployment.end();
}

/// EMPTY-PLAN-HAS-THE-SAME-RECEIPT: A's build installs no response hook,
/// and its call's receipt commits like any other. A dies before its
/// `turn.commit` enters the store; B's build installs a response hook. B
/// answers the raw receipt: no send, the new hook never runs, the usage is
/// charged once and one receipt committed.
#[tokio::test]
async fn an_answer_with_no_response_hooks_has_the_same_receipt() {
    let deployment = Deployment::new(
        Setup::new(Answer::Text, Hooks::StreamOnly).b_on_its_own_core(Hooks::All),
        |script| {
            script.cut_on("a", CommitLabel::TURN_COMMIT, 1, Fault::Abort);
        },
    )
    .await;
    let _handle = deployment.send().await;
    deployment
        .until("A dies at its turn commit", |deployment| {
            deployment.cut(CommitLabel::TURN_COMMIT)
        })
        .await;
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    assert_eq!(deployment.committed(CommitLabel::MODEL_ANSWER), 1);
    assert!(
        deployment.world.derived().is_empty(),
        "a hook B installed derives nothing of a call admitted without it"
    );
    assert_eq!(answered(&deployment.transcript().await), ANSWERS[0]);
    assert_eq!(
        deployment.usage().await,
        USAGE,
        "the paid answer is charged once"
    );
    deployment.end();
}

/// RAW-RECEIPT-GATES-HOOKS: the receipt commits and its acknowledgement
/// reaches A late. No stream finish, no response hook and no later commit
/// happen before it.
#[tokio::test]
async fn nothing_derives_from_a_receipt_before_its_acknowledgement() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |script| {
        script.cut_on(
            "a",
            CommitLabel::MODEL_ANSWER,
            1,
            Fault::DelayedAck(Duration::from_secs(30)),
        );
    })
    .await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    assert!(
        deployment.cut(CommitLabel::MODEL_ANSWER),
        "the receipt's acknowledgement was delayed:\n{}",
        deployment.nodes.script().rendered_trace()
    );
    let finished = deployment.world.finished.lock_recover().clone();
    assert!(
        !finished.is_empty() && finished.iter().all(|(_, acknowledged)| *acknowledged >= 1),
        "a stream finish ran before the receipt's acknowledgement: {finished:?}"
    );
    assert_derived_alike(&deployment.world, 1);
    let early = deployment.world.early.lock_recover().clone();
    assert!(
        early.is_empty(),
        "commits entered the store before the receipt's acknowledgement: {early:?}"
    );
    assert_eq!(deployment.world.sends(), 1);
    deployment.end();
}

/// RAW-RECEIPT-GATES-HOOKS, the negative boundary: A pauses before its
/// receipt enters the store and is reaped. The call is unanswered, as
/// documented: B sends it again, and nothing derives from the receipt A
/// never committed, which its zombie write cannot commit either.
#[tokio::test]
async fn a_receipt_lost_before_its_commit_leaves_the_call_unanswered() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |script| {
        script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::Zombie);
    })
    .await;
    let _handle = deployment.send().await;
    deployment
        .until("A pauses before its receipt", |deployment| {
            deployment.nodes.paused().contains(&"a".to_owned())
        })
        .await;
    assert!(
        deployment.world.derived().is_empty()
            && deployment.world.finished.lock_recover().is_empty(),
        "nothing derives from a receipt that never entered the store"
    );
    deployment.nodes.start("b");
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        2,
        "a call whose receipt never committed is unanswered, and sent again"
    );
    assert_eq!(
        deployment.committed(CommitLabel::MODEL_ANSWER),
        1,
        "only the successor's receipt committed"
    );
    let derived = deployment.world.derived();
    assert_eq!(
        derived.len(),
        1,
        "only B's purchase is derived: {derived:#?}"
    );
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("{}{HOOKED}", ANSWERS[1])
    );
    deployment.end();
}

/// The writes a code law's cell made: the markers it wrote.
fn markers(world: &World) -> Vec<String> {
    world
        .writes()
        .iter()
        .map(|args| args["marker"].as_str().unwrap_or_default().to_owned())
        .collect()
}

/// CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER: `cut` kills A in an RLM turn
/// whose answer is A's cell, and B resumes it: one send per call, and the
/// turn answers.
async fn a_cell_resumes_from_its_paid_answer(
    answer: Answer,
    hold: Option<Hold>,
    cut: Option<(CommitLabel, usize, Fault)>,
) -> Deployment {
    let deployment = Deployment::new(Setup::new(answer, Hooks::All), |script| {
        if let Some((label, nth, fault)) = cut {
            script.cut_on("a", label, nth, fault);
        }
    })
    .await;
    *deployment.world.hold.lock_recover() = hold;
    let _handle = deployment.send().await;
    deployment
        .until("A is cut", |deployment| {
            deployment.world.held() || cut.is_some_and(|(label, _, _)| deployment.cut(label))
        })
        .await;
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "the cell's paid answer is bought once"
    );
    assert_eq!(
        deployment.world.all_sends.load(Ordering::SeqCst),
        2,
        "the cell's call and the call after it, each sent once"
    );
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("done{HOOKED}")
    );
    deployment
}

/// CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER: killed inside the response
/// derivation, before the cell launched.
#[tokio::test]
async fn a_cell_killed_in_its_derivation_runs_the_paid_cell() {
    let deployment =
        a_cell_resumes_from_its_paid_answer(Answer::Cell, Some(Hold::Derivation), None).await;
    assert_eq!(markers(&deployment.world), [ANSWERS[0]]);
    let derived = deployment.world.derived();
    assert_eq!(
        derived[0].0, derived[1].0,
        "the cell's derivation is handed the receipt's response twice"
    );
    deployment.end();
}

/// CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER: the cell launched and its first
/// save, which carries `model.done`, was lost before it entered the store.
#[tokio::test]
async fn a_cell_killed_before_its_first_save_runs_the_paid_cell_again() {
    let deployment = a_cell_resumes_from_its_paid_answer(
        Answer::Cell,
        None,
        Some((CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT, 1, Fault::Abort)),
    )
    .await;
    assert_eq!(markers(&deployment.world), [ANSWERS[0]]);
    let derived = deployment.world.derived();
    assert_eq!(
        derived[0].0, derived[1].0,
        "the cell is derived again from the same receipt"
    );
    deployment.end();
}

/// CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER: the cell's first save committed
/// with `model.done`: B restores the cell's progress and its admission, and
/// neither derives the answer again nor sends it.
#[tokio::test]
async fn a_cell_killed_after_its_first_save_resumes_its_progress() {
    let deployment = a_cell_resumes_from_its_paid_answer(
        Answer::Cell,
        None,
        Some((
            CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT,
            1,
            Fault::CommitThenAbort,
        )),
    )
    .await;
    // The save admitted the `Once` write: its body ran at most once, with
    // A's marker, or was interrupted with its owner.
    let markers = markers(&deployment.world);
    assert!(
        markers.iter().all(|marker| marker == ANSWERS[0]) && markers.len() <= 1,
        "A's cell's write is made at most once: {markers:?}"
    );
    let cell_derivations = deployment
        .world
        .derived()
        .iter()
        .filter(|(handed, _)| handed.to_string().contains("typescript"))
        .count();
    assert_eq!(cell_derivations, 1, "the cell's answer is derived once");
    deployment.end();
}

/// CODE-CELL-REPLAYS-FROM-THE-PAID-ANSWER: a cell that never saves leaves
/// its answer to the next `model.start`, which A loses before it enters the
/// store. B derives the cell again from its receipt and reruns it.
#[tokio::test]
async fn a_cell_that_never_saves_is_derived_again_from_its_receipt() {
    let deployment = a_cell_resumes_from_its_paid_answer(
        Answer::PrintingCell,
        None,
        Some((CommitLabel::MODEL_START, 2, Fault::Abort)),
    )
    .await;
    assert!(markers(&deployment.world).is_empty());
    deployment.end();
}

/// The uncut run of `answer`'s turn, for a law to compare a resumed run
/// with: its derivations and its committed usage.
async fn uncut_run(answer: Answer) -> (Vec<Value>, LlmUsage) {
    let deployment = Deployment::new(Setup::new(answer, Hooks::All), |_| {}).await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    let derived = deployment
        .world
        .derived()
        .into_iter()
        .map(|(handed, _)| handed)
        .collect();
    let usage = deployment.usage().await;
    deployment.end();
    (derived, usage)
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS: A holds in its stream finish,
/// after the receipt committed, and is killed. B runs on a core of its own,
/// whose stream hooks never saw A's stream: it finishes the stream from the
/// receipt's capsule and derives what the uncut run derives, with no send.
/// The uncut run charges the paid answer's usage. B charges what the receipt
/// holds: that usage for a call that completed before its receipt, and for
/// a cell accepted mid-stream, whose tail drained on A alone, the call
/// counted unknown, never zero (FIG-6433).
async fn a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(answer: Answer) {
    let (uncut, uncut_usage) = uncut_run(answer).await;
    let final_omission = LlmUsage::unknown_call();
    assert_eq!(
        uncut_usage,
        USAGE.checked_add(&final_omission).unwrap(),
        "the uncut run charges the paid answer and retains its final call's absent usage"
    );
    let deployment = Deployment::new(
        Setup::new(answer, Hooks::All).b_on_its_own_core(Hooks::All),
        |_| {},
    )
    .await;
    *deployment.world.hold.lock_recover() = Some(Hold::Finish);
    let _handle = deployment.send().await;
    deployment
        .until("A's stream finish holds", |deployment| {
            deployment.world.held()
        })
        .await;
    assert_eq!(deployment.committed(CommitLabel::MODEL_ANSWER), 1);
    assert!(
        deployment.world.derived().is_empty(),
        "nothing derived before the stream finished"
    );
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "B sends nothing of A's purchase"
    );
    let derived = deployment
        .world
        .derived()
        .into_iter()
        .map(|(handed, _)| handed)
        .collect::<Vec<_>>();
    assert_eq!(
        derived, uncut,
        "B derives from the receipt what the uncut run derives"
    );
    assert_eq!(markers(&deployment.world), [ANSWERS[0]]);
    let resumed_usage = match answer {
        Answer::AbortedCell => LlmUsage::unknown_call(),
        _ => USAGE,
    };
    assert_eq!(
        deployment.usage().await,
        resumed_usage.checked_add(&final_omission).unwrap()
    );
    deployment.end();
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS for a stream that completed.
#[tokio::test]
async fn a_completed_streams_finish_resumes_from_the_receipt_on_another_core() {
    a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(Answer::Cell).await;
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS for a stream whose cell the
/// cell mask accepted at its close tag, whose response the runtime derived
/// from what streamed while the provider's tail drained.
#[tokio::test]
async fn a_protocol_aborted_streams_finish_resumes_from_the_receipt_on_another_core() {
    a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(Answer::AbortedCell).await;
}

/// Step the deployment for `millis` of virtual time, or until nothing is
/// left to step.
async fn run_for(deployment: &Deployment, millis: u64) {
    let until = deployment.clock.logical_ms() + millis;
    deployment.nodes.quiesce().await;
    while deployment.clock.logical_ms() < until && deployment.nodes.step().await.is_some() {}
    deployment.nodes.quiesce().await;
}

/// Whether `node` claimed the session.
fn claimed_by(deployment: &Deployment, node: &str) -> bool {
    deployment.nodes.script().trace().iter().any(|write| {
        write.point.label == CommitLabel::CLAIM
            && write.node.as_ref() == node
            && matches!(write.stored, Stored::Committed { effective: true })
    })
}

/// S7: a receipt owes exactly what its derivation resolves. A's build
/// streams through a stream hook and derives through a response hook, with
/// no stream-finished hook; B's build has the response hook alone. Nothing
/// finishes the stream, so nothing replays it: B claims the turn, derives
/// A's receipt and answers the paid answer.
#[tokio::test]
async fn a_node_without_a_stream_hook_no_finish_reads_derives_the_receipt() {
    let deployment = Deployment::new(
        Setup::new(Answer::Text, Hooks::StreamAndResponse).b_on_its_own_core(Hooks::ResponseOnly),
        |_| {},
    )
    .await;
    *deployment.world.hold.lock_recover() = Some(Hold::Derivation);
    let _handle = deployment.send().await;
    deployment
        .until("A's response hook holds", |deployment| {
            deployment.world.held()
        })
        .await;
    deployment.take_over().await;
    deployment.settle().await;
    assert_eq!(
        deployment.world.sends(),
        1,
        "B sends nothing of A's purchase"
    );
    assert_eq!(
        answered(&deployment.transcript().await),
        format!("{}{HOOKED}", ANSWERS[0])
    );
    deployment.end();
}

/// S7: a failed stream's finish is its derivation's too. The provider fails
/// after streaming and A dies as the receipt commits. B's build has no
/// stream-finished hook, so it never claims the turn; A's build, on node C,
/// finishes the failed stream with its provider error.
#[tokio::test]
async fn a_failed_streams_finish_is_owed_to_a_node_that_serves_it() {
    let deployment = Deployment::new(
        Setup::new(Answer::Failed, Hooks::All).b_on_its_own_core(Hooks::StreamAndResponse),
        |script| {
            script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::CommitThenAbort);
        },
    )
    .await;
    let handle = deployment.send().await;
    deployment
        .until("A dies with its receipt", |deployment| {
            deployment.cut(CommitLabel::MODEL_ANSWER)
        })
        .await;
    deployment.take_over().await;
    run_for(&deployment, 30_000).await;
    assert!(
        !claimed_by(&deployment, "b"),
        "a node that cannot finish the receipt's stream claimed it:\n{}",
        deployment.nodes.script().rendered_trace()
    );
    deployment.nodes.start("c");
    deployment.settle().await;
    assert_eq!(deployment.world.last_finish(), Some(Reason::ProviderError));
    let output = handle.output().await.expect("the turn settles");
    assert!(matches!(output.status(), lash::TurnStatus::Failed { .. }));
    deployment.end();
}

/// A stream-finished hook no response hook reads still ends the stream it
/// watched, for the reason the stream ended: the call's derivation
/// finishes it from the receipt.
#[tokio::test]
async fn a_finish_only_stream_is_finished_for_its_reason() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::FinishOnly), |_| {}).await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    let finished = deployment.world.finished.lock_recover().clone();
    assert_eq!(
        finished
            .iter()
            .map(|(reason, _)| *reason)
            .collect::<Vec<_>>(),
        [Reason::Complete],
        "the answered call's stream finishes once"
    );
    assert!(finished.iter().all(|(_, acknowledged)| *acknowledged >= 1));
    deployment.end();
}

/// A stream that failed is finished for its provider error, from the
/// receipt: no response hook runs over a failure.
#[tokio::test]
async fn a_failed_stream_is_finished_for_its_provider_error() {
    let deployment = Deployment::new(Setup::new(Answer::Failed, Hooks::All), |_| {}).await;
    let handle = deployment.send().await;
    deployment.settle().await;
    assert_eq!(
        deployment
            .world
            .finished
            .lock_recover()
            .iter()
            .map(|(reason, _)| *reason)
            .collect::<Vec<_>>(),
        [Reason::ProviderError]
    );
    assert!(deployment.world.derived().is_empty());
    let output = handle.output().await.expect("the turn settles");
    assert!(matches!(output.status(), lash::TurnStatus::Failed { .. }));
    deployment.end();
}

/// What a call does, for the cancel laws.
#[derive(Clone, Copy, Debug)]
enum Streams {
    /// A call to [`WRITE`], through every hook.
    Hooked,
    /// A call to [`WRITE`], its stream finished by a hook no response hook
    /// reads.
    FinishOnly,
    /// A provider failure after two deltas, through every hook.
    Failed,
}

impl Streams {
    fn setup(self) -> Setup {
        match self {
            Self::Hooked => Setup::new(Answer::Tool, Hooks::All),
            Self::FinishOnly => Setup::new(Answer::Tool, Hooks::FinishOnly),
            Self::Failed => Setup::new(Answer::Failed, Hooks::All),
        }
    }

    /// The commit after the receipt in the uncancelled run.
    fn next_phase(self) -> CommitLabel {
        match self {
            Self::Hooked | Self::FinishOnly => CommitLabel::MODEL_DONE,
            Self::Failed => CommitLabel::TURN_COMMIT,
        }
    }

    /// The reason the call's stream ends for.
    fn finish(self) -> Reason {
        match self {
            Self::Hooked | Self::FinishOnly => Reason::Complete,
            Self::Failed => Reason::ProviderError,
        }
    }

    /// Every cut between the receipt and the next phase this call has.
    fn cuts(self) -> Vec<ReceiptCut> {
        match self {
            Self::Hooked => vec![
                ReceiptCut::Receipt,
                ReceiptCut::Finish,
                ReceiptCut::Derivation,
                ReceiptCut::NextPhase,
            ],
            Self::FinishOnly | Self::Failed => vec![
                ReceiptCut::Receipt,
                ReceiptCut::Finish,
                ReceiptCut::NextPhase,
            ],
        }
    }
}

/// Where a cancel law cuts A: after its receipt committed and before the
/// next phase did. A never reads the cancel; B finds it with the receipt.
#[derive(Clone, Copy, Debug)]
enum ReceiptCut {
    /// A dies as the receipt commits.
    Receipt,
    /// A holds in the stream finish its derivation runs, and is killed.
    Finish,
    /// A holds in its response hook, and is killed.
    Derivation,
    /// A derived the answer and dies before its next phase enters the
    /// store.
    NextPhase,
}

/// How a turn cancelled at its receipt ended, as its host and the outside
/// world see it.
#[derive(Debug, PartialEq)]
struct Ended {
    status: lash::TurnStatus,
    /// The protocol iteration an `AfterStep` stop names.
    honoured_after_step: Option<usize>,
    writes: Vec<String>,
    sends: usize,
    /// The sealed model-call records the session's live feed published.
    calls: usize,
    /// The reason the last stream finish ended its stream for.
    finish: Option<Reason>,
}

/// A cancel of `mode` requested at the receipt of `streams`' call: while
/// the receipt's acknowledgement is delayed and its owner lives, or once
/// `cut` stopped A, which B then takes over.
async fn cancelled_at_the_receipt(
    streams: Streams,
    mode: lash::TurnCancelMode,
    cut: Option<ReceiptCut>,
) -> Ended {
    let next = streams.next_phase();
    let deployment = Deployment::new(streams.setup(), |script| match cut {
        None => {
            script.cut_on(
                "a",
                CommitLabel::MODEL_ANSWER,
                1,
                Fault::DelayedAck(Duration::from_secs(30)),
            );
        }
        Some(ReceiptCut::Receipt) => {
            script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::CommitThenAbort);
        }
        Some(ReceiptCut::NextPhase) => {
            script.cut_on("a", next, 1, Fault::Abort);
        }
        Some(ReceiptCut::Finish | ReceiptCut::Derivation) => {}
    })
    .await;
    *deployment.world.hold.lock_recover() = match cut {
        Some(ReceiptCut::Finish) => Some(Hold::Finish),
        Some(ReceiptCut::Derivation) => Some(Hold::Derivation),
        _ => None,
    };
    let handle = deployment.send().await;
    deployment
        .until("A reaches its cut", |deployment| match cut {
            None => deployment.committed(CommitLabel::MODEL_ANSWER) == 1,
            Some(ReceiptCut::Receipt) => deployment.cut(CommitLabel::MODEL_ANSWER),
            Some(ReceiptCut::NextPhase) => deployment.cut(next),
            Some(ReceiptCut::Finish | ReceiptCut::Derivation) => deployment.world.held(),
        })
        .await;
    handle
        .cancel()
        .mode(mode)
        .await
        .expect("the cancel is requested");
    if cut.is_some() {
        deployment.take_over().await;
    }
    deployment.settle().await;
    let cursor = handle.cursor().clone();
    let output = handle.output().await.expect("the turn settles");
    let honoured_after_step = match &output.outcome {
        lash::TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { evidence }) => {
            evidence.honoured_after_step
        }
        _ => None,
    };
    let ended = Ended {
        status: output.status(),
        honoured_after_step,
        writes: markers(&deployment.world),
        sends: deployment.world.all_sends.load(Ordering::SeqCst),
        calls: deployment.published_calls(&cursor).await,
        finish: deployment.world.last_finish(),
    };
    deployment.end();
    ended
}

/// A cancel of `mode` at the receipt of `streams`' call ends the turn as
/// the uncut run does at every cut between the receipt and the next phase:
/// the same outcome, the same tool writes, no second purchase, the paid
/// call reported once and its stream finished for its reason.
async fn a_cancel_at_the_receipt_resumes_to_the_uncut_result(
    streams: Streams,
    mode: lash::TurnCancelMode,
) {
    let uncut = cancelled_at_the_receipt(streams, mode, None).await;
    assert_eq!(
        uncut.sends, 1,
        "{streams:?}: the paid answer is bought once"
    );
    assert_eq!(
        uncut.calls, 1,
        "{streams:?}: the paid call is published once"
    );
    assert_eq!(
        uncut.finish,
        Some(streams.finish()),
        "{streams:?}: the call's stream is finished for its reason"
    );
    match (mode, streams) {
        (lash::TurnCancelMode::Immediate, _) => {
            assert!(
                uncut.writes.is_empty(),
                "no tool starts after an immediate cancel"
            );
            assert_eq!(uncut.status, lash::TurnStatus::Cancelled);
        }
        (_, Streams::Failed) => {
            assert!(matches!(uncut.status, lash::TurnStatus::Failed { .. }));
        }
        (_, Streams::Hooked | Streams::FinishOnly) => {
            assert_eq!(uncut.writes, [ANSWERS[0]], "the step's tool runs once");
            assert_eq!(uncut.status, lash::TurnStatus::Cancelled);
        }
    }
    for cut in streams.cuts() {
        let ended = cancelled_at_the_receipt(streams, mode, Some(cut)).await;
        assert_eq!(ended, uncut, "{streams:?} {mode:?} cut at {cut:?}");
    }
}

/// REPLAY-PRESERVES-CONTROL-AND-FEED: an `Immediate` cancel at the receipt
/// of a hooked call stops the turn before its tool, and reports the paid
/// call, at every cut.
#[tokio::test]
async fn an_immediate_cancel_at_a_hooked_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::Hooked,
        lash::TurnCancelMode::Immediate,
    )
    .await;
}

/// REPLAY-PRESERVES-CONTROL-AND-FEED: an `AfterStep` cancel at the receipt
/// of a hooked call lets the step finish, its tool run once, and stops the
/// turn at the next model start, at every cut.
#[tokio::test]
async fn an_after_step_cancel_at_a_hooked_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::Hooked,
        lash::TurnCancelMode::AfterStep,
    )
    .await;
}

/// An `Immediate` cancel at the receipt of a call whose stream only a
/// stream-finished hook watches, at every cut.
#[tokio::test]
async fn an_immediate_cancel_at_a_finish_only_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::FinishOnly,
        lash::TurnCancelMode::Immediate,
    )
    .await;
}

/// An `AfterStep` cancel at the receipt of a call whose stream only a
/// stream-finished hook watches, at every cut.
#[tokio::test]
async fn an_after_step_cancel_at_a_finish_only_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::FinishOnly,
        lash::TurnCancelMode::AfterStep,
    )
    .await;
}

/// An `Immediate` cancel at the receipt of a failed call, at every cut.
#[tokio::test]
async fn an_immediate_cancel_at_a_failed_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::Failed,
        lash::TurnCancelMode::Immediate,
    )
    .await;
}

/// An `AfterStep` cancel at the receipt of a failed call lets the failure
/// end the turn, at every cut.
#[tokio::test]
async fn an_after_step_cancel_at_a_failed_receipt_ends_as_the_uncut_run_at_every_cut() {
    a_cancel_at_the_receipt_resumes_to_the_uncut_result(
        Streams::Failed,
        lash::TurnCancelMode::AfterStep,
    )
    .await;
}

/// FIG-6274: a node that serves only the session's `Immediate` cancel ends
/// an answered model phase as every other path does, publishing the paid
/// call's sealed record once. A dies as the receipt of a call whose stream
/// it finishes commits; B's build cannot finish that stream, so it claims
/// the session for the cancel alone and ends the turn from its rows.
#[tokio::test]
async fn a_cancel_only_node_ending_an_answered_phase_publishes_the_record_once() {
    let deployment = Deployment::new(
        Setup::new(Answer::Text, Hooks::All).b_on_its_own_core(Hooks::StreamAndResponse),
        |script| {
            script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::CommitThenAbort);
        },
    )
    .await;
    let handle = deployment.send().await;
    deployment
        .until("A dies with its receipt", |deployment| {
            deployment.cut(CommitLabel::MODEL_ANSWER)
        })
        .await;
    handle
        .cancel()
        .mode(lash::TurnCancelMode::Immediate)
        .await
        .expect("the cancel is requested");
    deployment.take_over().await;
    deployment.settle().await;
    assert!(
        claimed_by(&deployment, "b"),
        "the cancel-only node ended the turn:\n{}",
        deployment.nodes.script().rendered_trace()
    );
    let cursor = handle.cursor().clone();
    let output = handle.output().await.expect("the turn settles");
    assert_eq!(output.status(), lash::TurnStatus::Cancelled);
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    assert_eq!(
        deployment.published_calls(&cursor).await,
        1,
        "the paid call's record is published once"
    );
    deployment.end();
}

/// Read only the committed-turn surface, so the missing retained record is
/// observable on the unfixed implementation as well as after this change.
async fn retained_model_rows(durable: &lash::DurableSession) -> (lash::TurnId, Vec<Value>) {
    let page = durable
        .committed_turns(None, std::num::NonZeroU32::MIN)
        .await
        .expect("committed turns");
    let turn = page
        .turns
        .first()
        .expect("one committed turn")
        .turn_id
        .clone();
    let read = durable
        .committed_turn(&turn)
        .await
        .expect("read turn")
        .expect("retained turn");
    let json = serde_json::to_value(read).expect("encode read");
    let rows = json
        .get("model_calls")
        .and_then(Value::as_array)
        .expect("every committed turn retains its model calls")
        .clone();
    assert!(
        !rows.is_empty(),
        "a completed provider call has one committed record"
    );
    (turn, rows)
}

#[tokio::test]
async fn a_late_follower_reads_the_same_committed_model_calls() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |_| {}).await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    let (turn, rows) = retained_model_rows(&deployment.durable).await;
    let follower_core = self::core(
        &deployment.backend,
        &deployment.clock,
        &deployment.world,
        &deployment.live_replay,
        Answer::Text,
        Hooks::All,
        "late-follower",
    );
    let follower = follower_core
        .session(session())
        .durable()
        .await
        .expect("late follower on a fresh core");
    let (_, followed) = retained_model_rows(&follower).await;
    assert_eq!(rows, followed);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["call"], 1);
    assert_eq!(rows[0]["end"], "answered");
    assert_eq!(deployment.world.sends(), 1);
    assert!(follower.committed_turn(&turn).await.unwrap().is_some());
    deployment.end();
}

#[tokio::test]
async fn a_model_record_commits_with_its_receipt_and_outlives_consumption() {
    retained_receipt(false).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn postgres_a_model_record_commits_with_its_receipt_and_outlives_consumption() {
    retained_receipt(true).await;
}

async fn retained_receipt(postgres: bool) {
    let mut setup = Setup::new(Answer::Text, Hooks::All);
    setup.postgres = postgres;
    let deployment = Deployment::new(setup, |_| {}).await;
    *deployment.world.hold.lock_recover() = Some(Hold::Derivation);
    let _handle = deployment.send().await;
    deployment.until("paid receipt", |d| d.world.held()).await;
    assert_eq!(deployment.committed(CommitLabel::MODEL_ANSWER), 1);
    let open = deployment
        .nodes
        .database()
        .turn(&session())
        .await
        .unwrap()
        .unwrap();
    let early = deployment
        .nodes
        .database()
        .model_calls(
            &session(),
            &lash_durable::domain::ModelCallId::turn_owner(&open.run),
        )
        .await
        .unwrap();
    assert_eq!(
        early.len(),
        1,
        "the call row is readable with the acknowledged receipt"
    );
    let reads = deployment
        .durable
        .read_model_calls(&[lash::turn::TurnModelCallRef {
            turn_id: open.run,
            call_id: early[0].call_id.clone(),
        }])
        .await
        .unwrap();
    assert!(matches!(
        reads[0],
        lash::turn::ModelCallRead::Settled { .. }
    ));
    deployment.take_over().await;
    deployment.settle().await;
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(deployment.world.sends(), 1);
    assert_eq!(deployment.usage().await, USAGE);
    deployment.end();
}

#[tokio::test]
async fn a_resent_model_call_counts_the_attempt_its_process_lost() {
    let deployment = Deployment::new(Setup::new(Answer::Text, Hooks::All), |script| {
        script.cut_on("a", CommitLabel::MODEL_ANSWER, 1, Fault::Zombie);
    })
    .await;
    let _handle = deployment.send().await;
    deployment
        .until("uncommitted provider answer", |d| {
            d.nodes.paused().contains(&"a".to_owned())
        })
        .await;
    deployment.nodes.start("b");
    deployment.settle().await;
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(rows.len(), 1);
    let record: lash::LlmCallRecord =
        serde_json::from_value(rows[0]["record"].clone()).expect("sealed record");
    assert_eq!(record.attempts.len(), 2);
    assert_eq!(record.attempts[0].ordinal, 1);
    assert_eq!(
        record.attempts[0].outcome,
        lash::provider::AttemptOutcome::Interrupted
    );
    assert_eq!(
        serde_json::to_value(record.attempts[0].protocol_position).unwrap(),
        "unobserved"
    );
    assert_eq!(record.attempts[0].usage, None);
    assert_eq!(record.attempts[1].ordinal, 2);
    assert_eq!(
        record.attempts[1].outcome,
        lash::provider::AttemptOutcome::Completed,
        "{rows:#?}\n{}",
        deployment.nodes.script().rendered_trace()
    );
    let usage = record.usage().expect("usage");
    assert_eq!(usage.unknown_calls, 1);
    assert_eq!(usage, USAGE.checked_add(&LlmUsage::unknown_call()).unwrap());
    assert_eq!(deployment.usage().await, usage);
    assert_eq!(deployment.world.sends(), 2);
    deployment.end();
}

#[tokio::test]
async fn a_consumed_receipts_lost_tail_commits_one_unobserved_record() {
    let deployment = Deployment::new(Setup::new(Answer::AbortedCell, Hooks::All), |script| {
        script.cut_on(
            "a",
            CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT,
            1,
            Fault::CommitThenAbort,
        );
    })
    .await;
    deployment.world.hold_tail.store(1, Ordering::SeqCst);
    let _handle = deployment.send().await;
    deployment
        .until("receipt consumed before tail settlement", |d| {
            d.cut(CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT)
        })
        .await;
    let open = deployment
        .nodes
        .database()
        .turn(&session())
        .await
        .unwrap()
        .unwrap();
    let pins = deployment
        .nodes
        .database()
        .model_call_pins(
            &session(),
            &lash_durable::domain::ModelCallId::turn_owner(&open.run),
        )
        .await
        .unwrap();
    assert_eq!(pins.len(), 1);
    let reads = deployment
        .durable
        .read_model_calls(&[lash::turn::TurnModelCallRef {
            turn_id: open.run,
            call_id: pins[0].call_id.clone(),
        }])
        .await
        .unwrap();
    assert!(matches!(
        reads[0],
        lash::turn::ModelCallRead::NotYetSettled { .. }
    ));
    deployment.take_over().await;
    deployment.settle().await;
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    let first: lash::turn::ModelCallRecord =
        serde_json::from_value(rows[0].clone()).expect("retained call");
    assert_eq!(first.end, lash::turn::ModelCallEnd::Answered);
    assert_eq!(first.record.attempts.len(), 1);
    assert_eq!(
        first.record.attempts[0].protocol_position,
        lash::provider::ProtocolPosition::Unobserved
    );
    assert_eq!(first.usage().unwrap().unknown_calls, 1);
    assert_eq!(deployment.world.sends(), 1);
    assert_eq!(
        rows.iter()
            .filter(|row| row["call_id"] == rows[0]["call_id"])
            .count(),
        1
    );
    deployment.end();
}

#[tokio::test]
async fn a_reserved_retry_lost_with_its_process_never_reuses_its_ordinal() {
    let label = CommitLabel::new("model.attempt");
    let deployment = Deployment::new(Setup::new(Answer::RetriedText, Hooks::All), |script| {
        script.cut_on("a", label, 1, Fault::CommitThenAbort);
    })
    .await;
    let _handle = deployment.send().await;
    deployment
        .until("retry ordinal committed", |d| d.cut(label))
        .await;
    deployment.take_over().await;
    deployment.settle().await;
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(rows.len(), 1);
    let call: lash::turn::ModelCallRecord = serde_json::from_value(rows[0].clone()).unwrap();
    assert_eq!(
        call.record
            .attempts
            .iter()
            .map(|attempt| attempt.ordinal)
            .collect::<Vec<_>>(),
        [1, 2, 3],
        "{call:#?}\n{}",
        deployment.nodes.script().rendered_trace()
    );
    assert!(
        call.record.attempts[..2].iter().all(
            |attempt| attempt.protocol_position == lash::provider::ProtocolPosition::Unobserved
        )
    );
    assert_eq!(
        call.record.attempts[2].outcome,
        lash::provider::AttemptOutcome::Completed
    );
    assert_eq!(call.usage().unwrap(), deployment.usage().await);
    deployment.end();
}

/// One turn with a returned retry, an accepted cell whose tail is cancelled
/// at its deadline, and a final completed call.
async fn mixed_call(request: &LlmRequest, sent: usize) -> Result<LlmResponse, LlmTransportError> {
    let usage = LlmUsage {
        input_tokens: 2,
        output_tokens: 1,
        ..Default::default()
    };
    if sent == 0 {
        return Err(LlmTransportError::new("retry with partial usage")
            .with_kind(lash_core::ProviderFailureKind::Stream)
            .with_retry_verdict(TransportRetryVerdict::RetryableTransient)
            .with_partial_response(LlmResponse {
                usage,
                provider_usage: Some(json!({"input_tokens":2,"output_tokens":1})),
                ..Default::default()
            }));
    }
    if sent == 2 {
        request
            .stream_events
            .as_ref()
            .unwrap()
            .send(LlmStreamEvent::Usage(usage));
        stream(
            request,
            &["<typescript>\nconsole.log(\"second cell\");\n</typescript>"],
        );
        return std::future::pending().await;
    }
    let text = if sent == 1 {
        "<typescript>\nconsole.log(\"retried cell\");\n</typescript>"
    } else {
        "<typescript>\nawait control.finish(\"done\");\n</typescript>"
    };
    Ok(LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_owned(),
            response_meta: None,
        }],
        usage: USAGE,
        terminal_reason: lash_core::LlmTerminalReason::Stop,
        ..Default::default()
    })
}

async fn retained_usage_matches_head(postgres: bool) {
    let mut setup = Setup::new(Answer::MixedCalls, Hooks::All);
    setup.postgres = postgres;
    let deployment = Deployment::new(setup, |_| {}).await;
    let handle = deployment.send().await;
    deployment.settle().await;
    handle.output().await.expect("the mixed turn completes");
    let (_, rows) = retained_model_rows(&deployment.durable).await;
    assert_eq!(rows.len(), 3, "one row per logical call: {rows:#?}");
    let calls = rows
        .into_iter()
        .map(|row| serde_json::from_value::<lash::turn::ModelCallRecord>(row).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        calls.iter().map(|call| call.call).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(calls[0].record.attempts.len(), 2, "the first call retried");
    assert_eq!(
        calls[1].record.attempts.last().unwrap().outcome,
        lash::provider::AttemptOutcome::Interrupted
    );
    assert_eq!(
        calls[2].record.attempts.last().unwrap().outcome,
        lash::provider::AttemptOutcome::Completed
    );
    let usage = calls
        .iter()
        .try_fold(LlmUsage::default(), |sum, call| {
            sum.checked_add(&call.usage()?)
        })
        .unwrap();
    assert_eq!(usage, deployment.usage().await);
    assert_eq!(usage.input_tokens, 26);
    assert_eq!(usage.output_tokens, 16);
    deployment.end();
}

#[tokio::test]
async fn the_heads_usage_delta_equals_its_retried_cancelled_and_completed_calls() {
    retained_usage_matches_head(false).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn postgres_the_heads_usage_delta_equals_its_retried_cancelled_and_completed_calls() {
    retained_usage_matches_head(true).await;
}

/// MODEL-HOOK-ADDRESS: live attempts are distinct; another node derives the
/// second call from its receipt with the same address and no provider send.
#[tokio::test]
async fn model_hooks_keep_call_attempt_addresses_across_retry_and_receipt_takeover() {
    let deployment = Deployment::new(
        Setup::new(Answer::RetryTool, Hooks::All).b_on_its_own_core(Hooks::All),
        |_| {},
    )
    .await;
    deployment.world.hold_second.store(true, Ordering::SeqCst);
    *deployment.world.hold.lock_recover() = Some(Hold::Derivation);
    let _handle = deployment.send().await;
    deployment
        .until("second call's derivation holds", |d| d.world.held())
        .await;
    let before = deployment.world.response_addresses.lock_recover().clone();
    let first = before.first().expect("the first call was derived").clone();
    let second = before
        .last()
        .expect("the second call is being derived")
        .clone();
    assert_ne!(first.call_id, second.call_id);
    assert_eq!(first.turn_id, second.turn_id);
    assert_eq!(first.session_id, session());
    assert_eq!(first.attempt, Some(1));
    assert_eq!(second.attempt, Some(2));
    assert!(
        before
            .iter()
            .all(|address| address == &first || address == &second)
    );
    deployment.take_over().await;
    deployment.settle().await;
    let responses = deployment.world.response_addresses.lock_recover().clone();
    assert!(
        responses.len() > before.len(),
        "the other node derived the receipt"
    );
    assert_eq!(&responses[..before.len()], before.as_slice());
    assert!(
        responses[before.len()..]
            .iter()
            .all(|address| address == &second),
        "every repeated derivation keeps the receipt's answering address"
    );
    let finishes = deployment.world.finished_addresses.lock_recover();
    let terminals: Vec<_> = finishes
        .iter()
        .filter(|(reason, _)| *reason != Reason::AttemptReset)
        .map(|(_, address)| address.clone())
        .collect();
    assert!(terminals.contains(&first) && terminals.contains(&second));
    assert!(
        terminals
            .iter()
            .all(|address| address == &first || address == &second)
    );
    assert_eq!(terminals.last(), Some(&second));
    assert!(
        finishes
            .windows(2)
            .filter(|pair| pair[1].0 != Reason::AttemptReset)
            .all(|pair| pair[0] == (Reason::AttemptReset, pair[1].1.clone())),
        "the replay's synthetic reset names the receipt's answering attempt"
    );
    drop(finishes);
    let streamed = deployment.world.stream_addresses.lock_recover();
    assert!(
        streamed
            .iter()
            .all(|address| address.session_id == first.session_id
                && address.turn_id == first.turn_id)
    );
    let distinct: std::collections::BTreeSet<_> = streamed
        .iter()
        .map(|address| (address.call_id.0.clone(), address.attempt))
        .collect();
    let expected: std::collections::BTreeSet<_> = [
        (first.call_id.0.clone(), Some(1)),
        (second.call_id.0.clone(), Some(1)),
        (second.call_id.0.clone(), Some(2)),
    ]
    .into_iter()
    .collect();
    assert_eq!(distinct, expected, "{streamed:?}");
    assert_eq!(deployment.world.all_sends.load(Ordering::SeqCst), 3);
    drop(streamed);
    deployment.end();
}
