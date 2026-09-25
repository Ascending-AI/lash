//! FIG-3587: a redriven code cell links against the binding set its live
//! pass journaled, whatever the live registry now offers.
//!
//! One RLM turn runs a cell that calls `tools.probe` once and finishes with
//! its reply. The tier's runner cuts the first attempt down at a journal
//! point the law names by replay key, and the redrive runs under a registry
//! that removed `tools.probe` or reworded its descriptor under the same name:
//!
//! - cut after the probe's result was recorded (the cell's seal could not be
//!   journaled), the redrive replays the model call and the cell from the
//!   journal — the model prompt and the cell's binding both come from the
//!   journaled surface — and the turn finishes with nothing dispatched;
//! - cut after the probe dispatched but before its result was recorded, the
//!   call would reach the drifted tool live, so every redrive refuses with
//!   `lashlang_cell_binding_drift`, parks the turn and dispatches nothing.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::ToolDefinitionBindingExt as _;
use lash_sansio::{SessionId, TurnId};

const CELL: &str =
    "<typescript>\nconst reply = await tools.probe({});\nfinish(reply);\n</typescript>";

/// What the registry offers for `tools.probe`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Probe {
    Registered,
    Removed,
    Described(&'static str),
    /// A dispatch-relevant change: another retry policy.
    Retried,
}

/// The plugin that registers `probe` for `tools.probe`.
pub(super) fn probe_factory(
    probe: Probe,
    executions: Arc<AtomicUsize>,
) -> Arc<dyn crate::facade_support::PluginFactory> {
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(ProbeTool { probe, executions });
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-binding-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(tools),
    ))
}

struct ProbeTool {
    probe: Probe,
    executions: Arc<AtomicUsize>,
}

impl ProbeTool {
    fn definition(&self) -> Option<crate::ToolDefinition> {
        let description = match self.probe {
            Probe::Registered | Probe::Retried => "Binding-drift probe tool.",
            Probe::Described(description) => description,
            Probe::Removed => return None,
        };
        let mut definition = crate::ToolDefinition::raw(
            "tool:probe",
            "probe",
            description,
            serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            serde_json::json!({ "type": "object" }),
        )
        .with_tool_binding(crate::ToolBinding::new(["tools"], "probe"));
        if matches!(self.probe, Probe::Retried) {
            definition.manifest.retry_policy = crate::ToolRetryPolicy::Safe {
                max_attempts: 3,
                base_delay_ms: 10,
                max_delay_ms: 100,
            };
        }
        Some(definition)
    }
}

#[async_trait::async_trait]
impl crate::ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        self.definition()
            .map(|definition| definition.manifest())
            .into_iter()
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        let definition = self.definition()?;
        (definition.manifest.name == name).then(|| Arc::new(definition.contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcome::ok(serde_json::json!({ "probed": true })).into()
    }
}

