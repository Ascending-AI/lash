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
//!   receipt with byte-identical input, with one provider send.
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
//! - **REPLAY-PRESERVES-CONTROL-AND-FEED:** an `Immediate` cancel delivered
//!   while the receipt's acknowledgement is delayed stops the turn before
//!   its tool runs; an `AfterStep` one lets the tool run once and stops the
//!   turn at the next model start. The feed half is in
//!   `tool_crash_laws.rs`
//!   (`a_native_call_cut_at_its_receipt_streams_its_activity_and_commits_one_outcome`).

// Test code.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/sim.rs"]
mod sim;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash::tools::{StaticToolExecute, StaticToolProvider};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{
    LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, StreamBlockEvent, StreamBlockIdentity,
    StreamBlockKind,
};
use lash_core::runtime::durable::session::SessionActivation;
use lash_core::{ExecutionPolicy, LlmOutputPart, LlmUsage, ToolCall, ToolOutcome};
use lash_core_execution::{Backend, BackendParts, NoProjectionProviders, StoreSet};
use lash_durable::runner::Activation;
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{Fault, Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt as _;
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
/// The usage every send reports.
const USAGE: LlmUsage = LlmUsage {
    input_tokens: 11,
    output_tokens: 7,
    cache_read_input_tokens: 0,
    cache_write_input_tokens: 0,
    reasoning_output_tokens: 0,
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
    /// The stream-finished hook, ending a stream that produced a response.
    Finish,
}

/// The outside world every node and core shares.
#[derive(Default)]
struct World {
    /// Provider sends of the turn's first call.
    sends: AtomicUsize,
    /// Every provider send.
    all_sends: AtomicUsize,
    /// What each response-hook run was handed: the response and the stream
    /// state, as JSON, and how many receipts were acknowledged by then.
    derived: Mutex<Vec<(Value, usize)>>,
    /// The reason of each stream finish that ended a stream with a
    /// response, and how many receipts were acknowledged by then.
    finished: Mutex<Vec<usize>>,
    /// The hook that holds on its next run.
    hold: Mutex<Option<Hold>>,
    /// Whether a hook is held now.
    held: AtomicUsize,
    /// Receipt acknowledgements their owners received.
    acknowledged: AtomicUsize,
    /// Commits that entered the store after a receipt and before its
    /// acknowledgement reached its owner.
    early: Mutex<Vec<String>>,
    /// The arguments of every `write` body entry.
    writes: Mutex<Vec<Value>>,
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
    /// A call to [`WRITE`] beside text, then `done`.
    Tool,
    /// A TypeScript cell that calls [`WRITE`] with its send's marker, then
    /// `done`.
    Cell,
    /// A TypeScript cell that only prints, then `done`.
    PrintingCell,
    /// The [`Answer::Cell`] cell streamed with trailing text and never
    /// completed: the cell mask aborts the stream at its close tag.
    AbortedCell,
}

impl Answer {
    fn code(self) -> bool {
        matches!(self, Self::Cell | Self::PrintingCell | Self::AbortedCell)
    }
}

/// The model over `world`.
fn model(world: Arc<World>, answer: Answer) -> ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("model-answer-receipt")
        .requires_streaming(true)
        .complete(move |mut request: LlmRequest| {
            let world = Arc::clone(&world);
            async move {
                world.all_sends.fetch_add(1, Ordering::SeqCst);
                let answered = request
                    .messages
                    .iter()
                    .any(|message| message.role == LlmRole::Assistant);
                if answered {
                    return Ok(streamed(&request, &["do", "ne"]));
                }
                let send = world.sends.fetch_add(1, Ordering::SeqCst);
                let marker = ANSWERS[send.min(ANSWERS.len() - 1)];
                let mut response = match answer {
                    Answer::Text => {
                        let (head, tail) = marker.split_at(4);
                        let (middle, tail) = tail.split_at(6);
                        streamed(&request, &[head, middle, tail])
                    }
                    Answer::Tool => {
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
                        pieces.push("\nTrailing prose the mask cuts.".to_owned());
                        stream(&request, &pieces);
                        // The provider never completes: the mask's abort
                        // ends the call, and the runtime synthesizes its
                        // response from what streamed.
                        drop(request.stream_events.take());
                        std::future::pending::<()>().await;
                        unreachable!("an aborted call never completes");
                    }
                };
                response.usage = USAGE;
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
}

/// The law plugin over `world`. Its stream hook keeps the chunks it was
/// handed in this core's memory: the state a node's stream hooks hold.
fn plugin(world: &Arc<World>, hooks: Hooks) -> Arc<dyn lash::plugins::PluginFactory> {
    let chunks = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = Arc::clone(&chunks);
    let mut spec = lash::plugins::PluginSpec::new().with_assistant_stream(
        lash::hook_key!("chunks"),
        Arc::new(move |ctx| {
            seen.lock_recover().push(ctx.chunk.clone());
            Box::pin(async move {
                Ok(lash::plugins::AssistantStreamTransform {
                    chunk: ctx.chunk,
                    ..Default::default()
                })
            })
        }),
    );
    if hooks == Hooks::All {
        let (finishing, deriving) = (Arc::clone(world), Arc::clone(world));
        spec = spec
            .with_assistant_stream_finished(
                lash::hook_key!("chunks"),
                Arc::new(move |ctx| {
                    let world = Arc::clone(&finishing);
                    let held = std::mem::take(&mut *chunks.lock_recover());
                    Box::pin(async move {
                        use lash::plugins::AssistantStreamFinishReason as Reason;
                        if !matches!(ctx.reason, Reason::Complete | Reason::Aborted) {
                            return Ok(None);
                        }
                        world
                            .finished
                            .lock_recover()
                            .push(world.acknowledged.load(Ordering::SeqCst));
                        world.hold_if(Hold::Finish).await;
                        Ok(Some(json!(held)))
                    })
                }),
            )
            .with_assistant_response(
                lash::hook_key!("suffix"),
                Some(lash::hook_key!("chunks")),
                Arc::new(move |ctx| {
                    let world = Arc::clone(&deriving);
                    Box::pin(async move {
                        let handed = json!({
                            "response": serde_json::to_value(&ctx.response).unwrap(),
                            "stream_state": ctx.stream_state,
                        });
                        world
                            .derived
                            .lock_recover()
                            .push((handed, world.acknowledged.load(Ordering::SeqCst)));
                        world.hold_if(Hold::Derivation).await;
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
fn core(
    backend: &Backend,
    clock: &Arc<SimClock>,
    world: &Arc<World>,
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
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
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
}

impl Setup {
    fn new(answer: Answer, hooks: Hooks) -> Self {
        Self {
            answer,
            hooks,
            b_hooks: None,
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
    clock: Arc<SimClock>,
    nodes: SimNodes,
    world: Arc<World>,
    core: lash::LashCore,
    _b_core: Option<lash::LashCore>,
    durable: lash::DurableSession,
}

impl Deployment {
    /// The deployment of `setup`, its fault rules armed by `arm`.
    async fn new(setup: Setup, arm: impl FnOnce(&Script)) -> Self {
        let clock = SimClock::new();
        let stores = sim::memory(Arc::clone(&clock)).await;
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let backend = Backend::assemble(BackendParts {
            stores: Arc::new(stores) as Arc<dyn StoreSet>,
            settings: sim::settings(),
            engines: Vec::new(),
            providers: Arc::new(NoProjectionProviders),
            formats: lash::formats::actor_state_surfaces(),
        })
        .expect("the backend assembles");
        let world = Arc::new(World::default());
        let core = core(
            &backend,
            &clock,
            &world,
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
            let b_core = self::core(&backend, &clock, &world, setup.answer, hooks, "receipt-b");
            nodes.activate_on("b", activation(&backend, &b_core));
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
            clock,
            nodes,
            world,
            core,
            _b_core: b_core,
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
        !finished.is_empty() && finished.iter().all(|acknowledged| *acknowledged >= 1),
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

/// The uncut run's derivations of `answer`'s turn, for a law to compare a
/// resumed run with.
async fn uncut_derivations(answer: Answer) -> Vec<Value> {
    let deployment = Deployment::new(Setup::new(answer, Hooks::All), |_| {}).await;
    let _handle = deployment.send().await;
    deployment.settle().await;
    let derived = deployment
        .world
        .derived()
        .into_iter()
        .map(|(handed, _)| handed)
        .collect();
    deployment.end();
    derived
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS: A holds in its stream finish,
/// after the receipt committed, and is killed. B runs on a core of its own,
/// whose stream hooks never saw A's stream: it finishes the stream from the
/// receipt's capsule and derives what the uncut run derives, with no send.
async fn a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(answer: Answer) {
    let uncut = uncut_derivations(answer).await;
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
    deployment.end();
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS for a stream that completed.
#[tokio::test]
async fn a_completed_streams_finish_resumes_from_the_receipt_on_another_core() {
    a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(Answer::Cell).await;
}

/// RECEIPT-INCLUDES-STREAM-RECOVERY-INPUTS for a stream the cell mask
/// aborted at its close tag, whose response the runtime synthesized.
#[tokio::test]
async fn a_protocol_aborted_streams_finish_resumes_from_the_receipt_on_another_core() {
    a_stream_finish_killed_on_a_resumes_from_the_receipt_on_b(Answer::AbortedCell).await;
}

/// REPLAY-PRESERVES-CONTROL-AND-FEED: a cancel of `mode` delivered while the
/// receipt of a call that asks for [`WRITE`] waits for its
/// acknowledgement. Answers the deployment once its turn ended.
async fn a_cancel_at_the_receipt(mode: lash::TurnCancelMode) -> Deployment {
    let deployment = Deployment::new(Setup::new(Answer::Tool, Hooks::All), |script| {
        script.cut_on(
            "a",
            CommitLabel::MODEL_ANSWER,
            1,
            Fault::DelayedAck(Duration::from_secs(30)),
        );
    })
    .await;
    let handle = deployment.send().await;
    deployment
        .until("the receipt waits for its acknowledgement", |deployment| {
            deployment.committed(CommitLabel::MODEL_ANSWER) == 1
        })
        .await;
    handle
        .cancel()
        .mode(mode)
        .await
        .expect("the cancel is requested");
    deployment.settle().await;
    assert!(
        deployment.cut(CommitLabel::MODEL_ANSWER),
        "the receipt's acknowledgement was delayed"
    );
    assert_eq!(
        deployment.world.sends(),
        1,
        "the paid answer is bought once"
    );
    let output = handle.output().await.expect("the turn settles");
    assert_eq!(
        output.status(),
        lash::TurnStatus::Cancelled,
        "{:?}",
        output.result.outcome
    );
    deployment
}

/// REPLAY-PRESERVES-CONTROL-AND-FEED: an `Immediate` cancel wins over the
/// receipt: the turn stops before the call it asked for is admitted.
#[tokio::test]
async fn an_immediate_cancel_at_the_receipt_stops_the_turn_before_its_tool() {
    let deployment = a_cancel_at_the_receipt(lash::TurnCancelMode::Immediate).await;
    assert!(
        deployment.world.writes().is_empty(),
        "no tool starts after an immediate cancel"
    );
    deployment.end();
}

/// REPLAY-PRESERVES-CONTROL-AND-FEED: an `AfterStep` cancel lets the step
/// the receipt answers finish, its tool run once, and stops the turn at
/// the next model start, which sends nothing.
#[tokio::test]
async fn an_after_step_cancel_at_the_receipt_finishes_the_step_and_stops() {
    let deployment = a_cancel_at_the_receipt(lash::TurnCancelMode::AfterStep).await;
    assert_eq!(
        markers(&deployment.world),
        [ANSWERS[0]],
        "the step's tool runs once"
    );
    assert_eq!(deployment.world.all_sends.load(Ordering::SeqCst), 1);
    deployment.end();
}
