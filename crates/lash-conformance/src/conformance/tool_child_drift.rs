//! FIG-3725: a group tool child judges its own tool against the registry
//! serving it, and a drifted one is served only from the child's journal.
//!
//! A tool call runs as a child of a durable effect group: a model-issued call
//! of the standard protocol's turn, and each leaf of a code cell's aggregate.
//! Where the child runs, it judges the tool its opener recorded against the
//! live registry on what decides how a call links and dispatches (FIG-3587).
//! A drifted tool's dispatching effects are served only from the journal: a
//! recorded result is served, and an effect the engine would run live refuses
//! with `lashlang_cell_binding_drift`, recording nothing (FIG-3719). The
//! turn's first attempt is cut at a journal point the law names, and the
//! redrive runs under a registry that removed `probe` or changed its retry
//! policy:
//!
//! - cut after the calls settled, the redrive is served every recorded result
//!   and the turn finishes with nothing dispatched;
//! - cut before the calls were issued, each call is needed live: the turn
//!   parks with the binding drift and nothing is dispatched, and every
//!   further run of the refused child refuses and parks again. An engine
//!   whose opener can read the refusal answers the redrive with it; one whose
//!   child runs in its own invocation writes the park from the child, and the
//!   opener waits for the child.
//!
//! A reworded tool is not drift: its call needed live runs once and the turn
//! finishes.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};

use super::cell_binding_drift::{Probe, probe_factory};

/// A cell whose two `tools.probe` calls run as one aggregate's group
/// children.
const AGGREGATE_CELL: &str = "<typescript>\nconst [first, second] = await Promise.all([tools.probe({}), tools.probe({})]);\nfinish(first);\n</typescript>";

/// The call id the model gives its native call on `probe`.
const NATIVE_CALL: &str = "native-probe-1";

/// How long a law waits for a park an engine writes on its own.
const PARK_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Which group children call the drifted tool.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// The model calls `probe` natively: the standard protocol's turn tool
    /// group.
    Native,
    /// A code cell's `Promise.all` over two `tools.probe` calls.
    Aggregate,
}

#[derive(Clone)]
struct World {
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    executions: Arc<AtomicUsize>,
}

