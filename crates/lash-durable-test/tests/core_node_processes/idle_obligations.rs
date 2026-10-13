//! IDLE-RUN-NEVER-STRANDED (law 13, process arm; S7): a process requires a
//! provider's callback only while a step it admitted under that provider is
//! unsettled. A process whose environment links a provider a node does not
//! serve, but which holds no unsettled call to it, is claimed and run there;
//! settling one of two calls to the provider leaves it required.

use super::*;

/// The token a node serves when its write component contributes the write
/// tool's provider, and a process requires while a write is unsettled.
fn write_token() -> String {
    format!(
        "callback:process:{}/tool_provider:tools",
        WRITE_COMPONENT.component()
    )
}

/// The write component's id and format with no tool provider: a node
/// that installs it serves the component's config and not the write tool.
fn write_component_without_tool() -> Arc<dyn lash::process::ProcessComponentFactory> {
    Arc::new(
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial(WRITE_COMPONENT.component()),
            lash_core::facade_support::PluginSpec::new(),
        )
        .as_component(WRITE_COMPONENT.format())
        .expect("the bare write component is a process component"),
    )
}

/// A write step named `step` on `x`.
fn write_step(step: &str, x: i64) -> lash_core::StepRequest {
    lash_core::StepRequest::Tool {
        step: lash_core::StepName(step.to_owned()),
        effect: lash_core_execution::testing::effect_of(&write_definition()),
        input: serde_json::json!({ "x": x }),
        site: None,
    }
}

/// The idle engine: it writes once, then sleeps until its start's
/// `until_ms` holding no step, and answers once it wakes.
const IDLE_ENGINE: &str = "core-node-idle-engine";

fn idle_engine_advance(
    state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { payload } => {
            *state = payload;
            lash_core::EngineAction::Steps {
                steps: vec![write_step("write", 1)],
                wake: None,
            }
        }
        lash_core::EngineEvent::StepSettled { .. } => lash_core::EngineAction::Sleep {
            until: lash_core::durable_port::DurableInstant(
                state["until_ms"].as_i64().unwrap_or_default(),
            ),
            site: None,
        },
        lash_core::EngineEvent::Woke => answer(serde_json::json!({ "woke": true })),
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

/// The two-write engine: it writes twice at once, a quick write and a held
/// one, and waits.
const TWO_WRITE_ENGINE: &str = "core-node-two-write-engine";

fn two_write_engine_advance(
    _state: &mut serde_json::Value,
    event: lash_core::EngineEvent,
) -> lash_core::EngineAction {
    match event {
        lash_core::EngineEvent::Started { .. } => lash_core::EngineAction::Steps {
            steps: vec![write_step("quick", 1), write_step("held", -1)],
            wake: None,
        },
        lash_core::EngineEvent::Cancelled { origin, .. } => cancelled(origin),
        _ => lash_core::EngineAction::Idle,
    }
}

fn wall_millis() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the wall clock is past the epoch")
            .as_millis(),
    )
    .expect("milliseconds fit")
}

