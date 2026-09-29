//! A settled batch member runs once across a crash of its turn (FIG-4064).
//!
//! One model response issues two parallel tool calls, which the runtime
//! dispatches as one batch. The `settled` member answers at once; the `held`
//! member starts and then waits on a gate the law controls. Once the settled
//! member has answered and its settlement has had time to become durable, the
//! law kills the turn's execution where it stands, with the held member still
//! in flight, and leaves the turn to the tier's recovery: a fresh driver over
//! the same host in process, Restate's redelivery of the same invocation on a
//! Restate tier. The gate then opens and the recovered turn must finish.
//!
//! The settled member's recorded completion is the member's outcome (ADR 0099
//! reuse): the recovery reuses it and never runs the member again. The held
//! member was cut mid-flight, so it may settle in the execution that outlived
//! the crash or run again under a fresh attempt; the law bounds nothing about
//! it beyond the turn finishing. The model's first response is recorded too,
//! so the recovery asks the model only for what came after it.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use crate::ToolDefinitionBindingExt as _;
use crate::admit;
use lash_core::testing::TestTurnDrive as _;
use lash_sansio::sync::MutexExt as _;

/// The law's deadlock budget for each run of the turn.
const TURN_BUDGET: Duration = Duration::from_secs(60);

/// How long the settled member's settlement is given to become durable before
/// the crash. The member answered by then; this covers only the journaling of
/// its answer, so the crash cuts after a completion the recovery must reuse.
const SETTLEMENT_GRACE: Duration = Duration::from_millis(500);

const SETTLED: &str = "batch_redrive_settled";
const HELD: &str = "batch_redrive_held";

/// What the members did, across every execution of the turn.
#[derive(Default)]
struct MemberWitness {
    started: std::sync::Mutex<BTreeMap<String, usize>>,
    executed: std::sync::Mutex<BTreeMap<String, usize>>,
    released: tokio::sync::Notify,
    open: std::sync::atomic::AtomicBool,
}

impl MemberWitness {
    fn count(map: &std::sync::Mutex<BTreeMap<String, usize>>, member: &str) -> usize {
        map.lock_recover().get(member).copied().unwrap_or_default()
    }

    fn started(&self, member: &str) -> usize {
        Self::count(&self.started, member)
    }

    fn executed(&self, member: &str) -> usize {
        Self::count(&self.executed, member)
    }

    fn release(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.released.notify_waiters();
    }

    async fn gate(&self) {
        loop {
            let released = self.released.notified();
            if self.open.load(Ordering::SeqCst) {
                return;
            }
            released.await;
        }
    }
}

fn member_definition(name: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "A batch member of the crash-redrive law.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .with_tool_binding(crate::ToolBinding::new(["tools"], name))
}

