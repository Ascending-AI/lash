//! The executable aggregate oracle (FIG-3395).
//!
//! Everything this repository knew about async aggregates was asked of the VM
//! in isolation: `lashlang::execute` against a hand-written `ExecutionHost`
//! that answers in one `perform` call and decides its own settlement order.
//! That harness cannot state the questions the aggregate landing is about,
//! because the order leaves settle in is the host's answer rather than the
//! runtime's observation, and nothing durable is written. The test262 slice
//! cannot serve either: it answers only `Finish`/`Print`, bars
//! `flags: [async]`, and none of its vendored probes is a Promise test.
//!
//! So the cases below are authored async aggregates run end to end — provider
//! cell, RLM bridge, real tool dispatch, real batch scheduler — against a
//! **journaled** store tier, with a host whose leaves settle in an order the
//! *test* controls. The order is decided by construction rather than by
//! timing: a leaf either parks on a completion key this test resolves, or
//! blocks inside its attempt until this test releases it. No case sleeps, and
//! no case asserts on a duration.
//!
//! Three pins the FIG-3397 landing re-pointed rather than discovered:
//!
//! * [`sqlite_a_rejected_promise_all_resumes_at_its_first_consumed_rejection`]
//!   retires ADR 0062 deviation 15: a rejected `Promise.all` resumes at the
//!   first rejection its consumer takes, while a held sibling is still in
//!   flight as a loser (ADR 0099 §10 L2).
//! * [`sqlite_a_terminal_leaf_settles_ahead_of_a_held_source_first_leaf`]
//!   pins ADR 0099 §5's settlement semantics: group children commit in durable
//!   commit order, so a held source-first leaf does not block a later sibling's
//!   terminal and the first-settled selection reports the later leaf's
//!   rejection.
//! * [`the_standalone_list_batch_still_selects_the_first_written_rejection`]
//!   pins the ruling that Lashlang-native aggregates wait for every result and
//!   report their first *written* rejection (ADR 0099 §10 L7).
//!
//! The compile-time aggregate paths (`Instruction::ResourceOperationBatch`
//! and `Instruction::ResourceOperationListBatch`) have had no authored
//! spelling since ADR 0096 retired the second dialect, so they are stated
//! separately, at the IR, rather than through the bridge.
//!
//! ## Registering another tier
//!
//! Every case body takes a [`JournaledTier`] and asserts nothing about which
//! one it got, so PostgreSQL registers the same list by adding a
//! `JournaledTier` constructor and one `#[tokio::test]` wrapper per case.

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};

/// How long a rendezvous may wait before the case fails as an assertion rather
/// than hanging the suite. It is a deadlock budget, never an ordering device:
/// no assertion in this file depends on its value.
const RENDEZVOUS_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);

/// The event type a leaf's declared intent emits.
const INTENT_EVENT: &str = "aggregate.oracle.leaf";

/// An event type the intent target never registered: emitting it is refused.
const UNREGISTERED_EVENT: &str = "aggregate.oracle.unregistered";

// ---------------------------------------------------------------------------
// The journaled tier
// ---------------------------------------------------------------------------

/// A journaled store tier one oracle case runs against.
///
/// The cases never name the backend; they take a fresh backend from this
/// for each run and hand it to the builder. A SQLite memory backend is the
/// floor: it journals every checkpoint and effect through the same SQLite
/// stores a file backend runs, without paying a file sync per effect across
/// the long race loops.
struct JournaledTier {
    /// Names the tier in assertion messages, so a shared case body says which
    /// registration failed.
    name: &'static str,
}

impl JournaledTier {
    fn sqlite() -> Self {
        Self { name: "sqlite" }
    }

    /// A fresh backend for one run: its own sessions and its own process
    /// registry.
    async fn backend(&self) -> Arc<lash_sqlite_store::SqliteBackend> {
        Arc::new(
            lash_sqlite_store::SqliteBackend::memory()
                .await
                .expect("open the aggregate-oracle backend"),
        )
    }
}

// ---------------------------------------------------------------------------
// The theatre: what the host was asked to do, and who settles when
// ---------------------------------------------------------------------------

/// The host's record and the rendezvous the cases settle their leaves with.
///
/// Latches are keyed by a namespaced string, so a leaf's gate is named by the
/// `id` the cell wrote — the settlement plan is authored where the aggregate
/// is authored, and the case only says when to release it.
///
/// `settled` is recorded by the session's tool-result projector, which runs
/// inside each batch child just before its future resolves
/// (`session/tool_execution.rs::complete_tool_call`). It is the latest
/// per-leaf observation a test can take from inside the batch — the turn's
/// activity stream is not one, because `ToolCallCompleted` reaches the sink
/// only once the whole batch has returned.
///
/// Two mechanisms that look like they would order leaves do not, and both were
/// measured before this file settled on its own: a leaf released at the end of
/// a sibling's *attempt* is polled ahead of the sibling that woke it, because
/// the sibling still has its journaled attempt effect to await and
/// `FuturesUnordered` drains its ready queue in wake order; and a leaf that
/// waits for a sibling's turn activity waits forever.
#[derive(Default)]
struct OracleTheatre {
    /// Every `oracle.step` id, in the order the host was entered.
    started: StdMutex<Vec<String>>,
    /// Every `oracle.step` id, in the order the runtime completed its call.
    settled: StdMutex<Vec<String>>,
    /// Completed tool calls of any tool, which the shape cases count.
    completed_calls: AtomicUsize,
    keys: StdMutex<HashMap<String, lash_core::AwaitEventKey>>,
    latches: StdMutex<HashMap<String, Arc<tokio::sync::watch::Sender<bool>>>>,
    /// The process every declared intent is realized against: registered up
    /// front, so its id is the one the registrar minted (ADR 0107).
    intent_process: std::sync::OnceLock<lash_sansio::ProcessId>,
}

impl OracleTheatre {
    fn intent_process(&self) -> lash_sansio::ProcessId {
        self.intent_process
            .get()
            .cloned()
            .expect("the intent target is registered before the turn runs")
    }

