//! Law L11 (FIG-3571), on the durable engine (ported by FIG-5308 from the
//! deleted lash-protocol-rlm `session_globals_law.rs`): session globals keep
//! their names and cross-cell visibility across cells and node restarts,
//! whatever carrier a cell's program travels in, and a front end's private
//! slots never become one.
//!
//! Each cell is a turn of one RLM session on a served core. The session's
//! globals are what the production prompt binds for the model's next step
//! (its `BOUND VARIABLES`). A restart kills the serving node and a new core
//! over the same database reads the session back from its committed
//! snapshot.
//!
//! Covers reassignment, block shadowing (including a later cell whose shadow
//! lowers to the same generated slot), root rebinding, member assignment, a
//! saved function with a frozen capture of a shadowed block binding called
//! after its block ends and after a restart, a process handle,
//! a projected host binding, a deferred tool binding's result, and a
//! loop-carried global.
//!
//! Multi-turn reads preserve all prior nested records, on one worker and
//! across node/worker restarts. The deterministic FIG-6466 reseal-order
//! regression lives in lash-vm-worker's page laws.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;
#[path = "support/web_fetch.rs"]
mod web_fetch;

use std::collections::BTreeSet;
use std::sync::Arc;

use served::{Tier, World};
use web_fetch::Fetch;

/// The session every cell runs in.
const SESSION: &str = "fig3571-l11";

/// The law's core: RLM turns with the session process controls, the
/// deferred `web.fetch` and its grant.
fn builder(backend: &lash::Backend) -> lash::LashCoreBuilder {
    lash::LashCore::rlm_builder(
        backend.clone(),
        served::rlm(backend, sim::untimed_workers()),
    )
    .plugin(Arc::new(
        lash::process_controls::SessionProcessAdminPluginFactory::new(
            lash_execution_contracts::process::lifetime::starter,
            lash_core::runtime::env_policy::policy::from_run(|_| Ok(Vec::new())),
        ),
    ))
    .tools(lash::tool_provider_key!("tools"), Arc::new(Fetch))
    .plugin(web_fetch::grant_fetch())
}

/// The names the production prompt binds in `request` (a rendered request):
/// every bound entry of its last `BOUND VARIABLES` section but the
/// read-only `history`.
fn bound_names(request: &str) -> BTreeSet<String> {
    fn texts(value: &serde_json::Value, into: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    match value {
                        serde_json::Value::String(text) if key == "text" => {
                            into.push(text.clone());
                        }
                        value => texts(value, into),
                    }
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|item| texts(item, into)),
            _ => {}
        }
    }
    let mut all = Vec::new();
    texts(
        &serde_json::from_str(request).expect("a rendered request"),
        &mut all,
    );
    let section = all
        .iter()
        .rev()
        .find_map(|text| {
            text.split_once("=== BOUND VARIABLES ===")
                .map(|(_, rest)| rest)
        })
        .expect("the request binds the session's variables");
    let section = section.split("\n===").next().unwrap_or_default();
    section
        .lines()
        .filter_map(|line| line.trim().strip_prefix("- `"))
        // A name listed as not bound is not a binding: its value cannot
        // be carried as data or saved as a self-contained function.
        .filter(|entry| !entry.contains("not bound:"))
        .filter_map(|entry| entry.split_once('`').map(|(name, _)| name.to_owned()))
        .filter(|name| name != "history")
        .collect()
}

/// The session's handle on the serving node.
async fn session(world: &World) -> lash::DurableSession {
    world
        .core
        .session(
            lash_vocabulary::identity::SessionId::try_from(SESSION.to_owned())
                .expect("a session id"),
        )
        .durable()
        .await
        .expect("the session is open on the serving node")
}

/// Run `code` as the session's next cell, in a turn of its own.
async fn run_cell(world: &World, name: &str, code: &str) -> lash::TurnOutput {
    world.script(name, vec![served::cell(code)]);
    world.send(&session(world).await, name).await
}

/// The globals the serving node shows the model at the start of a turn.
async fn globals(world: &World, name: &str) -> BTreeSet<String> {
    world.script(name, Vec::new());
    world.send(&session(world).await, name).await;
    world
        .requests(name)
        .first()
        .map(|request| bound_names(request))
        .expect("the serving node asked the model")
}

/// The globals a restarted node shows the model before any cell runs.
async fn globals_after_restart(world: &mut World, boot: &str) -> BTreeSet<String> {
    world.restart(boot, builder).await;
    globals(world, &format!("globals-after-{boot}")).await
}