#[derive(Clone)]
struct DriftWorld {
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    model_calls: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(
    world: &DriftWorld,
    session_id: &SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
    probe: Probe,
) -> crate::LashRuntime {
    let calls = Arc::clone(&world.model_calls);
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: CELL.to_string(),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build();
    let mut host =
        crate::LawBackend::over_stores(world.stores.as_ref(), Arc::clone(&world.effect_host))
            .host_config(
                crate::CommitBudget::bounded(1024 * 1024, 512),
                crate::QueuedWorkBatchingConfig::new(1),
            );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let factories = world
        .rlm
        .iter()
        .cloned()
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
    .expect("build the binding-drift conformance runtime")
}

/// What a redrive of the law's turn saw: its answer, and the model calls and
/// tool dispatches counted when it started (all the cut attempt made).
type Answer = (
    Result<crate::AssembledTurn, crate::RuntimeError>,
    (usize, usize),
);

/// One attempt at `session_id`'s turn under `probe`. It reports its answer,
/// with the counters it started from, on `answers` when there is one: a cut
/// attempt never answers.
fn attempt(
    world: &DriftWorld,
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
            let started = (
                world.model_calls.load(Ordering::SeqCst),
                world.executions.load(Ordering::SeqCst),
            );
            let mut runtime = build_runtime(&world, &session_id, store, probe).await;
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

/// The next answer a redrive reported.
async fn answer(answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answer>) -> Answer {
    answers
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran the redrive"))
}

/// The replay key of the probe's first attempt in `session_id`'s turn, found
/// by running the same turn to completion in a same-length probe session and
/// reading the replay keys the tier journaled for it: keys spell the session
/// id, so the probe's key names the real one once its session id is
/// substituted.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn first_attempt_key(
    world: &DriftWorld,
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    probe_session: &SessionId,
    session_id: &SessionId,
    turn_id: &TurnId,
) -> String {
    assert_eq!(
        probe_session.as_str().len(),
        session_id.as_str().len(),
        "same-length session ids"
    );
    let store = crate::conformance::law_session_store(world.stores.as_ref(), probe_session).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let probe = attempt(
        world,
        probe_session,
        turn_id,
        &store,
        Probe::Registered,
        Some(answers),
    );
    let scope = crate::ExecutionScope::turn(probe_session, turn_id);
    runner.run_turn(admit(scope.clone()), probe).await;
    answer(&mut answered)
        .await
        .0
        .expect("the probe turn completes");
    runner
        .recorded_replay_keys(&scope)
        .await
        .expect("the tier reads the replay keys it journaled")
        .into_iter()
        .find(|key| key.ends_with(":lk2:0000000000:attempt:1"))
        .expect("the probe's cell journaled its tool attempt")
        .replace(probe_session.as_str(), session_id.as_str())
}

/// Law: a redriven cell completes from its journal when the drifted tool's
/// result was recorded, and parks with the binding-drift refusal when the
/// call would reach the drifted tool live.
///
/// The first attempt is cut down by the tier's runner at a journal point the
/// law names by replay key: before the cell's seal is journaled (the probe's
/// result was recorded), or before the probe's result is.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn redriven_cell_links_against_its_journaled_binding_set(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let world = DriftWorld {
        effect_host,
        stores,
        rlm,
        model_calls: Arc::new(AtomicUsize::new(0)),
        executions: Arc::new(AtomicUsize::new(0)),
    };
    let turn_id = TurnId::from(format!("{prefix}-binding-drift-turn"));
    for (case, recorded, drift, word) in [
        ("done-rem", true, Probe::Removed, "missing"),
        ("done-chg", true, Probe::Retried, "changed"),
        ("done-dsc", true, Probe::Described("Reworded."), ""),
        ("live-rem", false, Probe::Removed, "missing"),
        ("live-chg", false, Probe::Retried, "changed"),
    ] {
        let session_id = SessionId::from(format!("{prefix}-{case}-real"));
        let probe_session = SessionId::from(format!("{prefix}-{case}-prob"));
        let attempt_key =
            first_attempt_key(&world, &runner, &probe_session, &session_id, &turn_id).await;
        let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
        let cut = if recorded {
            crate::JournalCut {
                replay_key: attempt_key.replace(":lk2:0000000000:attempt:1", ":lk2:~seal"),
                at: crate::JournalCutPoint::BeforeEffect,
            }
        } else {
            crate::JournalCut {
                replay_key: attempt_key,
                at: crate::JournalCutPoint::BeforeResult,
            }
        };
        let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
        let admitted = admit(crate::ExecutionScope::turn(&session_id, &turn_id));
        runner
            .run_cut_then_redriven_turn(
                admitted.clone(),
                cut,
                attempt(
                    &world,
                    &session_id,
                    &turn_id,
                    &store,
                    Probe::Registered,
                    None,
                ),
                attempt(
                    &world,
                    &session_id,
                    &turn_id,
                    &store,
                    drift,
                    Some(answers.clone()),
                ),
            )
            .await;
        let (turn, (asked, dispatched)) = answer(&mut answered).await;

        if recorded {
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
                "{case}: a completed redrive leaves no park"
            );
        } else {
            let mut turn = Some(turn);
            for redrive in ["first", "second"] {
                let error = turn
                    .take()
                    .expect("each redrive answered")
                    .expect_err("a call that would reach a drifted tool live refuses");
                assert_eq!(
                    error.code,
                    crate::RuntimeErrorCode::LashlangCellBindingDrift,
                    "{case}, {redrive} redrive: {error:?}"
                );
                let park = store
                    .load_turn_park(&session_id)
                    .await
                    .expect("read the park")
                    .expect("the refused turn is parked");
                let crate::store::ParkReason::BindingDrift { message } = &park.reason else {
                    panic!("{case}: the park names the binding drift: {park:?}");
                };
                assert!(
                    message.contains("`tools.probe`")
                        && message.contains("tool:probe")
                        && message.contains(word),
                    "{case}: the park names the binding and how it drifted: {message}"
                );
                if redrive == "first" {
                    let again = attempt(
                        &world,
                        &session_id,
                        &turn_id,
                        &store,
                        drift,
                        Some(answers.clone()),
                    );
                    runner.run_turn(admitted.clone(), again).await;
                    turn = Some(answer(&mut answered).await.0);
                }
            }
        }
        assert_eq!(
            world.model_calls.load(Ordering::SeqCst),
            asked,
            "{case}: the model call replays from the journal"
        );
        assert_eq!(
            world.executions.load(Ordering::SeqCst),
            dispatched,
            "{case}: nothing is dispatched on redrive"
        );
    }

    // A reworded descriptor is not drift: a call whose result was never
    // recorded runs the tool live, once, and the turn completes unparked.
    let session_id = SessionId::from(format!("{prefix}-live-dsc-real"));
    let probe_session = SessionId::from(format!("{prefix}-live-dsc-prob"));
    let attempt_key =
        first_attempt_key(&world, &runner, &probe_session, &session_id, &turn_id).await;
    let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_cut_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            crate::JournalCut {
                replay_key: attempt_key,
                at: crate::JournalCutPoint::BeforeResult,
            },
            attempt(
                &world,
                &session_id,
                &turn_id,
                &store,
                Probe::Registered,
                None,
            ),
            attempt(
                &world,
                &session_id,
                &turn_id,
                &store,
                Probe::Described("Reworded at length."),
                Some(answers),
            ),
        )
        .await;
    let (turn, (_, dispatched)) = answer(&mut answered).await;
    let turn = turn.unwrap_or_else(|error| panic!("a redescribed tool never parks: {error:?}"));
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