    fn latch(&self, key: &str) -> Arc<tokio::sync::watch::Sender<bool>> {
        Arc::clone(
            self.latches
                .lock_recover()
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::watch::channel(false).0)),
        )
    }

    fn raise(&self, key: &str) {
        self.latch(key).send_replace(true);
    }

    /// Waits for `key`, giving up after [`RENDEZVOUS_BUDGET`] so a case that
    /// cannot make progress fails as an assertion instead of hanging.
    async fn wait(&self, key: &str) -> bool {
        let latch = self.latch(key);
        let mut receiver = latch.subscribe();
        tokio::time::timeout(RENDEZVOUS_BUDGET, async move {
            receiver.wait_for(|raised| *raised).await.is_ok()
        })
        .await
        .unwrap_or(false)
    }

    async fn await_started(&self, id: &str) {
        assert!(
            self.wait(&format!("started:{id}")).await,
            "{id} never ran; started so far: {:?}",
            self.started()
        );
    }

    async fn await_settled(&self, id: &str) {
        assert!(
            self.wait(&format!("settled:{id}")).await,
            "{id} never settled"
        );
    }

    /// Waits until the batch's consumer has consumed `count` settlements. The
    /// `settled:{id}` latch is raised by the presentation step, which a group
    /// child runs before its final commit takes a rank, so only consumption
    /// proves where in the commit order a leaf landed.
    async fn await_consumed(&self, count: usize) {
        assert!(
            self.wait(&format!("consumed:{count}")).await,
            "the consumer never consumed {count} settlement(s)"
        );
    }

    fn release(&self, id: &str) {
        self.raise(&format!("release:{id}"));
    }

    fn publish_key(&self, id: &str, key: lash_core::AwaitEventKey) {
        self.keys.lock_recover().insert(id.to_string(), key);
        self.raise(&format!("key:{id}"));
    }

    /// Settles a leaf that was dispatched with `defer: true`, from outside the batch.
    async fn settle_deferred(
        &self,
        core: &LashCore,
        id: &str,
        resolution: lash_core::Resolution,
    ) -> Result<()> {
        assert!(
            self.wait(&format!("key:{id}")).await,
            "{id} never published a completion key"
        );
        let key = self
            .keys
            .lock_recover()
            .get(id)
            .cloned()
            .expect("the published completion key");
        assert_eq!(
            core.completions().resolve(key, resolution).await?,
            lash_core::ResolveOutcome::Accepted,
            "{id}'s completion must be accepted"
        );
        Ok(())
    }

    fn rejection(id: &str) -> lash_core::Resolution {
        lash_core::Resolution::Err(crate::ExternalCompletionError::new(
            crate::provider::FailureCode::foreign(
                crate::provider::Namespace::host("oracle").expect("valid namespace"),
                "oracle_step_failed",
            )
            .expect("a validated host namespace is foreign-mintable"),
            format!("step {id} rejected"),
        ))
    }

    fn started(&self) -> Vec<String> {
        self.started.lock_recover().clone()
    }

    fn settled(&self) -> Vec<String> {
        self.settled.lock_recover().clone()
    }
}

/// The turn's activity sink: it counts completed tool calls, which the group
/// consumer emits as it consumes settlements in commit order, and raises
/// `consumed:{n}` once the n-th settlement has been consumed.
#[async_trait]
impl TurnActivitySink for OracleTheatre {
    async fn emit(&self, activity: TurnActivity) {
        if matches!(activity.event, TurnEvent::ToolCallCompleted { .. }) {
            let consumed = self.completed_calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.raise(&format!("consumed:{consumed}"));
        }
    }
}

/// One presentation step in the session's chain: it records the leaf's
/// settlement and raises its latch, then passes the previous step's reply
/// through unchanged.
fn oracle_presentation_step(
    theatre: Arc<OracleTheatre>,
) -> lash_core::facade_support::ToolPresentationStep {
    Arc::new(
        move |input: lash_core::facade_support::ToolPresentationInput| {
            if input.context.tool_name == "oracle_step"
                && let Some(id) = input
                    .context
                    .args
                    .get("id")
                    .and_then(serde_json::Value::as_str)
            {
                theatre.settled.lock_recover().push(id.to_string());
                theatre.raise(&format!("settled:{id}"));
            }
            let previous = input.previous;
            Box::pin(async move { Ok(previous) })
        },
    )
}

/// How one `oracle.step` leaf behaves, decoded from the arguments the cell
/// wrote. Every knob the FIG-3397 cases need is here: block until released,
/// report started, emit an intent, reject, and fail during preparation.
#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StepArgs {
    id: String,
    /// Reject instead of returning a value.
    #[serde(default)]
    fail: bool,
    /// Park on a completion key and publish it, so the test settles this leaf
    /// from outside the batch.
    #[serde(default)]
    defer: bool,
    /// Block inside the attempt until the test releases this leaf by name.
    #[serde(default)]
    hold: bool,
    #[serde(default)]
    intent: bool,
    /// Declare an intent the runtime refuses — an event type the target never
    /// registered — so the call returns a value and still settles rejected.
    #[serde(default)]
    refused_intent: bool,
    /// Fail in `prepare_tool_call`, before the batch dispatches anything.
    #[serde(default)]
    prepare_fail: bool,
}

impl StepArgs {
    fn decode(args: &serde_json::Value) -> Self {
        serde_json::from_value(args.clone()).unwrap_or_else(|error| {
            panic!("oracle.step arguments must decode, got {args}: {error}")
        })
    }
}

/// The oracle's one leaf tool.
///
/// One tool, not several, because the compile-time list-batch path keys a
/// whole aggregate on a single operation: a harness whose leaves are different
/// tools could not express that shape at all.
struct OracleTools {
    theatre: Arc<OracleTheatre>,
    session_id: String,
}

fn step_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:oracle_step",
        "oracle_step",
        "One aggregate leaf whose settlement the test controls.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": { "type": "string" },
                "fail": { "type": "boolean" },
                "defer": { "type": "boolean" },
                "hold": { "type": "boolean" },
                "intent": { "type": "boolean" },
                "refused_intent": { "type": "boolean" },
                "prepare_fail": { "type": "boolean" }
            },
            "required": ["id"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(lash_lashlang_runtime::ToolBinding::new(["oracle"], "step"))
}

