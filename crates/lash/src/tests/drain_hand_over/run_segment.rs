//! FIG-4739: a foreground run on a draining build ends its invocation at
//! its next quiet point and goes on in a new invocation on the newest build.
//!
//! Each law sends one input whose run asks the model, calls a tool, and
//! asks the model again. While the run's first model call is in flight on
//! build N, build N+1 registers and an operator marks N draining.
//!
//! - **hand-over**: the drain freezes the run's tool Run on N, so N refuses
//!   the tool round the first model call asked for (FIG-5075). The run's
//!   physical turn on N commits there, owing the round, and N's shift hands
//!   over. N+1 admits the continuation, which admits the round from the
//!   calls the committed history records and asks the model again: the tool
//!   runs once, the model is asked exactly twice, and the send that started
//!   the run on N is answered by the turn N+1 ran.
//! - **refused round**: the continuation runs the refused round before it
//!   asks the model, and answers the very call N's model call recorded.
//!
//! - **crash**: the hand-over law again, with one invocation dying once on
//!   either side of a durable record of the hand-over: N's run before its
//!   round's cut check is recorded and after its boundary commit, N's shift
//!   before it sends the shift on, and N+1's continuation before its first
//!   step and after its final commit. The run ends the same: the tool ran
//!   once and the model was asked twice.
//! - **counts**: N holds the run while its first invocation runs there, and
//!   N+1 holds it from the admission of its continuation: N's drain completes
//!   at the hand-over, while the run is still running.
//! - **cancel before**: a cancel that lands while the run is still on N ends
//!   it there, and no continuation is owed.
//! - **cancel after**: a cancel that lands after the hand-over reaches the
//!   continuation on N+1 and ends the run.
//! - **journal budget**: on one build whose run invocations end after one
//!   effect, the run takes a boundary at every quiet point, and each
//!   continuation is a new `LashTurn` invocation with a journal of its own.
//!   Every tool call still runs once and the run's answer is the last
//!   continuation's.
//!
//! - **turn budget**: the same run bounded to two model calls stops for its
//!   budget after two, however many invocations ran them.
//!
//! These run on the Restate server double over SQLite memory, SQLite file
//! and PostgreSQL. The PostgreSQL legs are ignored in ordinary runs and
//! require `LASH_POSTGRES_DATABASE_URL`.

use super::*;

use std::sync::atomic::AtomicUsize;

/// The run's model: each of its first `tool_rounds` calls asks for the
/// tool, and the call after them answers with how many tool results the
/// request carried. A call numbered in `held` waits for the law to release
/// it first.
struct RunModel {
    tool_rounds: usize,
    held: &'static [usize],
    requests: std::sync::Mutex<Vec<LlmRequest>>,
    reached: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

fn tool_results(request: &LlmRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
        .count()
}

fn run_provider(model: &Arc<RunModel>) -> ProviderHandle {
    let model = Arc::clone(model);
    crate::testing::TestProvider::builder()
        .kind("run-segment")
        .complete(move |request| {
            let model = Arc::clone(&model);
            async move {
                let call = {
                    let mut requests = model.requests.lock_recover();
                    requests.push(request.clone());
                    requests.len()
                };
                if model.held.contains(&call) {
                    model.reached.notify_one();
                    model.release.notified().await;
                }
                if call <= model.tool_rounds {
                    return Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: format!("lookup-{call}"),
                            tool_name: "app_lookup".to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    });
                }
                Ok(text_response(&format!(
                    "answered after {} tool result(s)",
                    tool_results(&request)
                )))
            }
        })
        .build()
        .into_handle()
}

/// The run's tool, counting its executions.
struct CountedLookup {
    executed: Arc<AtomicUsize>,
}

struct DeferredLookup {
    executed: Arc<AtomicUsize>,
    key: Arc<std::sync::Mutex<Option<lash_core::AwaitEventKey>>>,
    dispatched: Arc<tokio::sync::Notify>,
}

fn deferred_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(lash_core::ToolDefinition::raw("tool:app_lookup", "app_lookup", "Look up app state.", serde_json::json!({"type":"object","properties":{"slot":{"type":"integer"}},"additionalProperties":false}), serde_json::json!({"type":"object"})).expect("tool schemas"), "app_lookup")
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

