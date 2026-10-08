//! A code cell's process handles, through a host's `send()` on a served
//! node (FIG-5310, ported from the deleted laws of
//! lash-protocol-rlm's `executor/tests/deferred_and_processes.rs`).
//!
//! An RLM turn's cell creates and starts TypeScript processes; the core's
//! node runs each process's body on the production process steps, and the
//! cell signals, awaits and inspects them through their handles.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_core::{ToolCall, ToolOutcome};
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};

/// The tool a cell hands a process id to.
const INSPECT: &str = "inspect";

fn inspect_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:status_tool/{INSPECT}"),
        INSPECT,
        "Inspects the process the call names by id.",
        object.clone(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the inspector's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["status_tool"], INSPECT))
}

/// Records every process id it was handed.
#[derive(Default)]
struct Inspector {
    inspected: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Inspector {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![inspect_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == INSPECT).then(|| Arc::new(inspect_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let process_id = call.args["process_id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        self.inspected.lock_recover().push(process_id);
        ToolOutcome::ok(serde_json::json!("inspected-ok")).into()
    }
}

/// A core running RLM turns whose processes outlive the turn that started
/// them, with the inspector.
async fn world(tier: Tier, inspector: &Arc<Inspector>) -> Option<World> {
    let inspector = Arc::clone(inspector);
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, None, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::detached,
            ),
        ))
        .tools(inspector)
    })
    .await
}

/// Every process the core's registry holds.
async fn processes(world: &World) -> Vec<lash_core::ProcessRecord> {
    world
        .backend
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("the registry lists its processes")
}

/// A signal a cell sends crosses the protocol and the process engine: the
/// process the cell started parks on `waitSignal`, the cell's signal
/// resolves it with its payload, and the process's terminal is that payload.
/// The start records the turn that started it.
async fn typescript_signal_round_trip_crosses_protocol_and_process_engine(tier: Tier) {
    let inspector = Arc::new(Inspector::default());
    let Some(world) = world(tier, &inspector).await else {
        return;
    };
    let cell = served::cell(
        "const worker = await processes.create({\n\
           dialect: \"typescript\",\n\
           source: 'const worker = async () => await waitSignal(\"ready\");'\n\
         });\n\
         const handle = await processes.start({ definition: worker });\n\
         await processes.signal({ handle: handle, name: \"ready\", payload: { ok: true } });\n\
         finish(await handle);",
    );
    let output = world
        .run("signal-round-trip", served::spec(64), vec![cell])
        .await;
    served::assert_answered("the turn that signals its process", &output);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!({ "ok": true })),
        "the process's terminal is the payload the cell signalled"
    );
    let records = processes(&world).await;
    let [record] = records.as_slice() else {
        panic!("the cell started exactly one process: {records:#?}");
    };
    assert!(record.is_terminal(), "the process ended: {record:#?}");
    let starter = record
        .ancestry
        .starter()
        .expect("a cell's start records its starter");
    assert_eq!(
        (starter.storage_kind(), starter.enclosing_session()),
        (
            "turn",
            Some(lash_core::ScopeId::session(
                lash::SessionId::try_from("signal-round-trip".to_owned()).expect("a session id")
            ))
        ),
        "the start records the turn that started it: {:?}",
        record.ancestry
    );
    world.shutdown().await;
}

/// A process handle a cell bound in one turn is still the process in the
/// session's next turn: that turn's cell awaits it and reads its terminal.
async fn typescript_restored_process_handle_await_crosses_turn_boundary(tier: Tier) {
    let inspector = Arc::new(Inspector::default());
    let Some(world) = world(tier, &inspector).await else {
        return;
    };
    world.script(
        "start-the-worker",
        vec![served::cell(
            "const worker = await processes.create({\n\
               dialect: \"typescript\",\n\
               source: 'const worker = async () => { return \"done\"; };'\n\
             });\n\
             const handle = await processes.start({ definition: worker });\n\
             finish(\"started\");",
        )],
    );
    world.script(
        "await-the-worker",
        vec![served::cell("finish(await handle);")],
    );
    let session = world.session("cross-turn-handle", served::spec(64)).await;
    let started = world.send(&session, "start-the-worker").await;
    served::assert_answered("the turn that starts the process", &started);
    assert_eq!(started.final_value(), Some(&serde_json::json!("started")));
    let awaited = world.send(&session, "await-the-worker").await;
    served::assert_answered("the next turn, which awaits the handle", &awaited);
    assert_eq!(
        awaited.final_value(),
        Some(&serde_json::json!("done")),
        "the next turn's cell awaits the process the first turn started"
    );
    world.shutdown().await;
}

/// A cell reads its handle's `process_id` and hands it to a later call: the
/// tool receives the id of the process the cell started, and the cell goes
/// on with the tool's answer.
async fn typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation(tier: Tier) {
    let inspector = Arc::new(Inspector::default());
    let Some(world) = world(tier, &inspector).await else {
        return;
    };
    let cell = served::cell(
        "const worker = await processes.create({\n\
           dialect: \"typescript\",\n\
           source: 'const worker = async () => { return \"done\"; };'\n\
         });\n\
         const handle = await processes.start({ definition: worker });\n\
         const processId = handle.process_id;\n\
         const status = await status_tool.inspect({ process_id: processId });\n\
         finish({ id: processId, status: status });",
    );
    let output = world
        .run("handle-process-id", served::spec(64), vec![cell])
        .await;
    served::assert_answered("the turn that inspects its process", &output);
    let finish = output
        .final_value()
        .expect("the cell finished with a value");
    let id = finish["id"].as_str().expect("the handle's id is a string");
    assert!(!id.is_empty(), "the handle's id is not empty");
    assert_eq!(finish["status"], serde_json::json!("inspected-ok"));
    assert_eq!(
        inspector.inspected.lock_recover().as_slice(),
        [id.to_owned()],
        "the tool received the id the cell read off its handle"
    );
    let records = processes(&world).await;
    assert_eq!(
        records
            .iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>(),
        [id.to_owned()],
        "the id is the started process's"
    );
    world.shutdown().await;
}

tiered_laws!(
    typescript_signal_round_trip_crosses_protocol_and_process_engine,
    typescript_restored_process_handle_await_crosses_turn_boundary,
    typescript_cell_reads_process_handle_id_and_invokes_subsequent_operation,
);