#[async_trait]
impl ToolProvider for OracleTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![step_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "oracle_step").then(|| Arc::new(step_definition().contract()))
    }

    /// Every step call may park, so the runtime pre-derives the completion key
    /// a `defer` leaf publishes. A call that does not park is unaffected.
    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == step_definition().id()
    }

    /// A leaf that asks to fail here settles during the batch's preparation
    /// pass, before any leaf is dispatched — the preparation prefix.
    async fn prepare_tool_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> std::result::Result<lash_core::PreparedToolCall, lash_core::ToolOutcome> {
        let args = StepArgs::decode(&call.pending.args);
        if args.prepare_fail {
            return Err(lash_core::ToolOutcome::failure(
                lash_core::ToolFailure::runtime(
                    lash_core::ToolFailureClass::Internal,
                    "oracle_prepare_failed",
                    format!("step {} refused in preparation", args.id),
                ),
            ));
        }
        Ok(lash_core::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let args = StepArgs::decode(call.args);
        self.theatre.started.lock_recover().push(args.id.clone());
        self.theatre.raise(&format!("started:{}", args.id));

        if args.defer {
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(error) => return lash_core::ToolOutcome::err_fmt(error).into(),
            };
            self.theatre.publish_key(&args.id, key);
            return lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into();
        }
        if args.hold {
            assert!(
                self.theatre.wait(&format!("release:{}", args.id)).await,
                "leaf {} was never released",
                args.id
            );
        }

        if args.fail {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::runtime(
                lash_core::ToolFailureClass::Internal,
                "oracle_step_failed",
                format!("step {} rejected", args.id),
            ))
            .into();
        }
        let value = serde_json::json!({ "id": args.id });
        if !args.intent && !args.refused_intent {
            return lash_core::ToolOutcome::ok(value).into();
        }
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(value),
            lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::EmitProcessEvent(
                lash_core::EmitProcessEventIntent {
                    session_id: SessionId::from(self.session_id.clone()),
                    process_id: self.theatre.intent_process(),
                    event_type: if args.refused_intent {
                        UNREGISTERED_EVENT.to_string()
                    } else {
                        INTENT_EVENT.to_string()
                    },
                    payload: serde_json::json!({ "id": args.id }),
                },
            )]),
        )
    }
}

// ---------------------------------------------------------------------------
// One aggregate case, run end to end
// ---------------------------------------------------------------------------

/// What one oracle run observed.
struct OracleRun {
    /// The value the turn finished with, if it finished with one.
    final_value: Option<serde_json::Value>,
    /// Every request the scripted provider was asked, serialized. The cell's
    /// own refusals reach the model here and nowhere else.
    requests: Vec<String>,
    theatre: Arc<OracleTheatre>,
    registry: Arc<dyn ProcessRegistry>,
    tool_calls: usize,
}

impl OracleRun {
    fn final_value(&self) -> &serde_json::Value {
        self.final_value
            .as_ref()
            .expect("the scripted cells must finish the turn")
    }
}

/// Builds a core whose provider replays `cells` and whose only leaf tool is
/// the oracle's, over `backend`.
fn oracle_core(
    backend: lash_core::Backend,
    session_id: &str,
    cells: Vec<String>,
    theatre: Arc<OracleTheatre>,
    requests: Arc<StdMutex<Vec<String>>>,
) -> Result<LashCore> {
    oracle_builder(backend, session_id, cells, theatre, requests)
        .build(crate::testing::runtime_lease_owner())
}

/// [`oracle_core`]'s builder, for a case that swaps one of its parts.
fn oracle_builder(
    backend: lash_core::Backend,
    session_id: &str,
    cells: Vec<String>,
    theatre: Arc<OracleTheatre>,
    requests: Arc<StdMutex<Vec<String>>>,
) -> crate::core::LashCoreBuilder {
    let scripted = Arc::new(TokioMutex::new(VecDeque::from(cells)));
    let provider = crate::testing::TestProvider::builder()
        .kind("aggregate-oracle")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            let scripted = Arc::clone(&scripted);
            async move {
                requests.lock_recover().push(
                    serde_json::to_string(&request.messages).expect("serialize request messages"),
                );
                let text = scripted
                    .lock()
                    .await
                    .pop_front()
                    .unwrap_or_else(|| typescript_block(r#"finish("out of cells");"#));
                Ok(text_response(&text))
            }
        })
        .build()
        .into_handle();
    explicit_ephemeral_facets(rlm_core_builder_over(backend))
        .provider(provider)
        .model(mock_model_spec())
        .plugins(lash_core::facade_support::PluginStack::from_factories([Arc::new(
            StaticPluginFactory::new(
                "aggregate-oracle",
                lash_core::facade_support::PluginSpec::new()
                    .with_presentation_step(oracle_presentation_step(Arc::clone(&theatre))),
            ),
        ) as Arc<dyn PluginFactory>]))
        .tools(Arc::new(OracleTools {
            theatre,
            session_id: session_id.to_string(),
        }))
        // ADR 0095: `processes` is catalogue presence, so a cell that authors
        // `processes.start` or `processes.await` needs this factory installed.
        .plugin(Arc::new(
            lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(),
        ))
        .plugin(lash_core::testing::process_engine_plugin_fixture())
}