#[async_trait]
impl ToolProvider for DeferredLookup {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![deferred_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(deferred_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        if let Some(slot) = call.args.get("slot").and_then(serde_json::Value::as_u64)
            && slot != 1
        {
            return lash_core::ToolOutcome::ok(serde_json::json!({"slot":slot})).into();
        }
        *self.key.lock_recover() = Some(call.context.completion_key().expect("a durable key"));
        self.dispatched.notify_one();
        lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new()).into()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_tools_run_hands_over_before_its_external_result() -> Result<()> {
    deferred_round_law(DeferredCase::Resolve).await
}

/// FIG-5059: an operator drain from a backend that holds no core, as
/// `lashctl drain` runs it, hands over a Run parked on its source wait at
/// once: no recovery pass runs after the drain, yet the Run cuts without
/// asking the model or running its tool again, and its continuation is
/// admitted on the newest build.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_operator_drain_hands_over_a_run_parked_on_its_source_wait() -> Result<()> {
    deferred_round_law(DeferredCase::OperatorDrain).await
}

#[derive(Clone, Copy, Debug)]
enum DeferredCase {
    Resolve,
    /// The drain is the operator's, from a backend that holds no core, and
    /// no recovery pass follows it.
    OperatorDrain,
    Mixed,
    HeldPending,
    Cancel,
    DispatchBefore,
    DispatchAfter,
    TransferBefore,
    TransferAfter,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mixed_deferred_round_folds_results_in_source_order() -> Result<()> {
    deferred_round_law(DeferredCase::Mixed).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deferred_source_stays_pending_across_elapsed_time_and_handover() -> Result<()> {
    deferred_round_law(DeferredCase::HeldPending).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_reaches_a_deferred_round_after_handover() -> Result<()> {
    deferred_round_law(DeferredCase::Cancel).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_dispatch_and_ownership_transfer_recover_on_both_sides() -> Result<()> {
    for case in [
        DeferredCase::DispatchBefore,
        DeferredCase::DispatchAfter,
        DeferredCase::TransferBefore,
        DeferredCase::TransferAfter,
    ] {
        deferred_round_law(case).await?;
    }
    Ok(())
}

async fn deferred_round_law(case: DeferredCase) -> Result<()> {
    let World { engine, _keep, .. } = double_world(Storage::SqliteMemory).await;
    let model = Arc::new(RunModel {
        tool_rounds: 1,
        held: &[],
        requests: std::sync::Mutex::default(),
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let provider = if matches!(case, DeferredCase::Mixed) {
        let model = Arc::clone(&model);
        crate::testing::TestProvider::builder()
            .kind("run-segment")
            .complete(move |request| {
                let model = Arc::clone(&model);
                async move {
                    let call = {
                        let mut requests = model.requests.lock_recover();
                        requests.push(request.clone());
                        requests.len()
                    };
                    if call == 1 {
                        return Ok(LlmResponse {
                            parts: (0..3)
                                .map(|slot| LlmOutputPart::ToolCall {
                                    call_id: format!("lookup-{slot}"),
                                    tool_name: "app_lookup".to_owned(),
                                    input_json: serde_json::json!({"slot":slot}).to_string(),
                                    replay: None,
                                })
                                .collect(),
                            ..Default::default()
                        });
                    }
                    Ok(text_response(&format!(
                        "answered after {} tool result(s)",
                        tool_results(&request)
                    )))
                }
            })
            .build()
            .into_handle()
    } else {
        run_provider(&model)
    };
    let Engine::Double(double) = &engine else {
        unreachable!("the law uses the double")
    };
    let crashes = lash_restate_test::CrashCount::new();
    assert!(double.server().on_crash(crashes.listener()));
    use lash_restate_test::{CrashPoint, CrashRule};
    if matches!(
        case,
        DeferredCase::DispatchBefore | DeferredCase::DispatchAfter
    ) {
        double.server().crash_on(
            CrashRule::new(if matches!(case, DeferredCase::DispatchBefore) {
                CrashPoint::BeforeCommand { index: 1 }
            } else {
                CrashPoint::BeforeFrame {
                    ty: lash_restate_test::protocol::MessageType::OutputCommand,
                }
            })
            .handler("child"),
        );
    }
    let executed = Arc::new(AtomicUsize::new(0));
    let key = Arc::new(std::sync::Mutex::new(None));
    let dispatched = Arc::new(tokio::sync::Notify::new());
    let old = engine
        .old_backend()
        .build_generation()
        .expect("a build")
        .clone();
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(engine.old_backend())
        .with_session_work(engine.old_work())
        .into_backend();
    let core = LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::new(DeferredLookup {
            executed: Arc::clone(&executed),
            key: Arc::clone(&key),
            dispatched: Arc::clone(&dispatched),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    let session_id = lash_core::SessionId::fixture(format!("deferred-run-{case:?}"));
    let handle = core
        .session(session_id.clone())
        .created()
        .await
        .open()
        .await?
        .send(TurnInput::text("wait for the external result"))
        .id(crate::TurnId::parse("run-run").expect("nonblank host identity"))
        .await?;
    tokio::time::timeout(WEDGE, dispatched.notified())
        .await
        .expect("the tool deferred");
    let parked = parked_deferred_run(double.server(), &session_id, "run-run").await;
    if matches!(case, DeferredCase::TransferBefore) {
        assert!(double.server().crash(&parked.id));
        parked_deferred_run(double.server(), &session_id, "run-run").await;
    }
    if matches!(case, DeferredCase::TransferAfter) {
        double.server().crash_on(
            CrashRule::new(CrashPoint::BeforeFrame {
                ty: lash_restate_test::protocol::MessageType::OutputCommand,
            })
            .service(lash_restate_test::TURN_DRIVER_SERVICE)
            .handler("run")
            .key(
                lash_restate::recorded_turn_invocation_key(
                    core.store_factory.as_ref(),
                    &session_id,
                    &lash_core::TurnId::fixture("run-run"),
                )
                .await?
                .expect("the parked run records its invocation"),
            ),
        );
    }
    if matches!(case, DeferredCase::HeldPending) {
        double.server().advance(std::time::Duration::from_secs(900));
    }
    let next = BuildGeneration::for_test("deferred-run-next");
    engine
        .roll(next.clone(), &Arc::new(Model::holding(0)))
        .await;
    let operator_drain = matches!(case, DeferredCase::OperatorDrain);
    if operator_drain {
        assert!(crate::drain_generation(&engine.old_backend(), &old).await?);
    } else {
        engine
            .old_backend()
            .generation_drain()
            .mark_draining(&old, 1)
            .await?;
    }
    let deadline = tokio::time::Instant::now() + WEDGE;
    loop {
        if !operator_drain {
            core._session_shifts
                .reconcile(
                    &lash_core::engine::ReconcileCursor::default(),
                    std::num::NonZeroUsize::new(16).expect("a page"),
                )
                .await?;
        }
        let drain = engine.old_backend().generation_drain();
        if drain.generation_work(&old).await?.in_flight_turns == 0
            && drain.generation_work(&next).await?.in_flight_turns == 1
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the deferred tool pins its Run on the draining build: old={:?}, next={:?}, pending={:#?}",
            drain.generation_work(&old).await?,
            drain.generation_work(&next).await?,
            double
                .server()
                .invocations()
                .into_iter()
                .filter(|view| view.status != "completed")
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        model.requests.lock_recover().len(),
        1,
        "no model request while waiting"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        if matches!(case, DeferredCase::Mixed) {
            3
        } else {
            1
        },
        "handover never redispatches the tool"
    );
    if operator_drain {
        // The operator's drain cut the Run and N+1 holds its continuation;
        // what the continuation does next is the other cases' to pin.
        drop(core);
        engine.finish().await;
        return Ok(());
    }
    let completion = key.lock_recover().clone().expect("the original key");
    parked_deferred_run(
        double.server(),
        &session_id,
        "follow-on:run-run:agent-frame:1#0",
    )
    .await;
    if matches!(case, DeferredCase::HeldPending) {
        double
            .server()
            .advance(std::time::Duration::from_secs(2701));
        double.server().settle().await;
        assert_eq!(
            model.requests.lock_recover().len(),
            1,
            "elapsed time cannot close the pending call"
        );
        assert_eq!(
            executed.load(Ordering::SeqCst),
            1,
            "elapsed time cannot reroute or redeliver settled X"
        );
    }
    match case {
        DeferredCase::Cancel => {
            handle.cancel().origin("deferred-round-law").await?;
        }
        _ => {
            core.completions()
                .resolve(
                    completion.clone(),
                    lash_core::Resolution::Ok(serde_json::json!({ "slot": 1 })),
                )
                .await?;
        }
    }
    let output = tokio::time::timeout(WEDGE, handle.output())
        .await
        .unwrap_or_else(|error| {
            let pending = double.server().invocations().into_iter().filter(|view| view.status != "completed").collect::<Vec<_>>();
            let journals = pending.iter().filter(|view| view.target.contains(lash_restate_test::TURN_DRIVER_SERVICE)).map(|view| {
                let entries = double.server().journal(&view.id).unwrap_or_default();
                let tail = entries.into_iter().rev().take(12).map(|entry| (entry.ty, entry.name.clone(), entry.call_command().map(|call| (call.service_name, call.handler_name)), entry.run_completion().map(|result| result.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())))).collect::<Vec<_>>();
                (view.target.clone(), tail)
            }).collect::<Vec<_>>();
            panic!("the Run completes: {error:?}; model requests={}, pending={pending:#?}, journals={journals:#?}", model.requests.lock_recover().len());
        })?;
    if matches!(case, DeferredCase::Cancel) {
        assert!(matches!(
            output.result.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::Cancelled { .. })
        ));
        assert_eq!(model.requests.lock_recover().len(), 1);
    } else {
        let expected = if matches!(case, DeferredCase::Mixed) {
            3
        } else {
            1
        };
        assert_eq!(
            output.result.outcome,
            TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
                text: format!("answered after {expected} tool result(s)")
            }),
            "the real results remain correlated in the resumed prompt: {:#?}",
            model.requests.lock_recover()
        );
        let requests = model.requests.lock_recover();
        assert_eq!(requests.len(), 2);
        assert_eq!(tool_results(&requests[1]), expected);
        let results = requests[1]
            .messages
            .iter()
            .flat_map(|message| message.blocks.iter())
            .filter_map(|block| match block {
                LlmContentBlock::ToolResult {
                    call_id, content, ..
                } => Some((
                    call_id.clone(),
                    serde_json::to_string(content).expect("model return"),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        if matches!(case, DeferredCase::Mixed) {
            assert_eq!(
                results
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .collect::<Vec<_>>(),
                ["lookup-0", "lookup-1", "lookup-2"]
            );
        }
    }
    assert_eq!(
        executed.load(Ordering::SeqCst),
        if matches!(case, DeferredCase::Mixed) {
            3
        } else {
            1
        }
    );
    if matches!(
        case,
        DeferredCase::DispatchBefore
            | DeferredCase::DispatchAfter
            | DeferredCase::TransferBefore
            | DeferredCase::TransferAfter
    ) {
        assert_eq!(crashes.get(), 1, "the requested crash actually ran");
    }
    let late = core
        .completions()
        .resolve(
            completion,
            lash_core::Resolution::Ok(serde_json::json!({"late":true})),
        )
        .await?;
    assert!(
        matches!(
            late,
            lash_core::ResolveOutcome::AlreadyResolved { .. }
                | lash_core::ResolveOutcome::UnknownOrRevoked
        ),
        "a late result cannot replace the terminal: {late:?}"
    );
    drop(core);
    engine.finish().await;
    Ok(())
}

#[async_trait]
impl ToolProvider for CountedLookup {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

fn run_core(
    backend: lash_core::Backend,
    work: Arc<dyn lash_core::SessionWorkEngine>,
    model: &Arc<RunModel>,
    executed: &Arc<AtomicUsize>,
) -> LashCore {
    let backend = lash_core::testing::runtime_helpers::LayeredBackend::over(backend)
        .with_session_work(work)
        .into_backend();
    LashCore::standard_builder(backend)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(
            crate::QueuedWorkBatchingConfig::new(1024).with_max_turn_input_admission(1),
        )
        .serve_test_llm_profile(run_provider(model), mock_llm_profile_spec())
        .tools(Arc::new(CountedLookup {
            executed: Arc::clone(executed),
        }))
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core")
}

/// A run mid-roll on the double: its first model call is held on build N,
/// build N+1 is registered, and N is marked draining.
struct RunRoll {
    engine: Engine,
    core: LashCore,
    model: Arc<RunModel>,
    executed: Arc<AtomicUsize>,
    session: lash_core::SessionId,
    handle: Option<crate::SendHandle>,
    old: BuildGeneration,
    next: BuildGeneration,
    _keep: Keep,
}

impl RunRoll {
    /// Send the run's input to `session` on N and roll while its first
    /// model call is in flight. The model holds its calls numbered in
    /// `held`, which names the first.
    async fn start(storage: Storage, session: &str, held: &'static [usize]) -> Result<Self> {
        let World { engine, _keep, .. } = double_world(storage).await;
        let model = Arc::new(RunModel {
            tool_rounds: 1,
            held,
            requests: std::sync::Mutex::default(),
            reached: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let executed = Arc::new(AtomicUsize::new(0));
        let old = engine
            .old_backend()
            .build_generation()
            .expect("the engine's generation is bound")
            .clone();
        let core = run_core(engine.old_backend(), engine.old_work(), &model, &executed);
        let session = lash_core::SessionId::fixture(session);
        let handle = core
            .session(session.clone())
            .created()
            .await
            .open()
            .await?
            .send(TurnInput::text("look it up, then answer"))
            .id(crate::TurnId::parse("run-run").expect("nonblank host identity"))
            .await?;
        tokio::time::timeout(WEDGE, model.reached.notified())
            .await
            .expect("the run reaches its first model call");
        let next = BuildGeneration::for_test("run-segment-next");
        engine
            .roll(next.clone(), &Arc::new(Model::holding(0)))
            .await;
        assert!(
            engine
                .old_backend()
                .generation_drain()
                .mark_draining(&old, 1)
                .await
                .expect("mark build N draining"),
            "the law's mark is N's first"
        );
        Ok(Self {
            engine,
            core,
            model,
            executed,
            session,
            handle: Some(handle),
            old,
            next,
            _keep,
        })
    }

    /// The run's send handle, to await its answer.
    fn sent(&mut self) -> crate::SendHandle {
        self.handle.take().expect("the run's send handle")
    }

    /// The builds the run's executes ran on.
    async fn builds(&self) -> std::collections::BTreeSet<String> {
        self.engine
            .shifts(&self.session)
            .await
            .into_iter()
            .filter_map(|row| row.pinned_deployment_id)
            .collect()
    }

    /// The turns `generation` holds in flight.
    async fn in_flight(&self, generation: &BuildGeneration) -> u64 {
        self.engine
            .old_backend()
            .generation_drain()
            .generation_work(generation)
            .await
            .expect("read the generation's work")
            .in_flight_turns
    }

    /// The continuation's admission is a plugin adoption point (FIG-4747):
    /// the decision that admitted it on N+1 journaled the plugin composition
    /// and writer formats it runs under, the record a run's own admission
    /// keeps. One core serves both builds here, so the two records agree.
    async fn assert_continuation_recorded_its_plugins(&self) -> Result<()> {
        let Engine::Double(double) = &self.engine else {
            unreachable!("the law runs on the double");
        };
        let server = double.server();
        let continuation = server
            .turn_invocations(
                &self.session,
                &lash_core::TurnId::fixture("follow-on:run-run:agent-frame:1#0"),
            )
            .into_iter()
            .next()
            .expect("the continuation's run invocation");
        fn decision(value: &serde_json::Value) -> Option<&serde_json::Value> {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.get("decision").and_then(|d| d.as_str()) == Some("run") {
                        return Some(value);
                    }
                    fields.values().find_map(decision)
                }
                serde_json::Value::Array(items) => items.iter().find_map(decision),
                _ => None,
            }
        }
        let recorded = server
            .journal(&continuation.id)
            .expect("the continuation's journal")
            .iter()
            .filter_map(|entry| entry.run_completion()?.ok())
            .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .find_map(|value| decision(&value).and_then(|run| run.get("plugins").cloned()))
            .expect("the continuation's admission journaled its plugins");
        let recorded: lash_core::store::plugin_writers::PluginAdmission =
            serde_json::from_value(recorded)?;
        assert!(
            !recorded.plugins().is_empty(),
            "the record names the admitting build's composition"
        );
        let store = lash_core::runtime::live_session_view(&self.core.store_factory, &self.session)
            .await?
            .expect("an opened session has a store");
        let fence = lash_core::testing::store_fixtures::seal_shift_fence_for_test(
            store.store(),
            &self.session,
            "run-segment-plugins",
        )
        .await;
        let run = lash_core::TurnId::from("run-run");
        let mut request = lash_core::testing::store_fixtures::admit_run_request_for_test(
            &fence,
            &run,
            lash_core::store::AdmittedHead::Input(lash_core::InputId::from("recorded")),
        );
        request.executor = store
            .store()
            .run_executor(&self.session, &run)
            .await?
            .expect("the run retains its admitting invocation");
        let admitted = store
            .admit_run(&request)
            .await?
            .expect("the run's admission is recorded");
        assert_eq!(
            recorded, admitted.plugins,
            "the continuation records the composition and writers a run's admission does"
        );
        Ok(())
    }

    /// The run left nothing behind: the head owes no continuation, and the
    /// draining build holds none of it.
    async fn assert_ended(&self) -> Result<()> {
        let store = lash_core::runtime::live_session_view(&self.core.store_factory, &self.session)
            .await?
            .expect("an opened session has a store");
        assert!(
            store.load_pending_follow_on().await?.is_none(),
            "the run's last commit left nothing owed"
        );
        let work = self
            .engine
            .old_backend()
            .generation_drain()
            .generation_work(&self.old)
            .await
            .expect("read N's work");
        assert_eq!(
            (work.in_flight_turns, work.parked_turns),
            (0, 0),
            "the draining build holds none of the run"
        );
        assert!(
            self.core
                .generation_drain_status(&self.old)
                .await?
                .drained(),
            "N's drain is complete"
        );
        Ok(())
    }
}

/// Where an invocation of the hand-over dies, once: on each side of every
/// durable record the hand-over writes.
#[derive(Clone, Copy, Debug)]
enum Crash {
    /// N's run dies with its tool round's admission unrecorded, before its
    /// boundary commit: its replay checks the drain's cut again.
    OldRunBeforeItsCutCheckIsRecorded,
    /// N's run dies after its boundary commit, with its outcome unrecorded:
    /// its replay ends at the boundary its journal recorded and answers the
    /// commit's receipt.
    OldRunAfterItsBoundaryCommit,
    /// N's shift dies after the admission that recorded the drain and before
    /// it sends the rest of the shift on.
    OldDriveBeforeItHandsOver,
    /// N+1's continuation run dies before its first step is recorded.
    ContinuationBeforeItsFirstStep,
    /// N+1's continuation run dies after its final commit, with its outcome
    /// unrecorded.
    ContinuationAfterItsCommit,
}

impl Crash {
    fn rule(self, session: &str) -> lash_restate_test::CrashRule {
        use lash_restate_test::protocol::MessageType;
        use lash_restate_test::{CrashPoint, CrashRule, TURN_DRIVER_SERVICE};
        let run_execution = |point| {
            CrashRule::new(point)
                .service(TURN_DRIVER_SERVICE)
                .handler("run")
        };
        let outcome = |run: &str| CrashPoint::BeforeStateWrite {
            key: "outcome".to_owned(),
            value_contains: Some(format!("\"run\":\"{run}\"")),
        };
        let continuation = "follow-on:run-run:agent-frame:1#0";
        match self {
            Self::OldRunBeforeItsCutCheckIsRecorded => {
                run_execution(CrashPoint::BeforeRunResultEnding {
                    suffix: ":admit".to_owned(),
                })
            }
            Self::OldRunAfterItsBoundaryCommit => run_execution(outcome("run-run")),
            Self::OldDriveBeforeItHandsOver => CrashRule::new(CrashPoint::BeforeFrame {
                ty: MessageType::OneWayCallCommand,
            })
            .service(SESSION_SHIFT_SERVICE)
            .handler("shift")
            .key(session),
            Self::ContinuationBeforeItsFirstStep => {
                run_execution(CrashPoint::BeforeRunResultStarting {
                    prefix: "lash:shift-run-start:".to_owned(),
                })
            }
            Self::ContinuationAfterItsCommit => run_execution(outcome(continuation)),
        }
    }
}

async fn a_run_on_a_draining_build_goes_on_in_a_new_invocation(
    storage: Storage,
    crash: Option<Crash>,
) -> Result<()> {
    let session = match crash {
        Some(crash) => format!("run-segment-hand-over-{crash:?}"),
        None => "run-segment-hand-over".to_owned(),
    };
    let mut roll = RunRoll::start(storage, &session, &[1]).await?;
    let crashes = lash_restate_test::CrashCount::new();
    if let Some(crash) = crash {
        let Engine::Double(double) = &roll.engine else {
            unreachable!("the law runs on the double");
        };
        double.server().crash_on(crash.rule(&session));
        assert!(
            double.server().on_crash(crashes.listener()),
            "the law's crash listener is the engine's only one"
        );
    }
    roll.model.release.notify_one();

    // The send that started on N is answered by the turn N+1 ran.
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
            text: "answered after 1 tool result(s)".to_owned(),
        }),
        "the run's answer is its continuation's"
    );

    // No effect ran twice: one tool call, and one model call on each side
    // of the boundary, the second asked from the history the boundary
    // committed and the round the continuation admitted.
    assert_eq!(roll.executed.load(Ordering::SeqCst), 1, "the tool ran once");
    let requests = roll.model.requests.lock_recover().clone();
    assert_eq!(requests.len(), 2, "the model was asked once per invocation");
    assert!(
        request_text(&requests[1]).contains("look it up, then answer"),
        "the continuation runs under the run's input: {:?}",
        requests[1].messages
    );
    assert_eq!(tool_results(&requests[1]), 1);
    assert_eq!(
        roll.builds().await.len(),
        2,
        "the run's shift moved to the newest build"
    );
    if crash.is_some() {
        assert_eq!(crashes.get(), 1, "the invocation died once");
    }
    roll.assert_continuation_recorded_its_plugins().await?;
    roll.assert_ended().await
}

/// The old generation drains at the hand-over, not at the run's end: while
/// the run's first invocation is on N, N holds the run; once N+1 has admitted
/// its continuation, N+1 holds it and N's drain is complete, though the run
/// is still running.
async fn a_run_counts_in_the_generation_that_is_running_it(storage: Storage) -> Result<()> {
    let mut roll = RunRoll::start(storage, "run-segment-counts", &[1, 2]).await?;
    assert_eq!(
        (
            roll.in_flight(&roll.old).await,
            roll.in_flight(&roll.next).await
        ),
        (1, 0),
        "the run's first invocation counts on the draining build"
    );
    assert!(
        !roll
            .core
            .generation_drain_status(&roll.old)
            .await?
            .drained(),
        "the drain waits for the invocation running on its build"
    );

    roll.model.release.notify_one();
    tokio::time::timeout(WEDGE, roll.model.reached.notified())
        .await
        .expect("the continuation reaches its model call on the newest build");
    assert_eq!(
        (
            roll.in_flight(&roll.old).await,
            roll.in_flight(&roll.next).await
        ),
        (0, 1),
        "the run counts in the generation that admitted its continuation"
    );
    assert!(
        roll.core
            .generation_drain_status(&roll.old)
            .await?
            .drained(),
        "N's drain is complete while the run goes on on N+1"
    );

    roll.model.release.notify_one();
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the run ends")?;
    assert!(output.result.is_success(), "{:?}", output.result.outcome);
    assert_eq!(roll.in_flight(&roll.next).await, 0, "the run ended");
    roll.assert_ended().await
}

/// A cancel of the run that lands while its first invocation is still on
/// the draining build ends the run there: the turn honours it at the step
/// boundary it would otherwise have handed over at, and owes no
/// continuation.
async fn a_cancel_before_the_boundary_ends_the_run_on_the_draining_build(
    storage: Storage,
) -> Result<()> {
    let mut roll = RunRoll::start(storage, "run-segment-cancel-before", &[1]).await?;
    let receipt = roll
        .handle
        .as_ref()
        .expect("the run's send handle")
        .cancel()
        .mode(lash_core::facade_support::TurnCancelMode::AfterStep)
        .origin("run-segment-law")
        .await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { run, .. } if run.as_str() == "run-run"),
        "the cancel reaches the running run: {receipt:?}"
    );
    roll.model.release.notify_one();
    let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
        .await
        .expect("the cancelled run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        roll.model.requests.lock_recover().len(),
        1,
        "the cancelled run never asked the model again"
    );
    roll.assert_ended().await
}

/// A cancel of the run that lands after the hand-over reaches the run's
/// continuation on the newest build: the cancellation is the logical run's,
/// whichever invocation is running it.
async fn a_cancel_after_the_hand_over_reaches_the_continuation(storage: Storage) -> Result<()> {
    let mut roll = RunRoll::start(storage, "run-segment-cancel-after", &[1, 2]).await?;
    roll.model.release.notify_one();
    tokio::time::timeout(WEDGE, roll.model.reached.notified())
        .await
        .expect("the continuation reaches its model call on the newest build");
    assert_eq!(
        roll.builds().await.len(),
        2,
        "the run's shift moved to the newest build"
    );
    let receipt = roll
        .handle
        .as_ref()
        .expect("the run's send handle")
        .cancel()
        .origin("run-segment-law")
        .await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { run, .. } if run.as_str() == "run-run"),
        "the cancel reaches the running run: {receipt:?}"
    );
    let outcome = tokio::time::timeout(WEDGE, roll.sent().outcome())
        .await
        .expect("the cancelled run answers")?;
    assert_eq!(outcome.status(), crate::TurnStatus::Cancelled);
    assert_eq!(
        outcome.run().map(lash_core::TurnId::as_str),
        Some("run-run")
    );
    assert_eq!(roll.executed.load(Ordering::SeqCst), 1, "the tool ran once");
    assert_eq!(roll.model.requests.lock_recover().len(), 2);
    roll.assert_ended().await
}

/// FIG-5075: the drain freezes the Run's admission on N, so the tool round
/// the model asked for on N is refused there. The refusal is a hand-over,
/// not a failure: N's turn ends owing the round, and the continuation on
/// N+1 admits it from the calls N's model call recorded. The tool runs once,
/// before the continuation asks the model, and the model is never asked for
/// those calls again.
async fn a_tool_round_the_drain_refuses_hands_over_to_the_newest_build(
    storage: Storage,
) -> Result<()> {
    let mut roll = RunRoll::start(storage, "run-segment-refused-round", &[1, 2]).await?;
    roll.model.release.notify_one();
    tokio::time::timeout(WEDGE, roll.model.reached.notified())
        .await
        .expect("the continuation asks the model on the newest build");
    assert_eq!(
        roll.builds().await.len(),
        2,
        "the run's shift moved to the newest build"
    );
    assert_eq!(
        roll.executed.load(Ordering::SeqCst),
        1,
        "the continuation ran the refused round before asking the model"
    );
    let requests = roll.model.requests.lock_recover().clone();
    assert_eq!(
        requests.len(),
        2,
        "the model was not asked for the calls again"
    );
    let answered: Vec<_> = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        answered,
        ["lookup-1"],
        "the continuation answers the call N's model call recorded"
    );
    roll.model.release.notify_one();
    let output = tokio::time::timeout(WEDGE, roll.sent().output())
        .await
        .expect("the run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
            text: "answered after 1 tool result(s)".to_owned(),
        }),
    );
    assert_eq!(roll.executed.load(Ordering::SeqCst), 1, "the tool ran once");
    roll.assert_ended().await
}

/// The double over `storage` with one build, whose run invocations end
/// after `budget` effects; with `always_replay`, every attempt's input
/// closes after its replayed journal (`INACTIVITY_TIMEOUT=0s`).
pub(super) async fn budget_world(storage: Storage, budget: u64, always_replay: bool) -> World {
    let (opening, keep) = prepare(storage).await;
    let lever = DrainLever::default();
    let opened = lever.clone();
    let double = lash_restate_test::backend_with_store_set_and_segment_budget(
        SEED,
        lash_restate_test::ServerConfig::default().always_replay(always_replay),
        Some(budget),
        lash_restate_test::DeploymentHooks::default(),
        |clock| async move {
            open(opening, clock, opened)
                .await
                .map_err(lash_restate_test::BackendError::Stores)
        },
    )
    .await
    .expect("the Restate double over the law's stores");
    World {
        engine: Engine::Double(double),
        lever,
        always_replay,
        _keep: keep,
    }
}

/// One `LashTurn` run of the law's session, as the engine's
/// `sys_invocation` reports it.
#[derive(Debug, serde::Deserialize)]
struct RunRunRow {
    target_service_key: Option<String>,
}

async fn a_run_past_its_journal_budget_goes_on_in_a_new_invocation(
    storage: Storage,
    always_replay: bool,
) -> Result<()> {
    const TOOL_ROUNDS: usize = 3;
    let World { engine, _keep, .. } = budget_world(storage, 1, always_replay).await;
    let model = Arc::new(RunModel {
        tool_rounds: TOOL_ROUNDS,
        held: &[],
        requests: std::sync::Mutex::default(),
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let executed = Arc::new(AtomicUsize::new(0));
    let core = run_core(engine.old_backend(), engine.old_work(), &model, &executed);
    let session_id = lash_core::SessionId::from("run-segment-journal-budget");
    let handle = budget_session(&core, &session_id, crate::TurnBudget::Unbounded).await?;
    let output = tokio::time::timeout(
        WEDGE,
        handle
            .send(TurnInput::text("look it up three times, then answer"))
            .id(crate::TurnId::parse("run-run").expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the run ends");
    let output = output?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Finished(lash_core::facade_support::TurnFinish::AssistantMessage {
            text: format!("answered after {TOOL_ROUNDS} tool result(s)"),
        }),
        "the run's answer is its last continuation's"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        TOOL_ROUNDS,
        "every tool call ran once"
    );
    let requests = model.requests.lock_recover().clone();
    assert_eq!(
        requests.iter().map(tool_results).collect::<Vec<_>>(),
        (0..=TOOL_ROUNDS).collect::<Vec<_>>(),
        "each model call was asked once, from the history committed before it"
    );

    // With a one-effect budget, each tool round takes one boundary after
    // its model call and another after the successor's recorded material
    // restoration. The final invocation asks the model for the answer.
    let Engine::Double(double) = &engine else {
        unreachable!("the law runs on the double");
    };
    let runs = lash_restate::RestateAdminClient::new(double.connection())
        .query_json::<RunRunRow>(
            "SELECT target_service_key FROM sys_invocation \
             WHERE target_service_name = 'LashTurn' AND target_handler_name = 'run'",
        )
        .await
        .expect("sys_invocation query");
    let runs: std::collections::BTreeSet<_> = runs
        .iter()
        .filter_map(|row| row.target_service_key.as_ref())
        .filter(|key| {
            double
                .server()
                .object_state("LashTurn", key)
                .contains_key("admission")
        })
        .cloned()
        .collect();
    // Each round adds a durable restored-material read on its successor.
    // With the unchanged one-effect budget, those reads each add a boundary.
    assert_eq!(
        runs.len(),
        2 * TOOL_ROUNDS + 1,
        "the run went on in a new invocation at every quiet point: {runs:?}"
    );
    let store = lash_core::runtime::live_session_view(&core.store_factory, &session_id)
        .await?
        .expect("an opened session has a store");
    assert!(
        store.load_pending_follow_on().await?.is_none(),
        "the last continuation's commit left nothing owed"
    );
    let head = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the completed run has a head");
    let state = head
        .state
        .plugin_state()
        .expect("the head carries plugin namespaces");
    assert!(!state.plugins.is_empty());
    for namespace in state.plugins.values() {
        assert_eq!(
            u64::from(namespace.publication.owner_segment.0),
            head.state.turn_index as u64,
            "every production continuation adopts publication ownership before its hooks"
        );
    }
    drop(core);
    engine.finish().await;
    Ok(())
}

/// The run's turn budget counts its model calls across its boundaries: a
/// run bounded to two model calls stops for its budget after two, however
/// many invocations ran them.
async fn a_runs_turn_budget_counts_across_its_boundaries(storage: Storage) -> Result<()> {
    let World { engine, _keep, .. } = budget_world(storage, 1, false).await;
    let model = Arc::new(RunModel {
        tool_rounds: 3,
        held: &[],
        requests: std::sync::Mutex::default(),
        reached: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let executed = Arc::new(AtomicUsize::new(0));
    let core = run_core(engine.old_backend(), engine.old_work(), &model, &executed);
    let session_id = lash_core::SessionId::from("run-segment-turn-budget");
    let handle = budget_session(&core, &session_id, crate::TurnBudget::bounded(2)).await?;
    let output = tokio::time::timeout(
        WEDGE,
        handle
            .send(TurnInput::text("look it up three times, then answer"))
            .id(crate::TurnId::parse("run-run").expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the run ends")?;
    assert_eq!(
        output.result.outcome,
        TurnOutcome::Stopped(lash_core::facade_support::TurnStop::MaxTurns),
        "the run stops for its budget"
    );
    assert_eq!(
        model.requests.lock_recover().len(),
        2,
        "the budget bounds the run's model calls, not one invocation's"
    );
    drop(core);
    engine.finish().await;
    Ok(())
}

/// Open `session` on `core` under `turn_budget`.
async fn budget_session(
    core: &LashCore,
    session: &lash_core::SessionId,
    turn_budget: crate::TurnBudget,
) -> Result<crate::LashSession> {
    let metadata = mock_llm_profile_spec();
    core.session(session.clone())
        .created_with(crate::SessionSpec::new(
            metadata.wire_model.clone(),
            turn_budget,
            crate::MaxToolCalls::new(1024),
        ))
        .await
        .open()
        .await
}

async fn turn_budget(storage: Storage, (): ()) -> Result<()> {
    a_runs_turn_budget_counts_across_its_boundaries(storage).await
}

async fn journal_budget(storage: Storage, always_replay: bool) -> Result<()> {
    a_run_past_its_journal_budget_goes_on_in_a_new_invocation(storage, always_replay).await
}

async fn counts(storage: Storage, (): ()) -> Result<()> {
    a_run_counts_in_the_generation_that_is_running_it(storage).await
}

async fn cancel_before(storage: Storage, (): ()) -> Result<()> {
    a_cancel_before_the_boundary_ends_the_run_on_the_draining_build(storage).await
}

async fn cancel_after(storage: Storage, (): ()) -> Result<()> {
    a_cancel_after_the_hand_over_reaches_the_continuation(storage).await
}

async fn refused_round(storage: Storage, (): ()) -> Result<()> {
    a_tool_round_the_drain_refuses_hands_over_to_the_newest_build(storage).await
}

async fn hands_over(storage: Storage, crash: Option<Crash>) -> Result<()> {
    a_run_on_a_draining_build_goes_on_in_a_new_invocation(storage, crash).await
}

drain_hand_over_laws! {
    run_hands_over_sqlite_memory: hands_over, Storage::SqliteMemory, None;
    run_hands_over_sqlite_file: hands_over, Storage::SqliteFile, None;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_hands_over_postgres: hands_over, Storage::Postgres, None;
    run_crash_old_run_before_cut_check_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRunBeforeItsCutCheckIsRecorded);
    run_crash_old_run_before_cut_check_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRunBeforeItsCutCheckIsRecorded);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_crash_old_run_before_cut_check_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRunBeforeItsCutCheckIsRecorded);
    run_crash_old_run_after_boundary_commit_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldRunAfterItsBoundaryCommit);
    run_crash_old_run_after_boundary_commit_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldRunAfterItsBoundaryCommit);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_crash_old_run_after_boundary_commit_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldRunAfterItsBoundaryCommit);
    run_crash_old_drive_before_hand_over_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::OldDriveBeforeItHandsOver);
    run_crash_old_drive_before_hand_over_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::OldDriveBeforeItHandsOver);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_crash_old_drive_before_hand_over_postgres:
        hands_over, Storage::Postgres, Some(Crash::OldDriveBeforeItHandsOver);
    run_crash_continuation_before_first_step_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::ContinuationBeforeItsFirstStep);
    run_crash_continuation_before_first_step_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::ContinuationBeforeItsFirstStep);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_crash_continuation_before_first_step_postgres:
        hands_over, Storage::Postgres, Some(Crash::ContinuationBeforeItsFirstStep);
    run_crash_continuation_after_commit_sqlite_memory:
        hands_over, Storage::SqliteMemory, Some(Crash::ContinuationAfterItsCommit);
    run_crash_continuation_after_commit_sqlite_file:
        hands_over, Storage::SqliteFile, Some(Crash::ContinuationAfterItsCommit);
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_crash_continuation_after_commit_postgres:
        hands_over, Storage::Postgres, Some(Crash::ContinuationAfterItsCommit);
    run_counts_sqlite_memory: counts, Storage::SqliteMemory, ();
    run_counts_sqlite_file: counts, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_counts_postgres: counts, Storage::Postgres, ();
    run_cancel_before_sqlite_memory: cancel_before, Storage::SqliteMemory, ();
    run_cancel_before_sqlite_file: cancel_before, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_cancel_before_postgres: cancel_before, Storage::Postgres, ();
    run_cancel_after_sqlite_memory: cancel_after, Storage::SqliteMemory, ();
    run_cancel_after_sqlite_file: cancel_after, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_cancel_after_postgres: cancel_after, Storage::Postgres, ();
    run_refused_round_sqlite_memory: refused_round, Storage::SqliteMemory, ();
    run_refused_round_sqlite_file: refused_round, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_refused_round_postgres: refused_round, Storage::Postgres, ();
    run_journal_budget_sqlite_memory: journal_budget, Storage::SqliteMemory, false;
    run_journal_budget_sqlite_file: journal_budget, Storage::SqliteFile, false;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_journal_budget_postgres: journal_budget, Storage::Postgres, false;
    replay_run_journal_budget_sqlite_memory: journal_budget, Storage::SqliteMemory, true;
    run_turn_budget_sqlite_memory: turn_budget, Storage::SqliteMemory, ();
    run_turn_budget_sqlite_file: turn_budget, Storage::SqliteFile, ();
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
    run_turn_budget_postgres: turn_budget, Storage::Postgres, ();
}

async fn parked_deferred_run(
    server: &lash_restate_test::RestateTestServer,
    session: &lash_core::SessionId,
    run: &str,
) -> lash_restate_test::InvocationView {
    let deadline = tokio::time::Instant::now() + WEDGE;
    let mut seen = None;
    loop {
        if let Some(view) = server
            .turn_invocations(session, &lash_core::TurnId::fixture(run))
            .into_iter()
            .find(|view| view.status == "completed")
        {
            let records: Vec<_> = server
                .journal(&view.id)
                .unwrap()
                .iter()
                .filter_map(|entry| entry.run_completion())
                .map(|result| result.map(|bytes| String::from_utf8_lossy(&bytes).into_owned()))
                .collect();
            panic!(
                "the deferred Run ended before its real result: {:?}; records={records:#?}",
                server.outcome(&view.id)
            );
        }
        let view = server
            .turn_invocations(session, &lash_core::TurnId::fixture(run))
            .into_iter()
            .find(|view| view.status == "running" && view.blocked_on_server == Some(true));
        if let Some(view) = view {
            if seen == Some(view.journal_len) {
                return view;
            }
            seen = Some(view.journal_len);
        } else {
            seen = None;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the run never parked: {:#?}",
            server.invocations()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