/// The model's answer: `probe` natively until a tool result is in, then
/// text; or the aggregate cell.
fn response(shape: Shape, request: &crate::LlmRequest) -> crate::LlmResponse {
    let answered = request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| matches!(block, crate::llm::types::LlmContentBlock::ToolResult { .. }));
    let part = match (shape, answered) {
        (Shape::Native, false) => crate::LlmOutputPart::ToolCall {
            call_id: NATIVE_CALL.to_string(),
            tool_name: "probe".to_string(),
            input_json: "{}".to_string(),
            replay: None,
        },
        (Shape::Native, true) => crate::LlmOutputPart::Text {
            text: "probed".to_string(),
            response_meta: None,
        },
        (Shape::Aggregate, _) => crate::LlmOutputPart::Text {
            text: AGGREGATE_CELL.to_string(),
            response_meta: None,
        },
    };
    crate::LlmResponse {
        parts: vec![part],
        ..crate::LlmResponse::default()
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(
    world: &World,
    shape: Shape,
    session_id: &SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
    probe: Probe,
) -> crate::LashRuntime {
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let response = response(shape, &request);
            async move { Ok(response) }
        })
        .build();
    let mut host =
        crate::LawBackend::over_stores(Arc::clone(&world.stores), Arc::clone(&world.effect_host))
            .host_config(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let protocol = match shape {
        Shape::Native => crate::testing::test_standard_protocol_factories(),
        Shape::Aggregate => world.rlm.clone(),
    };
    let factories = protocol
        .into_iter()
        .chain([probe_factory(probe, Arc::clone(&world.executions))])
        .collect::<Vec<_>>();
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    Box::pin(
        crate::LashRuntime::builder(host, crate::testing::runtime_lease_owner())
            .with_session_id(session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(factories)
            .with_store(store)
            .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
            .build(),
    )
    .await
    .expect("build the tool-child drift conformance runtime")
}

/// What an attempt answered, and how many tool executions had run when it
/// started: all the attempts before it made.
type Answer = (Result<crate::AssembledTurn, crate::RuntimeError>, usize);

/// One attempt at `session_id`'s turn under `probe`, reporting its answer on
/// `answers` when there is one: a cut attempt never answers.
fn attempt(
    world: &World,
    shape: Shape,
    session_id: &SessionId,
    turn_id: &TurnId,
    store: &Arc<dyn crate::RuntimePersistence>,
    probe: Probe,
    answers: Option<tokio::sync::mpsc::UnboundedSender<Answer>>,
) -> crate::ConformanceTurnAttempt {
    let world = world.clone();
    let session_id = session_id.clone();
    let turn_id = turn_id.clone();
    let store = Arc::clone(store);
    Arc::new(move |scope| {
        let world = world.clone();
        let session_id = session_id.clone();
        let turn_id = turn_id.clone();
        let store = Arc::clone(&store);
        let answers = answers.clone();
        Box::pin(async move {
            let started = world.executions.load(Ordering::SeqCst);
            let mut runtime = build_runtime(&world, shape, &session_id, store, probe).await;
            let mut input = crate::TurnInput::text("call the probe");
            input.trace_turn_id = Some(turn_id);
            let turn = runtime
                .stream_turn(
                    input,
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            if let Some(answers) = answers {
                let _ = answers.send((turn, started));
            }
            end
        })
    })
}

/// Where the law cuts the first attempt.
#[derive(Clone, Copy, Debug)]
enum Cut {
    /// After the calls settled: before the second model call (native) or
    /// before the cell's seal (aggregate).
    Settled,
    /// Before the calls were issued: before the first model call.
    Unissued,
}

/// The journal point `cut` names in `session_id`'s turn, found by running the
/// same turn to completion in a same-length probe session and reading the
/// replay keys the tier journaled for it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn cut_key(
    world: &World,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    shape: Shape,
    cut: Cut,
    probe_session: &SessionId,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> crate::JournalCut {
    assert_eq!(
        probe_session.as_str().len(),
        session_id.as_str().len(),
        "same-length session ids"
    );
    let store = crate::conformance::law_session_store(world.stores.as_ref(), probe_session).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let scope = crate::ExecutionScope::turn(probe_session, turn_id);
    runner
        .run_turn(
            admit(scope.clone()),
            attempt(
                world,
                shape,
                probe_session,
                turn_id,
                &store,
                Probe::Registered,
                Some(answers),
            ),
        )
        .await;
    answered
        .recv()
        .await
        .expect("the probe turn answered")
        .0
        .expect("the probe turn completes");
    let keys = runner
        .recorded_replay_keys(&scope)
        .await
        .expect("the tier reads the replay keys it journaled");
    let first_model_call = |key: &&String| key.contains(":0:llm_call:");
    let picked = match (shape, cut) {
        (_, Cut::Unissued) => keys.iter().find(first_model_call),
        (Shape::Native, Cut::Settled) => keys
            .iter()
            .find(|key| key.contains(":llm_call:") && !first_model_call(key)),
        (Shape::Aggregate, Cut::Settled) => keys.iter().find(|key| key.ends_with(":lk2:~seal")),
    };
    let replay_key = picked
        .unwrap_or_else(|| panic!("{shape:?}: no key for {cut:?} among {keys:#?}"))
        .replace(probe_session.as_str(), session_id.as_str());
    crate::JournalCut {
        replay_key,
        at: crate::JournalCutPoint::BeforeEffect,
    }
}

/// The session's park once `ready` holds for it, waiting up to [`PARK_WAIT`]
/// for an engine that writes it on its own.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn park_when(
    store: &Arc<dyn crate::RuntimePersistence>,
    session_id: &SessionId,
    ready: impl Fn(&crate::store::TurnPark) -> bool,
) -> crate::store::TurnPark {
    let waited = tokio::time::timeout(PARK_WAIT, async {
        loop {
            if let Some(park) = store
                .load_turn_park(session_id)
                .await
                .expect("read the park")
                .filter(|park| ready(park))
            {
                return park;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| panic!("`{session_id}` parked within {PARK_WAIT:?}"))
}

/// Law: a group tool child whose tool drifted is served its recorded result
/// and refuses, parking its turn, where it would run the tool live.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_group_tool_child_judges_its_own_drifted_tool(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let world = World {
        effect_host,
        stores,
        rlm,
        executions: Arc::new(AtomicUsize::new(0)),
    };
    let turn_id = TurnId::from(format!("{prefix}-child-drift-turn"));
    for (case, shape, cut, drift, word) in [
        ("nd-rem", Shape::Native, Cut::Settled, Probe::Removed, ""),
        ("nd-chg", Shape::Native, Cut::Settled, Probe::Retried, ""),
        ("ad-rem", Shape::Aggregate, Cut::Settled, Probe::Removed, ""),
        ("ad-chg", Shape::Aggregate, Cut::Settled, Probe::Retried, ""),
        (
            "nl-rem",
            Shape::Native,
            Cut::Unissued,
            Probe::Removed,
            "missing",
        ),
        (
            "nl-chg",
            Shape::Native,
            Cut::Unissued,
            Probe::Retried,
            "changed",
        ),
        (
            "al-chg",
            Shape::Aggregate,
            Cut::Unissued,
            Probe::Retried,
            "changed",
        ),
        (
            "al-rem",
            Shape::Aggregate,
            Cut::Unissued,
            Probe::Removed,
            "missing",
        ),
    ] {
        let session_id = SessionId::from(format!("{prefix}-{case}-real"));
        let probe_session = SessionId::from(format!("{prefix}-{case}-prob"));
        let journal_cut = cut_key(
            &world,
            &runner,
            shape,
            cut,
            &probe_session,
            &session_id,
            &turn_id,
        )
        .await;
        let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
        let admitted = admit(crate::ExecutionScope::turn(&session_id, &turn_id));
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
        let first = attempt(
            &world,
            shape,
            &session_id,
            &turn_id,
            &store,
            Probe::Registered,
            None,
        );
        let redrive = attempt(
            &world,
            shape,
            &session_id,
            &turn_id,
            &store,
            drift,
            Some(answers.clone()),
        );
        if matches!(cut, Cut::Settled) {
            let dispatched_before = world.executions.load(Ordering::SeqCst);
            runner
                .run_cut_then_redriven_turn(admitted, journal_cut, first, redrive)
                .await;
            let (turn, dispatched) = answered.recv().await.expect("the redrive answered");
            assert!(
                dispatched > dispatched_before,
                "{case}: the first attempt ran its calls live"
            );
            assert_eq!(
                world.executions.load(Ordering::SeqCst),
                dispatched,
                "{case}: the redrive is served every recorded result"
            );
            let turn =
                turn.unwrap_or_else(|error| panic!("{case}: the redrive completes: {error:?}"));
            assert!(
                matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
                "{case}: redriven outcome {:?}; errors {:?}",
                turn.outcome,
                turn.errors
            );
            assert!(
                store
                    .load_turn_park(&session_id)
                    .await
                    .expect("read the park")
                    .is_none(),
                "{case}: a redrive served from the journal leaves no park"
            );
            continue;
        }

        // Needed live: the first attempt issued nothing, so every call the
        // redrive makes would reach the drifted tool live.
        let dispatched = world.executions.load(Ordering::SeqCst);
        let mut redriving = tokio::spawn({
            let runner = Arc::clone(&runner);
            async move {
                runner
                    .run_cut_then_redriven_turn(admitted, journal_cut, first, redrive)
                    .await;
            }
        });
        let assert_drift_park = |park: &crate::store::TurnPark| {
            assert_eq!(
                park.turn_id, turn_id,
                "{case}: the park is keyed by the turn"
            );
            let crate::store::ParkReason::BindingDrift { message } = &park.reason else {
                panic!("{case}: the park names the binding drift: {park:?}");
            };
            assert!(
                message.contains("tool:probe") && message.contains(word),
                "{case}: the park names the tool and how it drifted: {message}"
            );
            if matches!(shape, Shape::Native) {
                assert!(
                    message.contains(NATIVE_CALL),
                    "{case}: the park names the call: {message}"
                );
            }
        };
        assert_drift_park(&park_when(&store, &session_id, |_| true).await);
        // An engine whose opener reads the child's refusal answers the
        // redrive with it, and the next redrive refuses again. One whose
        // child runs in its own invocation keeps its opener waiting on the
        // child, and re-runs the refused child, which parks again.
        let answer = tokio::select! {
            answer = answered.recv() => answer,
            _ = park_when(&store, &session_id, |park| park.attempts >= 2) => None,
        };
        let opener_answered = answer.is_some();
        if let Some((turn, _)) = answer {
            let error = turn.expect_err("a call that would reach a drifted tool live refuses");
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::LashlangCellBindingDrift,
                "{case}: {error:?}"
            );
            (&mut redriving).await.expect("the redrive task");
            let again = attempt(
                &world,
                shape,
                &session_id,
                &turn_id,
                &store,
                drift,
                Some(answers.clone()),
            );
            runner
                .run_turn(
                    admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
                    again,
                )
                .await;
            let error = answered
                .recv()
                .await
                .expect("the second redrive answered")
                .0
                .expect_err("the second redrive refuses again");
            assert_eq!(
                error.code,
                crate::RuntimeErrorCode::LashlangCellBindingDrift,
                "{case}, second redrive: {error:?}"
            );
        }
        assert_drift_park(&park_when(&store, &session_id, |park| park.attempts >= 2).await);
        assert_eq!(
            world.executions.load(Ordering::SeqCst),
            dispatched,
            "{case}: a call needed live on a drifted tool dispatches nothing"
        );
        redriving.abort();
        // Restored: the refused child recorded nothing, so a redrive under
        // the original tool runs its calls live and finishes, clearing the
        // park. Where the opener waits on the child instead (Restate), the
        // restore needs the child's serving deployment replaced, which
        // `lash-restate-test`'s `tool_child_drift` laws run.
        if opener_answered {
            runner
                .run_turn(
                    admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
                    attempt(
                        &world,
                        shape,
                        &session_id,
                        &turn_id,
                        &store,
                        Probe::Registered,
                        Some(answers.clone()),
                    ),
                )
                .await;
            let (turn, _) = answered
                .recv()
                .await
                .expect("the restored redrive answered");
            let turn = turn.unwrap_or_else(|error| {
                panic!("{case}: the restored redrive completes: {error:?}")
            });
            assert!(
                matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
                "{case}: restored outcome {:?}; errors {:?}",
                turn.outcome,
                turn.errors
            );
            assert!(
                world.executions.load(Ordering::SeqCst) > dispatched,
                "{case}: the restored tool runs the calls"
            );
            assert!(
                store
                    .load_turn_park(&session_id)
                    .await
                    .expect("read the park")
                    .is_none(),
                "{case}: the finished turn is not parked"
            );
        }
    }

    // A reworded tool is not drift: a call needed live runs once and the turn
    // finishes unparked.
    let session_id = SessionId::from(format!("{prefix}-nl-dsc-real"));
    let probe_session = SessionId::from(format!("{prefix}-nl-dsc-prob"));
    let journal_cut = cut_key(
        &world,
        &runner,
        Shape::Native,
        Cut::Unissued,
        &probe_session,
        &session_id,
        &turn_id,
    )
    .await;
    let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let dispatched = world.executions.load(Ordering::SeqCst);
    runner
        .run_cut_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            journal_cut,
            attempt(
                &world,
                Shape::Native,
                &session_id,
                &turn_id,
                &store,
                Probe::Registered,
                None,
            ),
            attempt(
                &world,
                Shape::Native,
                &session_id,
                &turn_id,
                &store,
                Probe::Described("Reworded at length."),
                Some(answers),
            ),
        )
        .await;
    let turn = answered
        .recv()
        .await
        .expect("the redrive answered")
        .0
        .unwrap_or_else(|error| panic!("a reworded tool never parks: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "{:?}",
        turn.errors
    );
    assert_eq!(world.executions.load(Ordering::SeqCst), dispatched + 1);
    assert!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park")
            .is_none()
    );
}