/// The process a declared intent is realized against. Registered up front with
/// the event type the leaf emits, because an emission against an unregistered
/// event type is refused and the case would pass for the wrong reason.
async fn register_intent_target(
    registry: &dyn ProcessRegistry,
    session_id: &str,
    theatre: &OracleTheatre,
) {
    let intent_process = registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_extra_event_types(vec![lash_core::ProcessEventType {
                name: INTENT_EVENT.to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SessionId::from(session_id.to_string())],
        )
        .await
        .expect("register the intent target process")
        .id;
    theatre
        .intent_process
        .set(intent_process)
        .expect("one intent target per oracle run");
}

/// One oracle session whose turn runs on its own task.
///
/// A case that settles a parked leaf has to act while the batch is in flight,
/// which it cannot do from the task that is awaiting the turn.
struct DrivenOracle {
    core: LashCore,
    theatre: Arc<OracleTheatre>,
    registry: Arc<dyn ProcessRegistry>,
    requests: Arc<StdMutex<Vec<String>>>,
    turn: tokio::task::JoinHandle<Result<TurnReport>>,
}

impl DrivenOracle {
    /// Awaits the turn and collects what the run observed.
    async fn finish(self) -> Result<OracleRun> {
        self.finish_within(RENDEZVOUS_BUDGET).await
    }

    /// [`Self::finish`] for a turn that legitimately runs longer than a
    /// rendezvous, such as a loop of hundreds of journaled races. `budget`
    /// is still only a deadlock budget.
    async fn finish_within(self, budget: std::time::Duration) -> Result<OracleRun> {
        let result = tokio::time::timeout(budget, self.turn)
            .await
            .expect("the driven turn must report")
            .expect("turn task")?;
        let tool_calls = self.theatre.completed_calls.load(Ordering::SeqCst);
        let requests = self.requests.lock_recover().clone();
        Ok(OracleRun {
            final_value: result.final_value().cloned(),
            requests,
            theatre: self.theatre,
            registry: self.registry,
            tool_calls,
        })
    }
}

async fn drive_cells(
    tier: &JournaledTier,
    session_id: &str,
    cells: Vec<String>,
) -> Result<DrivenOracle> {
    let theatre = Arc::new(OracleTheatre::default());
    let backend = tier.backend().await;
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    register_intent_target(registry.as_ref(), session_id, &theatre).await;
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let core = oracle_core(
        backend.into(),
        session_id,
        cells,
        Arc::clone(&theatre),
        Arc::clone(&requests),
    )?;
    let session = core.session(session_id).open().await?;
    let streamed = Arc::clone(&theatre);
    let turn = tokio::spawn(async move {
        session
            .turn(TurnInput::text("settle the aggregate"))
            .stream_to(streamed.as_ref())
            .await
    });
    Ok(DrivenOracle {
        core,
        theatre,
        registry,
        requests,
        turn,
    })
}

async fn run_cell(tier: &JournaledTier, session_id: &str, cell: &str) -> Result<OracleRun> {
    run_cells(tier, session_id, vec![typescript_block(cell)]).await
}

async fn run_cells(
    tier: &JournaledTier,
    session_id: &str,
    cells: Vec<String>,
) -> Result<OracleRun> {
    drive_cells(tier, session_id, cells).await?.finish().await
}

/// The cell body every rejection case shares: run the aggregate, finish with
/// the reason if it rejects. A cell that let the rejection escape would make
/// the RLM driver ask the provider for another cell instead, and the reason
/// under test would never be asserted on.
fn catching_cell(aggregate: &str) -> String {
    format!(
        r#"try {{
  finish({{ resolved: await {aggregate} }});
}} catch (error) {{
  finish({{ reason: error.message }});
}}"#
    )
}

fn reason(run: &OracleRun) -> String {
    run.final_value()
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| panic!("the aggregate must reject, got {}", run.final_value()))
        .to_string()
}

// ---------------------------------------------------------------------------
// Shapes: what the host is asked to do for each spelling of one aggregate
// ---------------------------------------------------------------------------

/// `Promise.all` and `Promise.allSettled` over every operand spelling the
/// dialect admits, asserted on what the host was asked to do rather than only
/// on the value that came back.
///
/// The spellings are in one case because they are one law: acceptance is a
/// runtime question asked of the operand's *value*, so a name, a `map`, a
/// duplicate element and a plain value cannot take different paths. A
/// zero-tool aggregate is in the same table because the interesting fact about
/// it is the absence of a host call.
async fn aggregate_shapes_ask_the_host_for_exactly_their_leaves(
    tier: &JournaledTier,
) -> Result<()> {
    for (label, cell, expected, expected_calls) in [
        (
            "a literal array",
            r#"finish(await Promise.all([oracle.step({ id: "a" }), oracle.step({ id: "b" })]));"#
                .to_string(),
            serde_json::json!([{ "id": "a" }, { "id": "b" }]),
            vec!["a", "b"],
        ),
        (
            "an array bound to a name",
            r#"const leaves = [oracle.step({ id: "a" }), oracle.step({ id: "b" })];
finish(await Promise.all(leaves));"#
                .to_string(),
            serde_json::json!([{ "id": "a" }, { "id": "b" }]),
            vec!["a", "b"],
        ),
        (
            "a mapped array",
            r#"const ids = ["a", "b"];
finish(await Promise.all(ids.map((id) => oracle.step({ id: id }))));"#
                .to_string(),
            serde_json::json!([{ "id": "a" }, { "id": "b" }]),
            vec!["a", "b"],
        ),
        (
            "an async-mapped array",
            r#"const ids = ["a", "b"];
finish(await Promise.all(ids.map(async (id) => await oracle.step({ id: id }))));"#
                .to_string(),
            serde_json::json!([{ "id": "a" }, { "id": "b" }]),
            vec!["a", "b"],
        ),
        (
            "duplicate leaves",
            r#"finish(await Promise.all([oracle.step({ id: "a" }), oracle.step({ id: "a" })]));"#
                .to_string(),
            serde_json::json!([{ "id": "a" }, { "id": "a" }]),
            vec!["a", "a"],
        ),
        (
            "plain values beside a leaf",
            r#"finish(await Promise.all([7, oracle.step({ id: "a" }), "plain"]));"#.to_string(),
            serde_json::json!([7, { "id": "a" }, "plain"]),
            vec!["a"],
        ),
        (
            "a zero-tool aggregate",
            r#"finish(await Promise.all([1, 2, 3]));"#.to_string(),
            serde_json::json!([1, 2, 3]),
            vec![],
        ),
        (
            "an empty aggregate",
            r#"finish(await Promise.all([]));"#.to_string(),
            serde_json::json!([]),
            vec![],
        ),
    ] {
        let run = run_cell(tier, "aggregate-oracle-shapes", &cell).await?;
        assert_eq!(
            run.final_value(),
            &expected,
            "{}/{label}: the aggregate value",
            tier.name
        );
        // A journaled tier's group children are spawned tasks whose start
        // order is the scheduler's (FIG-3637): the law is about *which*
        // leaves the host was asked for, not the order it was asked in.
        // Comparing sorted keeps the "exactly these leaves" guarantee —
        // multiplicities included.
        let mut started = run.theatre.started();
        started.sort_unstable();
        let mut expected_calls = expected_calls;
        expected_calls.sort_unstable();
        assert_eq!(
            started, expected_calls,
            "{}/{label}: the calls the host was asked for",
            tier.name
        );
        assert_eq!(
            run.tool_calls,
            expected_calls.len(),
            "{}/{label}: one completed tool call per leaf",
            tier.name
        );
    }
    Ok(())
}

