//! FIG-3680: an orchestrating tool whose body journals no nested effect
//! redrives from the RLM cell that called it.
//!
//! One RLM turn runs a cell that calls `tools.relay`, an orchestrating tool
//! whose body returns without issuing a nested effect, and then `tools.probe`,
//! a leaf. The relay's call writes nothing under the cell's key namespace —
//! only its result's presentation, which is keyed by its call id — so the
//! journal holds nothing at the relay's issue ordinal and the probe's attempt
//! beyond it. The tier's runner cuts the first attempt before the cell's seal
//! is journaled, and the redrive must finish the turn from the journal: the
//! relay's body re-runs (it replays by re-execution, like the cell), its
//! presentation is served, the probe's result is served, and neither the model
//! nor the probe is asked again.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::ToolDefinitionBindingExt as _;
use lash_sansio::{SessionId, TurnId};

const CELL: &str = "<typescript>\nconst relayed = await tools.relay({});\nconst probed = await \
                    tools.probe({});\nfinish({ relayed, probed });\n</typescript>";

fn relay_definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        "tool:relay",
        "relay",
        "An orchestrating tool whose body issues no nested effect.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], "relay"))
}

/// The orchestrating relay: it answers from its arguments alone and journals
/// nothing.
struct EmptyRelay {
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::facade_support::OrchestratingToolImplementation for EmptyRelay {
    fn manifest(&self) -> crate::ToolManifest {
        relay_definition().manifest()
    }

    fn contract(&self) -> Arc<crate::ToolContract> {
        Arc::new(relay_definition().contract())
    }

    async fn execute(
        &self,
        _args: &serde_json::Value,
        _context: &crate::facade_support::OrchestrationContext<'_>,
    ) -> crate::ToolOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        crate::ToolOutcome::ok(serde_json::json!({ "relayed": true }))
    }
}

#[expect(
    unsafe_code,
    reason = "OrchestratingToolDef::from_first_party is lash-core's unsafe capability boundary, and this crate owns the tool contract it registers"
)]
fn relay_factory(executions: Arc<AtomicUsize>) -> Arc<dyn crate::facade_support::PluginFactory> {
    let relay = unsafe {
        crate::facade_support::OrchestratingToolDef::from_first_party(Arc::new(EmptyRelay {
            executions,
        }))
    };
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-empty-relay",
        crate::facade_support::PluginSpec::new().with_orchestrating_tool(relay),
    ))
}

#[derive(Clone)]
struct RelayWorld {
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
    model_calls: Arc<AtomicUsize>,
    relays: Arc<AtomicUsize>,
    probes: Arc<AtomicUsize>,
}

/// The counters a redrive started from: model calls, relay bodies, probe
/// dispatches.
type Counts = (usize, usize, usize);