/// A process whose environment links the write tool, its one write settled
/// and asleep, is claimed and answered by a node whose write component
/// contributes no tool: it holds no unsettled call to the provider.
async fn a_process_holding_no_call_to_an_unserved_provider_still_runs(tier: Tier) {
    let world = Arc::new(World::default());
    let (stores, _keep) = stores(tier).await;
    let engine = || -> Vec<Arc<dyn lash_core::ProcessEngine>> {
        vec![Arc::new(ScriptEngine {
            kind: IDLE_ENGINE,
            advance: idle_engine_advance,
            performs: writes,
        })]
    };
    let (backend, first) = node_over(
        Arc::clone(&stores),
        lash_core_execution::DurableSettings::default(),
        engine(),
        |backend| {
            lash::LashCore::standard_builder(backend.clone())
                .process_component(write_component(&world), WRITE_COMPONENT)
        },
    );
    let process = start_under(
        &first,
        &write_environment(),
        IDLE_ENGINE,
        serde_json::json!({ "until_ms": wall_millis() + 5_000 }),
        |request| request,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), async {
        while world.writes.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the write runs within a minute");
    tokio::time::sleep(Duration::from_millis(250)).await;
    first.shutdown().await.expect("the first node stops");
    drop(first);
    assert!(
        backend
            .process_registry()
            .get_process(&process)
            .await
            .expect("the process is read")
            .is_some_and(|record| !matches!(
                record.lifecycle,
                lash_core::ProcessLifecycleState::Terminal { .. }
            )),
        "the process sleeps when its first node stops"
    );

    let (_, bare) = node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        engine(),
        |backend| {
            lash::LashCore::standard_builder(backend.clone())
                .process_component(write_component_without_tool(), WRITE_COMPONENT)
        },
    );
    let serves = lash::testing::node_serves(&bare);
    assert!(
        !serves.contains(&write_token()),
        "the bare node does not serve the write tool: {serves:?}"
    );
    assert_eq!(
        success(&ended(&bare, &process).await),
        serde_json::json!({ "woke": true }),
        "the bare node claims the process and runs it to its answer"
    );
    assert_eq!(world.writes.lock().unwrap().len(), 1, "one write");
    bare.shutdown().await.expect("the bare node stops");
}

on_every_tier!(a_process_holding_no_call_to_an_unserved_provider_still_runs);

/// What the process actor `process` carried as its claim requirements,
/// read by a probe node that serves `serves` and claims it once released.
async fn claimed_requirements(
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
    serves: Vec<String>,
) -> Vec<String> {
    use lash_core::durable_port::{ActorKey, CommitLabel, NodeId, NodeSpec, Release};
    let store = backend.durable();
    let probe = store
        .register_node(&NodeSpec {
            node: NodeId::new("law-requirement-probe"),
            decodes: backend.formats().decodes(),
            serves,
            ttl_millis: 15_000,
        })
        .await
        .expect("register the probe node");
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let snapshot = store.actor(&actor).await.expect("read the actor");
    let mut requires = None;
    for claim in store.claim(&probe, 64).await.expect("claim") {
        if claim.actor == actor {
            requires = Some(claim.requires.clone());
        }
        let mut tx = store
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("open the probe's transaction");
        tx.give_up(Release::Ready);
        store
            .commit(tx, CommitLabel::new("law.probe_requires"))
            .await
            .expect("release the probed actor");
    }
    store
        .release_node(&probe)
        .await
        .expect("release the probe node");
    requires.unwrap_or_else(|| panic!("the stopped node released the process ready: {snapshot:?}"))
}

/// A process writes twice at once; the quick write settles and the held
/// one does not when its node stops, and the process still requires the
/// write tool's provider.
async fn settling_one_of_two_writes_leaves_the_provider_required(tier: Tier) {
    let world = Arc::new(World::default());
    let (stores, _keep) = stores(tier).await;
    let (backend, core) = node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        vec![Arc::new(ScriptEngine {
            kind: TWO_WRITE_ENGINE,
            advance: two_write_engine_advance,
            performs: writes,
        })],
        |backend| {
            lash::LashCore::standard_builder(backend.clone())
                .process_component(write_component(&world), WRITE_COMPONENT)
        },
    );
    let process = start_under(
        &core,
        &write_environment(),
        TWO_WRITE_ENGINE,
        serde_json::json!({}),
        |request| request,
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), world.held.notified())
        .await
        .expect("the held write starts");
    tokio::time::timeout(Duration::from_secs(60), async {
        while world.writes.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both writes ran");
    // The quick write's outcome commits within the group-commit window
    // once its body answers, and the next pass folds it; the held write
    // never answers.
    tokio::time::sleep(Duration::from_millis(250)).await;
    let serves = lash::testing::node_serves(&core);
    assert!(serves.contains(&write_token()), "{serves:?}");
    core.shutdown().await.expect("the node stops");
    let requires = claimed_requirements(&backend, &process, serves).await;
    assert!(
        requires.contains(&write_token()),
        "the unsettled held write keeps its provider required: {requires:?}"
    );
}

on_every_tier!(settling_one_of_two_writes_leaves_the_provider_required);