fn names(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// ECMA-262 block scoping (ADR 0062/0064): a `let`/`const` declared inside a
/// block, a loop binder included, does not exist after its block, so the
/// front end marks it private like its own slots. `item` (the loop binder)
/// and `armed` (a `const` in an `if` arm) are therefore not session globals;
/// the top-level `let answer` and `let counter` are.
const CELL_1_GLOBALS: &[&str] = &["answer", "box", "counter", "fetched", "total"];
/// `reader` is saved with its block-local `answer` frozen at 5. The private
/// slot itself stays out of the globals, even after a restart. `plan`
/// holds a lambda, saved under a generated name that no global shows, and
/// `run` reaches it through its capture of `plan` (SAVE-IN-DATA).
const CELL_2_GLOBALS: &[&str] = &[
    "answer",
    "box",
    "counter",
    "fetched",
    "from_host",
    "total",
    "worker",
    "handle",
    "later",
    "plan",
    "reader",
    "run",
];

/// After every cell and every restart the session's globals are exactly the
/// authored ones, and the last cell reads each by name after two restarts.
async fn session_globals_survive_cells_and_reload_and_private_slots_never_do(tier: Tier) {
    let Some(mut world) = World::new(tier, builder).await else {
        return;
    };
    world.session(SESSION, served::spec(64)).await;
    let live = world
        .core
        .session(
            lash_vocabulary::identity::SessionId::try_from(SESSION.to_owned())
                .expect("a session id"),
        )
        .open()
        .await
        .expect("the session opens");
    live.admin()
        .protocol()
        .apply_session_extension(
            lash::rlm::rlm_session_projection_extension(
                lash::rlm::CodeModeProjectedBindings::new()
                    .bind_json("host_config", serde_json::json!({ "label": "from-host" }))
                    .expect("the projected binding is unique"),
            ),
            "host:rlm_session_globals:apply_session_extension:166".to_string(),
        )
        .await
        .expect("the host projection is accepted")
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("the host projects its binding");

    let first = run_cell(
        &world,
        "l11-cell-1",
        r#"let answer = 41;
const box = { n: 1 };
let counter = 0;
const fetched = await web.fetch({ url: "global" });
for (const item of [1, 2]) {
  counter = counter + item;
}
if (counter > 0) {
  const armed = counter;
  box.n = armed;
}
{
  const answer = 100;
  box.n = answer;
}
box.n += 1;
answer = answer + 1;
const total = [1, 2, 3].map((value) => value * 2).length;"#,
    )
    .await;
    served::assert_answered("cell 1", &first);
    assert_eq!(
        globals(&world, "globals-after-cell-1").await,
        names(CELL_1_GLOBALS),
        "cell 1"
    );
    assert_eq!(
        globals_after_restart(&mut world, "after-cell-1").await,
        names(CELL_1_GLOBALS),
        "restart after cell 1"
    );

    let second = run_cell(
        &world,
        "l11-cell-2",
        r#"let reader = null;
if (counter > 0) {
  let answer = 5;
  reader = () => answer;
}
const worker = async () => { await sleep(60000); return null; };
const handle = await processes.start({ definition: worker });
const later = reader() + answer;
const from_host = host_config.label;
const plan = { map: (x) => x + answer };
const run = () => plan.map(1);"#,
    )
    .await;
    served::assert_answered("cell 2", &second);
    assert_eq!(
        globals(&world, "globals-after-cell-2").await,
        names(CELL_2_GLOBALS),
        "cell 2"
    );
    assert_eq!(
        globals_after_restart(&mut world, "after-cell-2").await,
        names(CELL_2_GLOBALS),
        "restart after cell 2"
    );

    // Cell 3's block shadow lowers to the same generated slot cell 2's did;
    // neither survives its cell, so nothing stale collides.
    let third = run_cell(
        &world,
        "l11-cell-3",
        r#"if (counter > 0) {
  let answer = 7;
  box.n = answer;
}
const fetched = "rebound";
await control.finish({ answer: answer, counter: counter, n: box.n, later: later, saved_reader: reader(), fetched: fetched, from_host: from_host, total: total, handle: typeof handle, planned: plan.map(2), ran: run() });"#,
    )
    .await;
    served::assert_answered("cell 3", &third);
    let rendered = third
        .finished()
        .map(|(_, value)| value)
        .expect("cell 3 finishes the turn")
        .to_string();
    for expected in [
        "\"answer\":42",
        "\"counter\":3",
        "\"n\":7",
        "\"later\":47",
        "\"saved_reader\":5",
        "\"fetched\":\"rebound\"",
        "\"from_host\":\"from-host\"",
        "\"total\":3",
        "\"handle\":\"object\"",
        "\"planned\":44",
        "\"ran\":43",
    ] {
        assert!(
            rendered.contains(expected),
            "cell 3 reads every global by name after two restarts: missing {expected} in {rendered}"
        );
    }
    assert_eq!(
        globals(&world, "globals-after-cell-3").await,
        names(CELL_2_GLOBALS),
        "cell 3"
    );
    assert_eq!(
        globals_after_restart(&mut world, "after-cell-3").await,
        names(CELL_2_GLOBALS),
        "restart after cell 3"
    );
    world.shutdown().await;
}