/// `allSettled` reports every leaf as a record, in **input** order, whichever
/// leaf settled first. The rejecting leaf is written first and settles last, so
/// an implementation that reported settlement order would swap them.
async fn all_settled_reports_every_leaf_in_input_order(tier: &JournaledTier) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-all-settled",
        vec![typescript_block(
            r#"finish(await Promise.allSettled([
  oracle.step({ id: "late", hold: true, fail: true }),
  oracle.step({ id: "early", defer: true })
]));"#,
        )],
    )
    .await?;

    driven
        .theatre
        .settle_deferred(
            &driven.core,
            "early",
            lash_core::Resolution::Ok(serde_json::json!({ "id": "early" })),
        )
        .await?;
    driven.theatre.await_settled("early").await;
    driven.theatre.release("late");
    let run = driven.finish().await?;

    assert_eq!(
        run.theatre.settled(),
        vec!["early", "late"],
        "{}: leaf 1 settled first",
        tier.name
    );
    let items = run
        .final_value()
        .as_array()
        .unwrap_or_else(|| panic!("allSettled returns an array, got {}", run.final_value()));
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["status"], serde_json::json!("rejected"));
    assert!(
        items[0]["reason"]
            .to_string()
            .contains("step late rejected"),
        "{}: leaf 0 keeps its input position: {}",
        tier.name,
        items[0]
    );
    assert_eq!(
        items[1],
        serde_json::json!({ "status": "fulfilled", "value": { "id": "early" } }),
        "{}: leaf 1 keeps its input position",
        tier.name
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Selection: which rejection a rejected aggregate reports
// ---------------------------------------------------------------------------

/// The decisive case of the arc, asked of the whole runtime rather than of a
/// host that hands the VM a settlement order it made up.
///
/// The two leaves are written in the same order in both rows; only which one
/// parks changes. Row one settles leaf 1 first, so an implementation reporting
/// the input-order rejection fails it; row two settles leaf 0 first, so an
/// implementation reporting the reverse fails that one. Neither passes both.
///
/// The order is decided by construction, not by timing. The held leaf blocks
/// inside its attempt until this test releases it, and the parked leaf is
/// resolved — and its completion accepted — before that release, so the held
/// leaf cannot settle first whatever the scheduler does.
async fn promise_all_reports_the_first_settled_rejection(tier: &JournaledTier) -> Result<()> {
    for (label, first, second) in [
        ("leaf 1 settles first", "q", "p"),
        ("leaf 0 settles first", "p", "q"),
    ] {
        let cell = format!(
            r#"Promise.all([
  oracle.step({{ id: "p", fail: true, {p_mode} }}),
  oracle.step({{ id: "q", fail: true, {q_mode} }})
])"#,
            p_mode = if first == "p" {
                "defer: true"
            } else {
                "hold: true"
            },
            q_mode = if first == "q" {
                "defer: true"
            } else {
                "hold: true"
            },
        );
        let driven = drive_cells(
            tier,
            "aggregate-oracle-first-settled",
            vec![typescript_block(&catching_cell(&cell))],
        )
        .await?;

        driven
            .theatre
            .settle_deferred(&driven.core, first, OracleTheatre::rejection(first))
            .await?;
        driven.theatre.await_settled(first).await;
        // The held leaf cannot commit, so the first consumed settlement is
        // the parked leaf's: its rank precedes the held leaf's by construction.
        driven.theatre.await_consumed(1).await;
        driven.theatre.release(second);
        let run = driven.finish().await?;

        // `Promise.all` resumes at its first consumed rejection, so the held
        // leaf is a loser the turn's end may cancel before it ever settles.
        assert_eq!(
            run.theatre.settled().first().map(String::as_str),
            Some(first),
            "{}/{label}: the case decided the settlement order",
            tier.name
        );
        assert!(
            reason(&run).contains(&format!("step {first} rejected")),
            "{}/{label}: the first-settled rejection is the one reported, got {}",
            tier.name,
            reason(&run)
        );
        assert_eq!(
            run.theatre.started().len(),
            2,
            "{}/{label}: both leaves were dispatched",
            tier.name
        );
    }
    Ok(())
}