struct RedriveMembers {
    witness: Arc<MemberWitness>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for RedriveMembers {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        [SETTLED, HELD]
            .into_iter()
            .map(|name| member_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        [SETTLED, HELD]
            .contains(&name)
            .then(|| Arc::new(member_definition(name).contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let name = call.name().to_string();
        *self
            .witness
            .started
            .lock_recover()
            .entry(name.clone())
            .or_default() += 1;
        if name == HELD {
            self.witness.gate().await;
        }
        *self
            .witness
            .executed
            .lock_recover()
            .entry(name.clone())
            .or_default() += 1;
        crate::ToolOutcome::ok(serde_json::json!({ "member": name })).into()
    }
}

/// The turn's responses: the producer's batch over the two members, then the
/// final text.
fn script(producer: &crate::ToolBatchProducer) -> Vec<crate::LlmResponse> {
    let plan = crate::ToolBatchPlan {
        scenario: "batch-crash-redrive".to_string(),
        leaves: [SETTLED, HELD]
            .into_iter()
            .map(|member| crate::ToolBatchLeaf {
                tool: member.to_string(),
                route: crate::ToolBatchRoute::Leaf,
            })
            .collect(),
        via: crate::ToolBatchEntry::Direct,
        relay_tool: String::new(),
        idle_tools: Vec::new(),
    };
    let mut script = (producer.script)(&plan);
    script.push(crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: "batch recovered".to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..crate::LlmResponse::default()
    });
    script
}

/// One execution of the turn: a fresh runtime over the tier's host and
/// stores, driving the turn on the controller the tier lends it. The model
/// answers a request by how many answers the conversation already holds, so
/// a replay that asks again is answered as the first execution was.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn drive_redrive_turn(
    session_id: &lash_sansio::SessionId,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    producer: &crate::ToolBatchProducer,
    witness: Arc<MemberWitness>,
    model_calls: Arc<AtomicUsize>,
    turn_scope: crate::ScopedEffectController<'_>,
) -> crate::TurnOutcome {
    let responses = Arc::new(script(producer));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let responses = Arc::clone(&responses);
            let model_calls = Arc::clone(&model_calls);
            async move {
                model_calls.fetch_add(1, Ordering::SeqCst);
                let answered = request
                    .messages
                    .iter()
                    .filter(|message| {
                        matches!(message.role, lash_sansio::llm::types::LlmRole::Assistant)
                    })
                    .count();
                Ok(responses[answered.min(responses.len() - 1)].clone())
            }
        })
        .build();
    let law_backend = crate::LawBackend::over_stores(Arc::clone(&stores), host);
    let mut config = law_backend.host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let members: Arc<dyn crate::ToolProvider> = Arc::new(RedriveMembers { witness });
    let mut factories = producer.factories.clone();
    factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-batch-crash-redrive",
        crate::facade_support::PluginSpec::new().with_tool_provider(members),
    )));
    let mut runtime = Box::pin(
        crate::LashRuntime::builder(config, crate::testing::runtime_lease_owner())
            .with_session_id(session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_host(crate::facade_support::PluginHost::new(factories))
            .with_store(crate::conformance::helpers::session_view(
                &crate::conformance::law_session_store(stores.as_ref(), session_id).await,
                session_id.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the batch crash-redrive runtime");
    let mut input = crate::TurnInput::text("run the batch across a crash");
    input.trace_turn_id = Some(redrive_turn_id(session_id));
    tokio::time::timeout(
        TURN_BUDGET,
        runtime.drive_turn(
            input,
            crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), turn_scope),
        ),
    )
    .await
    .expect("the batch crash-redrive turn settles within its budget")
    .expect("run the batch crash-redrive turn")
    .outcome
}

fn redrive_turn_id(session_id: &lash_sansio::SessionId) -> lash_sansio::TurnId {
    lash_sansio::TurnId::from(format!("{session_id}-turn"))
}

/// A crash of the turn while its batch is open, with one member settled and
/// one held, recovers without running the settled member again.
///
/// The crash is the crashing attempt's own panic, fired from outside the
/// turn once the settled member answered and the held one started: the tier
/// then redelivers the turn to the redriving attempt the way it recovers a
/// crashed turn, and the held member's gate opens.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_settled_batch_member_runs_once_across_a_turn_crash(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: crate::ToolBatchProducer,
) {
    let session_id =
        lash_sansio::SessionId::from(format!("{prefix}-{}-batch-crash-redrive", producer.label));
    let admitted = admit(crate::ExecutionScope::turn(
        &session_id,
        redrive_turn_id(&session_id),
    ));
    let witness = Arc::new(MemberWitness::default());
    let model_calls = Arc::new(AtomicUsize::new(0));
    let crash = crate::ConformanceCrash::new();
    let (outcomes, mut outcome) = tokio::sync::mpsc::unbounded_channel();
    let attempt = |crashing: bool| -> crate::ConformanceTurnAttempt {
        let session_id = session_id.clone();
        let host = Arc::clone(&host);
        let stores = Arc::clone(&stores);
        let producer = producer.clone();
        let witness = Arc::clone(&witness);
        let model_calls = Arc::clone(&model_calls);
        let crash = crash.clone();
        let outcomes = outcomes.clone();
        Arc::new(move |turn_scope| {
            let session_id = session_id.clone();
            let host = Arc::clone(&host);
            let stores = Arc::clone(&stores);
            let producer = producer.clone();
            let witness = Arc::clone(&witness);
            let model_calls = Arc::clone(&model_calls);
            let crash = crash.clone();
            let outcomes = outcomes.clone();
            Box::pin(async move {
                if !crashing {
                    witness.release();
                }
                let turn = drive_redrive_turn(
                    &session_id,
                    host,
                    stores,
                    &producer,
                    witness,
                    model_calls,
                    turn_scope,
                );
                if crashing {
                    tokio::select! {
                        biased;
                        () = crash.fired() => panic!("the batch crash-redrive turn crashes here"),
                        ended = turn => panic!(
                            "the crashing batch turn ended ({ended:?}) before its crash fired"
                        ),
                    }
                }
                let _ = outcomes.send(turn.await);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    let fire = {
        let witness = Arc::clone(&witness);
        let crash = crash.clone();
        crate::task::spawn(async move {
            while !(witness.executed(SETTLED) >= 1 && witness.started(HELD) >= 1) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(SETTLEMENT_GRACE).await;
            // The held member is still in flight: its gate opens only in the
            // redriving attempt.
            assert_eq!(
                witness.executed(HELD),
                0,
                "the held member is in flight at the crash"
            );
            crash.fire();
        })
    };
    runner
        .run_crashed_then_redriven_turn(admitted, attempt(true), attempt(false))
        .await;
    fire.await.expect("the crash trigger's task");
    let ended = outcome
        .recv()
        .await
        .expect("the recovered turn reports its outcome");
    assert!(
        matches!(ended, crate::TurnOutcome::Finished(_)),
        "the recovered batch turn finishes: {ended:?}"
    );
    assert_eq!(
        witness.executed(SETTLED),
        1,
        "{}: the recovery reuses the settled member's recorded completion and never runs it \
         again (started {} times; the model was called {} times)",
        producer.label,
        witness.started(SETTLED),
        model_calls.load(Ordering::SeqCst),
    );
    assert!(
        witness.executed(HELD) >= 1,
        "the held member settles once its gate opens"
    );
}
