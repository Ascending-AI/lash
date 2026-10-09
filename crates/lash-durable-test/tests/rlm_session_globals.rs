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
//! closure over a shadowed block binding called after its block ends (closures
//! never cross a cell, so none is called after a restart), a process handle,
//! a projected host binding, a deferred tool binding's result, and a
//! loop-carried global.

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
use web_fetch::{Fetch, GrantFetch};

/// The session every cell runs in.
const SESSION: &str = "fig3571-l11";

/// The law's core: RLM turns with the session process controls, the
/// deferred `web.fetch` and its grant.
fn builder(backend: &lash::Backend) -> lash::LashCoreBuilder {
    lash::LashCore::rlm_builder(
        backend.clone(),
        served::rlm(backend, Some(Arc::new(GrantFetch)), sim::untimed_workers()),
    )
    .plugin(Arc::new(
        lash::process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::starter),
    ))
    .tools(Arc::new(Fetch))
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
        // A name listed as not bound is not a binding: it held a function
        // or a task, which no later cell reads (`K-SES-003`).
        .filter(|entry| !entry.contains("not bound:"))
        .filter_map(|entry| entry.split_once('`').map(|(name, _)| name.to_owned()))
        .filter(|name| name != "history")
        .collect()
}

/// The session's handle on the serving node.
async fn session(world: &World) -> lash::DurableSession {
    world
        .core
        .session(lash::SessionId::try_from(SESSION.to_owned()).expect("a session id"))
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
/// `reader` is absent by design: a closure never crosses a program boundary,
/// so the closure over a shadowed block binding is exercised inside its own
/// cell.
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
        .session(lash::SessionId::try_from(SESSION.to_owned()).expect("a session id"))
        .open()
        .await
        .expect("the session opens");
    live.admin()
        .protocol()
        .apply_session_extension(
            lash::rlm::rlm_session_projection_extension(
                lash::rlm::RlmProjectedBindings::new()
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
const from_host = host_config.label;"#,
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
finish({ answer: answer, counter: counter, n: box.n, later: later, fetched: fetched, from_host: from_host, total: total, handle: typeof handle });"#,
    )
    .await;
    served::assert_answered("cell 3", &third);
    let rendered = third
        .final_value()
        .expect("cell 3 finishes the turn")
        .to_string();
    for expected in [
        "\"answer\":42",
        "\"counter\":3",
        "\"n\":7",
        "\"later\":47",
        "\"fetched\":\"rebound\"",
        "\"from_host\":\"from-host\"",
        "\"total\":3",
        "\"handle\":\"object\"",
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
) -> Option<lash_core::plugin::HydratedExecutionState> {
    let view = lash_core_execution::store::SessionStore::new(
        world.backend.stores().session_store_factory(),
        lash::SessionId::try_from(SESSION.to_owned()).expect("a session id"),
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
        "let accumulated = \"COMMITTED\";\nfinish(accumulated);",
    )
    .await;
    served::assert_answered("the establishing turn", &first);
    let before = durable_execution_state(&world)
        .await
        .expect("the establishing turn committed an execution root");

    let live = world
        .core
        .session(lash::SessionId::try_from(SESSION.to_owned()).expect("a session id"))
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

    let after = run_cell(&world, "fig2521-after-message", "finish(accumulated);").await;
    served::assert_answered("the turn after the append", &after);
    assert_eq!(
        after.final_value(),
        Some(&serde_json::json!("COMMITTED")),
        "the next turn reads the committed global"
    );
    world.shutdown().await;
}

tiered_laws!(
    session_globals_survive_cells_and_reload_and_private_slots_never_do,
    rlm_message_append_keeps_the_committed_execution,
);