/// A terminal leaf settles ahead of a held source-earlier leaf.
///
/// Under ADR 0099 §5 the batch is a durable effect group: a child commits its
/// final record and settles in commit order, not source order, so the held
/// source-first leaf no longer blocks a later sibling's terminal. The second
/// leaf's rejection is therefore the settlement the aggregate's first-settled
/// selection reports.
///
/// This is the inversion of the old head-of-line law: the pre-group batch
/// path serialized terminal settlement behind each source-earlier leaf's
/// intent-drain slot, and this same cell reported `step first rejected`.
async fn a_terminal_leaf_settles_ahead_of_a_held_source_first_leaf(
    tier: &JournaledTier,
) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-head-of-line",
        vec![typescript_block(&catching_cell(
            r#"Promise.all([
  oracle.step({ id: "first", hold: true, fail: true }),
  oracle.step({ id: "second", fail: true })
])"#,
        ))],
    )
    .await?;

    // A batch whose group never dispatched fails the turn instead of parking
    // on a leaf; report that outcome rather than timing out on the rendezvous.
    let mut probe = tokio::time::interval(std::time::Duration::from_millis(50));
    let deadline = tokio::time::Instant::now() + RENDEZVOUS_BUDGET;
    while tokio::time::Instant::now() < deadline {
        probe.tick().await;
        if driven.theatre.started().len() == 2 {
            break;
        }
        if driven.turn.is_finished() {
            let run = driven.finish().await?;
            panic!(
                "{}: the turn finished with no leaf ever started; started: {:?}, \
                 final value: {:?}, provider requests: {:?}",
                tier.name,
                run.theatre.started(),
                run.final_value(),
                run.requests
            );
        }
    }

    // Both leaves' attempts have run; the held source-first leaf no longer
    // blocks the later terminal leaf's settlement.
    driven.theatre.await_started("second").await;
    driven.theatre.await_started("first").await;
    driven.theatre.await_settled("second").await;
    assert_eq!(
        driven.theatre.settled(),
        vec!["second"],
        "{}: a terminal leaf settles ahead of an unfinished source-earlier leaf, saw {:?}",
        tier.name,
        driven.theatre.settled()
    );
    // Consumed, not merely presented: the later leaf's rank is fixed before
    // the held leaf can commit.
    driven.theatre.await_consumed(1).await;

    // `Promise.all` resumed at that first consumed rejection (ADR 0099 §10
    // L2); the held source-first leaf is a loser the turn's end cancels.
    driven.theatre.release("first");
    let run = driven.finish().await?;

    assert_eq!(
        run.theatre.settled().first().map(String::as_str),
        Some("second"),
        "{}: settlement order is the group's commit order, not source order",
        tier.name
    );
    assert!(
        reason(&run).contains("step second rejected"),
        "{}: the first-settled rejection is the reported one, got {}",
        tier.name,
        reason(&run)
    );
    Ok(())
}

/// A leaf that fails in `prepare_tool_call` settles before any leaf is
/// dispatched, so it leads the batch's settlement order however it is written
/// — the preparation prefix.
///
/// The preparation failure is written *last* and the dispatched leaf rejects
/// on its own, so neither input order nor "whichever rejected first among the
/// leaves that ran" produces this answer.
async fn a_preparation_failure_leads_the_settlement_order(tier: &JournaledTier) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-preparation-prefix",
        &catching_cell(
            r#"Promise.all([
  oracle.step({ id: "dispatched", fail: true }),
  oracle.step({ id: "refused", prepare_fail: true })
])"#,
        ),
    )
    .await?;

    assert!(
        reason(&run).contains("step refused refused in preparation"),
        "{}: the preparation prefix leads the order, got {}",
        tier.name,
        reason(&run)
    );
    // The dispatched sibling is admitted before the prefix answers (§11
    // clause 3), but whether its attempt enters the host before the caught
    // rejection finishes the turn is the journaled group's scheduling, not
    // this law.
    assert!(
        !run.theatre.started().contains(&"refused".to_string()),
        "{}: a call refused in preparation never reaches the host",
        tier.name
    );
    Ok(())
}

/// ADR 0062 deviation 15 retired (FIG-3397): a rejected `Promise.all` resumes
/// at the first rejection its consumer takes (ADR 0099 §10 L2), and a sibling
/// still in flight is a loser that runs on under the opener.
///
/// The witness needs no clock. One leaf rejects; the other is held inside its
/// own attempt by a release only this test can give. The catch clause calls
/// another leaf: that leaf starting while the held one has not settled is the
/// resume the deviation said v1 could not make.
async fn a_rejected_promise_all_resumes_at_its_first_consumed_rejection(
    tier: &JournaledTier,
) -> Result<()> {
    let driven = drive_cells(
        tier,
        "aggregate-oracle-deviation-15",
        vec![typescript_block(
            r#"try {
  await Promise.all([
    oracle.step({ id: "rejecting", defer: true }),
    oracle.step({ id: "held", hold: true })
  ]);
  finish({ resolved: true });
} catch (error) {
  await oracle.step({ id: "after-rejection" });
  finish({ reason: error.message });
}"#,
        )],
    )
    .await?;

    driven
        .theatre
        .settle_deferred(
            &driven.core,
            "rejecting",
            OracleTheatre::rejection("rejecting"),
        )
        .await?;
    driven.theatre.await_started("after-rejection").await;
    assert!(
        !driven.theatre.settled().contains(&"held".to_string()),
        "{}: the aggregate resumed while the held leaf was still in flight, saw {:?}",
        tier.name,
        driven.theatre.settled()
    );

    driven.theatre.release("held");
    let run = driven.finish().await?;
    assert!(
        reason(&run).contains("step rejecting rejected"),
        "{}: the first consumed rejection is the reported one, got {}",
        tier.name,
        reason(&run)
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Leaves that are not ordinary tool calls
// ---------------------------------------------------------------------------

/// `processes.await` leaves aggregate like any other leaf: two started
/// processes, one aggregate, both durable waits settled.
async fn an_aggregate_of_durable_waits_settles_both(tier: &JournaledTier) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-process-awaits",
        r#"const worker = async (input: unknown) => { return input; };
const left = await processes.start({ definition: worker, args: { input: "left" } });
const right = await processes.start({ definition: worker, args: { input: "right" } });
finish(await Promise.all([
  processes.await({ handle: left }),
  processes.await({ handle: right })
]));"#,
    )
    .await?;

    assert_eq!(
        run.final_value(),
        &serde_json::json!(["left", "right"]),
        "{}: each durable wait yields its process's value, in input order",
        tier.name
    );
    Ok(())
}