impl RelayWorld {
    fn counts(&self) -> Counts {
        (
            self.model_calls.load(Ordering::SeqCst),
            self.relays.load(Ordering::SeqCst),
            self.probes.load(Ordering::SeqCst),
        )
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(
    world: &RelayWorld,
    session_id: &SessionId,
    store: Arc<dyn crate::RuntimePersistence>,
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
        crate::LawBackend::over_stores(Arc::clone(&world.stores), Arc::clone(&world.effect_host))
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
        .chain([
            relay_factory(Arc::clone(&world.relays)),
            super::cell_binding_drift::probe_factory(
                super::cell_binding_drift::Probe::Registered,
                Arc::clone(&world.probes),
            ),
        ])
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
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the empty-orchestration conformance runtime")
}

type Answer = (Result<crate::AssembledTurn, crate::RuntimeError>, Counts);

/// One attempt at `session_id`'s turn. It reports its answer, with the
/// counters it started from, on `answers` when there is one: a cut attempt
/// never answers.
fn attempt(
    world: &RelayWorld,
    session_id: &SessionId,
    turn_id: &TurnId,
    store: &Arc<dyn crate::RuntimePersistence>,
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
            let started = world.counts();
            let mut runtime = build_runtime(&world, &session_id, store).await;
            let mut input = crate::TurnInput::text("relay, then probe");
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

async fn answer(answers: &mut tokio::sync::mpsc::UnboundedReceiver<Answer>) -> Answer {
    answers
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran the attempt"))
}

/// Law: an orchestrating call that journals no nested effect, crashed
/// before its cell sealed, redrives from the cell to the turn's end with
/// nothing dispatched twice.
///
/// A probe run of the same turn in a same-length session finds the cell's
/// key namespace (keys spell the session id). It also pins what the law is
/// about: the journal holds nothing at the relay's ordinal and the probe's
/// attempt at the next one.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_empty_orchestrating_call_redrives_from_its_cell(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    rlm: Vec<Arc<dyn crate::facade_support::PluginFactory>>,
) {
    let world = RelayWorld {
        effect_host,
        stores,
        rlm,
        model_calls: Arc::new(AtomicUsize::new(0)),
        relays: Arc::new(AtomicUsize::new(0)),
        probes: Arc::new(AtomicUsize::new(0)),
    };
    let turn_id = TurnId::from(format!("{prefix}-empty-relay-turn"));
    let session_id = SessionId::from(format!("{prefix}-empty-relay-real"));
    let probe_session = SessionId::from(format!("{prefix}-empty-relay-prob"));

    let probe_store =
        crate::conformance::law_session_store(world.stores.as_ref(), &probe_session).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    let probe_scope = crate::ExecutionScope::turn(&probe_session, &turn_id);
    runner
        .run_turn(
            admit(probe_scope.clone()),
            attempt(
                &world,
                &probe_session,
                &turn_id,
                &probe_store,
                Some(answers),
            ),
        )
        .await;
    let (probe_turn, _) = answer(&mut answered).await;
    let probe_turn = probe_turn.expect("the probe turn completes");
    assert!(
        matches!(probe_turn.outcome, crate::TurnOutcome::Finished(_)),
        "probe outcome {:?}; errors {:?}",
        probe_turn.outcome,
        probe_turn.errors
    );
    let keys = runner
        .recorded_replay_keys(&probe_scope)
        .await
        .expect("the tier reads the replay keys it journaled");
    let probe_attempt = keys
        .iter()
        .find(|key| key.ends_with(":lk2:0000000001:attempt:1"))
        .unwrap_or_else(|| panic!("the cell journaled the probe's attempt: {keys:?}"));
    let namespace = probe_attempt.trim_end_matches("0000000001:attempt:1");
    assert!(
        !keys
            .iter()
            .any(|key| key.starts_with(&format!("{namespace}0000000000"))),
        "the relay's call journals nothing under the cell's namespace: {keys:?}"
    );
    let seal = format!("{namespace}~seal").replace(probe_session.as_str(), session_id.as_str());

    let store = crate::conformance::law_session_store(world.stores.as_ref(), &session_id).await;
    let (answers, mut answered) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_cut_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&session_id, &turn_id)),
            crate::JournalCut {
                replay_key: seal,
                at: crate::JournalCutPoint::BeforeEffect,
            },
            attempt(&world, &session_id, &turn_id, &store, None),
            attempt(&world, &session_id, &turn_id, &store, Some(answers)),
        )
        .await;
    let (turn, (asked, _, probed)) = answer(&mut answered).await;
    let turn = turn.unwrap_or_else(|error| panic!("the redrive completes: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "redriven outcome {:?}; errors {:?}",
        turn.outcome,
        turn.errors
    );
    assert!(
        store
            .load_turn_park(&session_id)
            .await
            .expect("read the park")
            .is_none(),
        "a completed redrive leaves no park"
    );
    assert_eq!(
        world.model_calls.load(Ordering::SeqCst),
        asked,
        "the model call replays from the journal"
    );
    assert_eq!(
        world.probes.load(Ordering::SeqCst),
        probed,
        "the probe's recorded result is served, not dispatched again"
    );
    assert_eq!(
        world.probes.load(Ordering::SeqCst),
        2,
        "the probe ran once in each session's live pass and never on the redrive"
    );
}