/// The durable head's execution state of the session [`SESSION`].
async fn durable_execution_state(
    world: &World,
) -> Option<lash_core::plugin::HydratedExecutionGraph> {
    let view = lash_core_execution::store::SessionStore::new(
        world.backend.stores().session_store_factory(),
        lash_vocabulary::identity::SessionId::try_from(SESSION.to_owned()).expect("a session id"),
    )
    .expect("the session's store");
    lash_core_execution::store::load_session_window_state(
        &view,
        lash_core_execution::store::WindowSelector::Current,
    )
    .await
    .expect("load the durable head")
    .expect("the session is persisted")
    .state
    .execution_state_hydration()
    .expect("hydrate the durable execution state")
}

/// FIG-2521 (h): a message-only host append between turns leaves the
/// committed execution and the durable head's execution untouched, and the
/// next turn reads the committed global.
async fn rlm_message_append_keeps_the_committed_execution(tier: Tier) {
    let Some(world) = World::new(tier, builder).await else {
        return;
    };
    world.session(SESSION, served::spec(64)).await;
    let first = run_cell(
        &world,
        "fig2521-establish",
        "let accumulated = \"COMMITTED\";\nawait control.finish(accumulated);",
    )
    .await;
    served::assert_answered("the establishing turn", &first);
    let before = durable_execution_state(&world)
        .await
        .expect("the establishing turn committed an execution root");

    let live = world
        .core
        .session(
            lash_vocabulary::identity::SessionId::try_from(SESSION.to_owned())
                .expect("a session id"),
        )
        .open()
        .await
        .expect("the session opens");
    let appended = live
        .admin()
        .state()
        .append_session_nodes(lash_core::AppendSessionNodesRequest {
            operation_id: "fig2521-message".to_owned(),
            requires_ancestor_node_id: None,
            nodes: vec![lash_core::SessionAppendNode::message(
                lash_core::PluginMessage::text(lash_core::MessageRole::User, "message only")
                    .with_id("fig2521-message"),
            )],
        })
        .await
        .expect("the append is accepted")
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .expect("the message append settles");
    assert!(
        matches!(
            appended,
            lash_core::AppendSessionNodesOutcome::Appended { .. }
        ),
        "{appended:?}"
    );
    assert_eq!(
        durable_execution_state(&world).await.as_ref(),
        Some(&before),
        "a message-only append leaves the durable execution as committed"
    );

    let after = run_cell(
        &world,
        "fig2521-after-message",
        "await control.finish(accumulated);",
    )
    .await;
    served::assert_answered("the turn after the append", &after);
    assert_eq!(
        after.finished().map(|(_, value)| value),
        Some(&serde_json::json!("COMMITTED")),
        "the next turn reads the committed global"
    );
    world.shutdown().await;
}

/// The session the page laws run in.
const PAGES_SESSION: &str = "multi-turn-pages";
/// The host tool whose nested result each turn binds.
const RECORDS: &str = "records";
const RECORDS_OPERATION: &str = "tools.records";
/// How many turns the page laws run.
const PAGE_TURNS: usize = 3;

/// `records`' body: twenty nested records of the turn its arguments name,
/// enough to span many heap pages.
struct Records;

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for Records {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let turn = call.args.get("turn").and_then(serde_json::Value::as_u64);
        lash_core::ToolOutcome::ok(records(turn.unwrap_or_default())).into()
    }
}

fn records(turn: u64) -> serde_json::Value {
    (0..20_u64)
        .map(|i| {
            serde_json::json!({
                "id": i,
                "name": format!("record-{turn}-{i}"),
                "title": "A nested host tool result with ordinary fields",
                "description": "The value arrives from a native tool and is printed by the cell",
                "score": i * 13 + turn,
                "ratio": 1.25,
                "state": "ready",
                "tags": ["sqlite", "typescript", "nested"],
                "details": { "owner": "benchmark", "values": [i, i + 1, i + turn] },
            })
        })
        .collect()
}