/// A bare process handle written into an aggregate is refused, and the refusal
/// names the tool that parks on the durable wait instead.
///
/// The refusal is a VM-level one a cell cannot catch, so the RLM driver asks
/// the provider for another cell — which is where the diagnostic is observed.
async fn a_pending_handle_in_an_aggregate_is_refused(tier: &JournaledTier) -> Result<()> {
    let run = run_cells(
        tier,
        "aggregate-oracle-pending-handle",
        vec![
            typescript_block(
                r#"const worker = async (input: unknown) => { return input; };
const handle = await processes.start({ definition: worker, args: { input: "only" } });
finish(await Promise.all([handle]));"#,
            ),
            typescript_block(r#"finish("observed the refusal");"#),
        ],
    )
    .await?;

    assert_eq!(
        run.final_value(),
        &serde_json::json!("observed the refusal"),
        "{}: the refused cell is followed by a second cell",
        tier.name
    );
    assert!(
        run.requests.len() >= 2,
        "{}: the refusal must reach the model",
        tier.name
    );
    assert!(
        run.requests[1].contains("processes.await"),
        "{}: the refusal names the tool that parks on the durable wait: {}",
        tier.name,
        run.requests[1]
    );
    Ok(())
}

/// An aggregate whose operand is not an array is refused whatever it was
/// spelled as, and nothing reaches the host.
async fn an_aggregate_over_a_non_array_is_refused(tier: &JournaledTier) -> Result<()> {
    let run = run_cells(
        tier,
        "aggregate-oracle-non-array",
        vec![
            typescript_block(
                r#"const operand = 3;
finish(await Promise.all(operand));"#,
            ),
            typescript_block(r#"finish("observed the refusal");"#),
        ],
    )
    .await?;

    assert!(
        run.theatre.started().is_empty(),
        "{}: a refused aggregate reaches no leaf",
        tier.name
    );
    assert!(
        run.requests.len() >= 2 && run.requests[1].contains("Promise aggregate requires an array"),
        "{}: the refusal names the operand rule",
        tier.name
    );
    Ok(())
}

/// A leaf may declare an intent, and a leaf that settled inside a successful
/// aggregate has its declaration realized.
///
/// FIG-3397 asks the opposite question of the same capability — whether a
/// *loser*'s declaration is realized — so the capability lives in the harness
/// and this case pins the answer the winner-side already has.
async fn an_aggregate_leafs_declared_intent_is_realized(tier: &JournaledTier) -> Result<()> {
    let run = run_cell(
        tier,
        "aggregate-oracle-intents",
        r#"finish(await Promise.all([
  oracle.step({ id: "emitting", intent: true }),
  oracle.step({ id: "quiet" })
]));"#,
    )
    .await?;

    assert_eq!(
        run.final_value(),
        &serde_json::json!([{ "id": "emitting" }, { "id": "quiet" }])
    );
    let events = run
        .registry
        .recent_events(&run.theatre.intent_process(), 16)
        .await
        .expect("read the intent target's events");
    let emitted = events
        .iter()
        .filter(|event| event.event_type == INTENT_EVENT)
        .collect::<Vec<_>>();
    assert_eq!(
        emitted.len(),
        1,
        "{}: exactly the declaring leaf's intent is realized, saw {:?}",
        tier.name,
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    assert_eq!(emitted[0].payload, serde_json::json!({ "id": "emitting" }));
    Ok(())
}

/// §10 L3: an aggregate's infrastructure failure is the host-control channel.
#[path = "aggregate_oracle/host_control.rs"]
mod host_control;
/// §9: completed groups retire under a live opener.
#[path = "aggregate_oracle/opener_bound.rs"]
mod opener_bound;
/// `Promise.race` and `Promise.any` on the product path (FIG-3397).
#[path = "aggregate_oracle/race_any.rs"]
mod race_any;
/// §11 clause 4: every timer aggregate is its own group.
#[path = "aggregate_oracle/timer_identity.rs"]
mod timer_identity;

// ---------------------------------------------------------------------------
// SQLite registration of the case list
// ---------------------------------------------------------------------------

fn sqlite() -> JournaledTier {
    JournaledTier::sqlite()
}

#[tokio::test]
async fn sqlite_aggregate_shapes_ask_the_host_for_exactly_their_leaves() -> Result<()> {
    aggregate_shapes_ask_the_host_for_exactly_their_leaves(&sqlite()).await
}