/// The page laws' core over `workers`: RLM turns and the `records` tool.
fn page_builder(
    backend: &lash::Backend,
    workers: lash::vm::WorkerService,
) -> lash::LashCoreBuilder {
    let definition = lash_vocabulary::tool_contract::ToolDefinition::raw(
        RECORDS,
        RECORDS_OPERATION,
        "Answers the nested records of a turn.",
        serde_json::json!({ "type": "object", "properties": { "turn": { "type": "integer" } } }),
        serde_json::json!({ "type": "array" }),
    )
    .expect("records' schemas")
    .with_execution(std::time::Duration::from_secs(120));
    lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, workers)).tools(
        lash::tool_provider_key!("tools"),
        Arc::new(lash::tools::StaticToolProvider::new(
            vec![definition],
            Records,
        )),
    )
}

/// One worker serves the whole pool: every turn's cell runs on it.
fn one_worker() -> lash::vm::WorkerService {
    let mut config = sim::untimed_workers().config().clone();
    config.max_workers = 1;
    lash::vm::WorkerService::new(config).with_worker_receipts()
}

/// Turn `turn`'s cell: it rebinds `result` to the turn's records, keeps
/// them as `turn_<turn>`, prints them, and finishes with every turn's
/// records so far, which reads the values earlier turns committed.
fn page_cell(turn: usize) -> String {
    let kept: Vec<String> = (1..=turn).map(|turn| format!("turn_{turn}")).collect();
    format!(
        "const result = await tools.records({{ turn: {turn} }});\n\
         const turn_{turn} = result;\n\
         console.log(JSON.stringify(result));\n\
         await control.finish(JSON.stringify([{}]));",
        kept.join(", ")
    )
}

/// Runs turn `turn` and checks every turn's records are read back whole.
async fn page_turn(world: &World, turn: usize) {
    let name = format!("multi-turn-{turn}");
    world.script(&name, vec![served::cell(&page_cell(turn))]);
    let session = world
        .core
        .session(
            lash_vocabulary::identity::SessionId::try_from(PAGES_SESSION.to_owned())
                .expect("a session id"),
        )
        .durable()
        .await
        .expect("the session is open on the serving node");
    let output = world.send(&session, &name).await;
    served::assert_answered(&format!("turn {turn}"), &output);
    let expected: Vec<serde_json::Value> = (1..=turn as u64).map(records).collect();
    let answered = output
        .finished()
        .and_then(|(_, value)| value.as_str())
        .map(|text| serde_json::from_str::<serde_json::Value>(text).expect("the cell's JSON"));
    assert_eq!(
        answered,
        Some(serde_json::Value::Array(expected)),
        "turn {turn} reads every prior turn's records"
    );
}

/// Three turns read back all earlier turns' nested records. Worker receipts
/// prove that the same worker serves the entire session.
async fn later_turns_read_all_prior_records_on_one_worker(tier: Tier) {
    let workers = one_worker();
    let Some(world) = World::new(tier, |backend| page_builder(backend, workers.clone())).await
    else {
        return;
    };
    world.session(PAGES_SESSION, served::spec(8)).await;
    for turn in 1..=PAGE_TURNS {
        page_turn(&world, turn).await;
    }
    let pids: BTreeSet<u32> = workers
        .worker_receipts()
        .iter()
        .map(|receipt| receipt.pid)
        .collect();
    assert_eq!(pids.len(), 1, "one worker serves every turn: {pids:?}");
    world.shutdown().await;
}

/// Three turns read back all earlier turns' nested records after each node
/// restart. Worker receipts prove that each turn uses a fresh worker.
async fn later_turns_read_all_prior_records_after_each_restart(tier: Tier) {
    let mut workers = one_worker();
    let first = workers.clone();
    let Some(mut world) = World::new(tier, |backend| page_builder(backend, first)).await else {
        return;
    };
    world.session(PAGES_SESSION, served::spec(8)).await;
    let mut pids = BTreeSet::new();
    for turn in 1..=PAGE_TURNS {
        if turn > 1 {
            workers = one_worker();
            let fresh = workers.clone();
            world
                .restart(&format!("multi-turn-boot-{turn}"), |backend| {
                    page_builder(backend, fresh)
                })
                .await;
        }
        page_turn(&world, turn).await;
        let ran_on: BTreeSet<u32> = workers
            .worker_receipts()
            .iter()
            .map(|receipt| receipt.pid)
            .collect();
        assert_eq!(
            ran_on.len(),
            1,
            "turn {turn} runs on one worker: {ran_on:?}"
        );
        assert!(
            pids.is_disjoint(&ran_on),
            "turn {turn} runs on a fresh worker: {ran_on:?} after {pids:?}"
        );
        pids.extend(ran_on);
    }
    world.shutdown().await;
}

tiered_laws!(
    session_globals_survive_cells_and_reload_and_private_slots_never_do,
    rlm_message_append_keeps_the_committed_execution,
    later_turns_read_all_prior_records_on_one_worker,
    later_turns_read_all_prior_records_after_each_restart,
);