#[tokio::test]
async fn sqlite_all_settled_reports_every_leaf_in_input_order() -> Result<()> {
    all_settled_reports_every_leaf_in_input_order(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_promise_all_reports_the_first_settled_rejection() -> Result<()> {
    promise_all_reports_the_first_settled_rejection(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_terminal_leaf_settles_ahead_of_a_held_source_first_leaf() -> Result<()> {
    a_terminal_leaf_settles_ahead_of_a_held_source_first_leaf(&sqlite()).await
}

#[tokio::test]
async fn sqlite_a_preparation_failure_leads_the_settlement_order() -> Result<()> {
    a_preparation_failure_leads_the_settlement_order(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_rejected_promise_all_resumes_at_its_first_consumed_rejection() -> Result<()> {
    a_rejected_promise_all_resumes_at_its_first_consumed_rejection(&sqlite()).await
}

#[tokio::test]
async fn sqlite_an_aggregate_of_durable_waits_settles_both() -> Result<()> {
    an_aggregate_of_durable_waits_settles_both(&sqlite()).await
}

#[tokio::test]
async fn sqlite_a_pending_handle_in_an_aggregate_is_refused() -> Result<()> {
    a_pending_handle_in_an_aggregate_is_refused(&sqlite()).await
}

#[tokio::test]
async fn sqlite_an_aggregate_over_a_non_array_is_refused() -> Result<()> {
    an_aggregate_over_a_non_array_is_refused(&sqlite()).await
}

#[tokio::test]
async fn sqlite_an_aggregate_leafs_declared_intent_is_realized() -> Result<()> {
    an_aggregate_leafs_declared_intent_is_realized(&sqlite()).await
}

// ---------------------------------------------------------------------------
// The compile-time aggregate paths, stated at the IR
// ---------------------------------------------------------------------------
//
// `Instruction::ResourceOperationBatch` and
// `Instruction::ResourceOperationListBatch` are formed by the compiler from
// `Expr::Await` over a list or a comprehension. ADR 0096 retired the dialect
// that spelled those, so neither is reachable from an authored cell and
// neither can be stated through the bridge above. They are stated here, at the
// IR, because FIG-3397 changes both and a landing that only re-points the
// bridge cases would move these silently.
//
// The two paths disagreed before FIG-3397: the literal-array batch selected by
// the reported settlement order and the list-comprehension batch in written
// order. The ruling puts both on the written-order rule (ADR 0099 §10 L7).

/// A host that records every batch it is handed and answers each leaf with a
/// rejection. It runs its leaves in reverse, so a selection that followed the
/// order leaves ran in would report leaf 1; the Lashlang-native aggregates ask
/// it for every result and report in written order (ADR 0099 §10 L7).
struct AllResultsHost {
    /// The batch sizes the VM asked for, in order.
    batches: StdMutex<Vec<usize>>,
    /// The consumer mode each batch asked for.
    consumers: StdMutex<Vec<lashlang::AggregateConsumer>>,
    calls: AtomicUsize,
}

impl AllResultsHost {
    fn new() -> Self {
        Self {
            batches: StdMutex::new(Vec::new()),
            consumers: StdMutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }
}

impl lashlang::ExecutionHost for AllResultsHost {
    async fn perform(
        &self,
        op: lashlang::AbilityOp,
    ) -> std::result::Result<lashlang::AbilityResult, lashlang::ExecutionHostError> {
        match op {
            lashlang::AbilityOp::ResourceOperationBatch(batch) => {
                self.batches.lock_recover().push(batch.leaves.len());
                self.consumers.lock_recover().push(batch.consumer);
                self.calls.fetch_add(batch.leaves.len(), Ordering::SeqCst);
                let mut results = vec![None; batch.leaves.len()];
                for index in (0..batch.leaves.len()).rev() {
                    results[index] = Some(lashlang::ResourceOperationResult::Error(
                        lashlang::ExecutionHostError::new(format!("leaf-{index} rejected")),
                    ));
                }
                Ok(lashlang::AbilityResult::ResourceOperationBatch(
                    lashlang::ResourceOperationBatchResult::AllResults(
                        results.into_iter().flatten().collect(),
                    ),
                ))
            }
            lashlang::AbilityOp::Finish(value) => Ok(lashlang::AbilityResult::Value(value)),
            other => Err(lashlang::ExecutionHostError::new(format!(
                "unexpected ability {other:?}"
            ))),
        }
    }
}

/// `await [tools.step(0)?, tools.step(1)?]` — the literal-array batch path.
fn literal_array_batch_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    let leaf = |id: f64| {
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::num(id)],
        ))
    };
    b::program(vec![b::finish(b::await_expr(b::list(vec![
        leaf(0.0),
        leaf(1.0),
    ])))])
}

/// The literal-array compile-time batch is a Lashlang-native aggregate: it
/// asks the host for every result and reports the **first written** rejection
/// (ADR 0099 §10 L7). Before FIG-3397 it selected the first settled one; the
/// ruling puts the dialect's own aggregates on one input-order rule, and only
/// the TypeScript `Promise.*` aggregates carry an ECMA consumer mode.
#[tokio::test]
async fn a_literal_array_batch_reports_the_first_written_rejection() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&literal_array_batch_program())
        .expect("compile the literal batch");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the literal array forms one batch of two leaves"
    );
    assert_eq!(
        host.consumers.lock_recover().as_slice(),
        [lashlang::AggregateConsumer::AllSettled],
        "a Lashlang-native aggregate asks for every result"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the written-first rejection is the reported one: {rendered}"
    );
    assert!(
        !rendered.contains("leaf-1 rejected"),
        "the leaf the host ran first is not the reported one: {rendered}"
    );
}

/// `await [tools.step(id)? for id in [0, 1]]` — the standalone list-batch path.
fn list_batch_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    b::program(vec![b::finish(b::await_expr(b::comprehension(
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::var("id")],
        )),
        vec![b::comprehension_for(
            "id",
            b::list(vec![b::num(0.0), b::num(1.0)]),
        )],
    )))])
}

/// The standalone list-batch path keeps its all-results wait and reports the
/// **first written** rejection — ruled explicitly by FIG-3397 (ADR 0099 §10
/// L7): the VM asks the host for every result and selects in written order.
#[tokio::test]
async fn the_standalone_list_batch_still_selects_the_first_written_rejection() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&list_batch_program())
        .expect("compile the list batch");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the comprehension forms one batch of two leaves"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the written-first rejection is the reported one: {rendered}"
    );
    assert!(
        !rendered.contains("leaf-1 rejected"),
        "the leaf the host ran first is not consulted on this path: {rendered}"
    );
    assert_eq!(
        host.consumers.lock_recover().as_slice(),
        [lashlang::AggregateConsumer::AllSettled],
        "the list batch asks for every result"
    );
}

/// `await [[tools.step(y)? for y in ys] for x in xs]` — a comprehension whose
/// element is itself a comprehension.
fn nested_comprehension_program() -> lashlang::Program {
    use lashlang::testing::ast_builders as b;
    let inner = b::comprehension(
        b::unwrap(b::receiver_call(
            b::resource(&["tools"]),
            "step",
            vec![b::var("inner")],
        )),
        vec![b::comprehension_for(
            "inner",
            b::list(vec![b::num(0.0), b::num(1.0)]),
        )],
    );
    b::program(vec![b::finish(b::await_expr(b::comprehension(
        inner,
        vec![b::comprehension_for("outer", b::list(vec![b::num(0.0)]))],
    )))])
}

/// The nested-comprehension law: the whole nest is one batch, and it keeps the
/// Lashlang-native written rejection order (ADR 0099 §10 L7).
///
/// One host batch of two leaves, and the written-first rejection is the one
/// reported, even though the host ran the other leaf first.
#[tokio::test]
async fn a_nested_comprehension_is_one_batch_that_keeps_written_rejection_order() {
    let host = AllResultsHost::new();
    let compiled = lashlang::testing::harness::try_compile_program(&nested_comprehension_program())
        .expect("compile the nested shape");
    let error = lashlang::execute(&compiled, &mut lashlang::State::new(), &host)
        .await
        .expect_err("both leaves reject, so the aggregate rejects");

    assert_eq!(
        host.batches.lock_recover().as_slice(),
        [2],
        "the nest is one batch, not one batch per inner comprehension"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("leaf-0 rejected"),
        "the inner template keeps written order: {rendered}"
    );
}
