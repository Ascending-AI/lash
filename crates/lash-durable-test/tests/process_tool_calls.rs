//! The tool calls a process body issues, through a host's `send()` on a
//! served node (ported by FIG-5216 from the deleted lash-conformance
//! `tool_call_identity/admission.rs` and `tool_batch_parallelism/limit.rs`
//! process laws).
//!
//! An RLM turn's cell starts Lashlang processes; the core's node runs each
//! process's `vm_run` steps and the catalog tool steps its body issues, on
//! the production process steps, and the cell awaits their terminals.

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

/// The probe a process body calls.
const PROBE: &str = "process_probe";

/// One execution of the probe's body.
#[derive(Clone, Debug)]
struct Execution {
    /// The call's `label` argument: which logical call the law meant.
    label: String,
    call_id: lash_core::ToolCallId,
    /// Who the call ran for.
    owner: String,
}

/// Everything the probe did, and the gate a held call waits on.
struct Witness {
    executions: Mutex<Vec<Execution>>,
    gate: tokio::sync::watch::Sender<bool>,
}

impl Default for Witness {
    fn default() -> Self {
        Self {
            executions: Mutex::default(),
            gate: tokio::sync::watch::channel(false).0,
        }
    }
}

impl Witness {
    fn of(&self, label: &str) -> Vec<Execution> {
        self.executions
            .lock_recover()
            .iter()
            .filter(|execution| execution.label == label)
            .cloned()
            .collect()
    }

    /// The one execution of `label`'s body.
    fn only(&self, label: &str) -> Execution {
        let executions = self.of(label);
        assert_eq!(
            executions.len(),
            1,
            "`{label}`'s body ran exactly once: {executions:?}"
        );
        executions[0].clone()
    }
}

fn probe_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE}"),
        PROBE,
        "Records its call and answers its label; a held call waits for the law.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], PROBE))
}

struct Probe {
    witness: Arc<Witness>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == PROBE).then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        self.witness.executions.lock_recover().push(Execution {
            label: label.clone(),
            call_id: call.context.call_id().clone(),
            owner: call.context.owner().runtime_owner().to_string(),
        });
        if call.args["hold"].as_bool().unwrap_or_default() {
            let mut gate = self.witness.gate.subscribe();
            let _ = gate.wait_for(|open| *open).await;
        }
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// A core running RLM turns with the probe, its model the registered
/// scripts.
async fn world(tier: Tier, witness: &Arc<Witness>) -> Option<World> {
    let witness = Arc::clone(witness);
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, None, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::starter,
            ),
        ))
        .tools(Arc::new(Probe { witness }))
    })
    .await
}

/// A cell that defines one process body, one probe call under the label it
/// is started with, and starts it once per label. The cell answers every
/// terminal.
fn process_cell(labels: &[&str]) -> lash_core::llm::types::LlmResponse {
    let body = format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: \
         `const body = async (label: string) => {{\n  return await tools.{PROBE}({{ label: label }});\n}};` }});"
    );
    let starts = labels
        .iter()
        .enumerate()
        .map(|(index, label)| {
            format!(
                "const h{index} = await processes.start({{ definition: body, args: {{ label: \"{label}\" }} }});"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let starts = format!("{body}\n{starts}");
    let awaits = (0..labels.len())
        .map(|index| format!("await h{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    served::cell(&format!("{starts}\nfinish([{awaits}]);"))
}

/// A process body's call is named by the process it runs in: the same
/// statement at the same position of two processes is two identities, each
/// owned by its process, and each body runs once.
async fn process_admission_names_each_call(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let output = world
        .run(
            "process-admission",
            served::spec(64),
            vec![process_cell(&["process-one", "process-two"])],
        )
        .await;
    served::assert_answered("the turn that starts two processes", &output);
    let one = witness.only("process-one");
    let two = witness.only("process-two");
    assert_ne!(
        one.call_id, two.call_id,
        "the same call at the same position of two processes is two identities"
    );
    assert!(
        one.owner.starts_with("process:")
            && two.owner.starts_with("process:")
            && one.owner != two.owner,
        "each call runs for its own process: {one:?}, {two:?}"
    );
    world.shutdown().await;
}

/// The `max_tool_calls` the holding law's session records.
const LIMIT: usize = 2;

/// A process that holds `max_tool_calls` calls is refused one more while it
/// holds them (FIG-4546): it races `LIMIT` calls, its winner answers and its
/// loser stays in flight, so the race is still held whole; the one further
/// call is refused without running, and the process fails with the typed
/// refusal, counting the race once.
async fn tool_call_limit_counts_what_a_process_holds(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let body = format!(
        "const body = async () => {{\n  \
           const raced = await Promise.race([tools.{PROBE}({{ label: \"winner\" }}), \
           tools.{PROBE}({{ label: \"loser\", hold: true }})]);\n  \
           await tools.{PROBE}({{ label: \"refused\" }});\n  \
           return raced;\n}};"
    );
    let cell = served::cell(&format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: `{body}` }});\n\
         const held = await processes.start({{ definition: body }});\n\
         finish(await held);"
    ));
    let _ = world
        .run("process-holding", served::spec(LIMIT), vec![cell])
        .await;
    assert_eq!(witness.of("winner").len(), 1, "the race's winner answered");
    assert_eq!(
        witness.of("loser").len(),
        1,
        "the race's loser is in flight"
    );
    assert!(
        witness.of("refused").is_empty(),
        "the call past the limit never ran: {:?}",
        witness.of("refused")
    );
    let processes = world
        .backend
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("the registry lists its processes");
    let held = processes
        .iter()
        .find(|process| process.is_terminal())
        .unwrap_or_else(|| panic!("the holding process ended: {processes:#?}"));
    let Some(lash_core::ProcessAwaitOutput::Settled { output }) = held.outcome() else {
        panic!("the holding process settled: {held:#?}");
    };
    let lash_core::ToolCallOutcome::Failure(failure) = &output.outcome else {
        panic!("the holding process failed: {output:?}");
    };
    assert_eq!(
        failure.code,
        lash_core::ToolCallLimitExceeded::CODE,
        "{failure:?}"
    );
    let exceeded = lash_core::ToolCallLimitExceeded {
        scope: lash_core::ToolCallLimitScope::Process,
        limit: lash_core::MaxToolCalls::new(LIMIT),
        counted: LIMIT,
        requested: 1,
    };
    assert_eq!(
        failure.raw.as_ref().map(|raw| raw.to_json_value()),
        Some(serde_json::json!({ "tool_call_limit": exceeded })),
        "the refusal counts the held race once and names the call past it: {failure:?}"
    );
    world.shutdown().await;
}

/// A cell that starts one process whose body awaits each of `groups` in
/// turn, every group one `Promise.all` of probe calls, and answers its
/// terminal.
fn groups_cell(groups: &[&[&str]]) -> lash_core::llm::types::LlmResponse {
    let awaits = groups
        .iter()
        .map(|group| {
            let calls = group
                .iter()
                .map(|label| format!("tools.{PROBE}({{ label: \"{label}\" }})"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("  await Promise.all([{calls}]);\n")
        })
        .collect::<String>();
    served::cell(&format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: \
         `const body = async () => {{\n{awaits}  return \"done\";\n}};` }});\n\
         const held = await processes.start({{ definition: body }});\n\
         finish(await held);"
    ))
}

/// The refusal a process holding `counted` calls is shown when it asks for
/// `requested` more, worded as the model reads it.
fn process_refusal(counted: usize, requested: usize) -> String {
    lash_core::ToolCallLimitExceeded {
        scope: lash_core::ToolCallLimitScope::Process,
        limit: lash_core::MaxToolCalls::new(LIMIT),
        counted,
        requested,
    }
    .to_string()
}

/// Runs the process of `groups` in a turn of session `name` under [`LIMIT`];
/// what every request of the turn showed the model, joined.
async fn run_groups(world: &World, name: &str, groups: &[&[&str]]) -> String {
    let _ = world
        .run(name, served::spec(LIMIT), vec![groups_cell(groups)])
        .await;
    world.requests(name).join("\n")
}

/// A process's group of exactly `max_tool_calls` calls runs as it always
/// did, and a group of one more is refused whole: none of its calls starts,
/// and the turn that awaits the process is shown the refusal naming the
/// limit (FIG-4546).
async fn process_tool_call_limit_admits_the_limit_and_refuses_the_group_past_it(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let at: &[&str] = &["at-0", "at-1"];
    let shown = run_groups(&world, "process-limit-at", &[at]).await;
    for label in at {
        witness.only(label);
    }
    assert!(
        !shown.contains("tool call limit exceeded"),
        "a group of exactly max_tool_calls calls is not refused: {shown}"
    );

    let past: &[&str] = &["past-0", "past-1", "past-2"];
    let shown = run_groups(&world, "process-limit-past", &[past]).await;
    for label in past {
        assert!(
            witness.of(label).is_empty(),
            "no call of a refused group runs: {label}"
        );
    }
    let refusal = process_refusal(0, LIMIT + 1);
    assert!(
        shown.contains(&refusal),
        "the turn is shown the refusal, naming the limit: {refusal}\n{shown}"
    );
    world.shutdown().await;
}

/// A group a process consumed is no longer held: a second group of
/// `max_tool_calls` calls runs, and one of a call more is refused while the
/// calls before it ran untouched (FIG-4546).
async fn process_tool_call_limit_staged_calls(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let first: &[&str] = &["twice-0", "twice-1"];
    let second: &[&str] = &["twice-2", "twice-3"];
    let shown = run_groups(&world, "process-limit-twice", &[first, second]).await;
    for label in first.iter().chain(second) {
        witness.only(label);
    }
    assert!(
        !shown.contains("tool call limit exceeded"),
        "two groups of max_tool_calls calls in sequence are not refused: {shown}"
    );

    let first: &[&str] = &["staged-0", "staged-1"];
    let past: &[&str] = &["staged-2", "staged-3", "staged-4"];
    let shown = run_groups(&world, "process-limit-staged", &[first, past]).await;
    for label in first {
        witness.only(label);
    }
    for label in past {
        assert!(
            witness.of(label).is_empty(),
            "no call of the refused group runs: {label}"
        );
    }
    let refusal = process_refusal(0, LIMIT + 1);
    assert!(
        shown.contains(&refusal),
        "the turn is shown the refusal at the group past the limit: {refusal}\n{shown}"
    );
    world.shutdown().await;
}

/// Run a turn of session `name` whose cell starts one process from `source`
/// and awaits it; the process's terminal and the trigger subscriptions the
/// store holds once it ended.
async fn run_trigger_process(
    world: &World,
    name: &str,
    source: &str,
) -> (
    lash_core::ProcessAwaitOutput,
    Vec<lash_core::TriggerSubscriptionRecord>,
) {
    let cell = served::cell(&format!(
        "const body = await processes.create({{ dialect: \"typescript\", source: `{source}` }});\n\
         const held = await processes.start({{ definition: body }});\n\
         finish(await held);"
    ));
    let output = world.run(name, served::spec(64), vec![cell]).await;
    served::assert_answered("the turn that awaits the trigger process", &output);
    let processes = world
        .backend
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await
        .expect("the registry lists its processes");
    let [process] = processes.as_slice() else {
        panic!("the cell started one process: {processes:#?}");
    };
    let terminal = process
        .outcome()
        .unwrap_or_else(|| panic!("the process ended: {process:#?}"));
    let subscriptions = world
        .backend
        .trigger_store()
        .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
        .await
        .expect("the trigger store lists its subscriptions");
    (terminal, subscriptions)
}

/// A process body's `triggers.list` runs through the trigger command handler
/// as a step of its process: the process answers the empty registry, and
/// listing installs nothing.
async fn process_body_uses_trigger_command_handler(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let (terminal, subscriptions) = run_trigger_process(
        &world,
        "process-trigger-list",
        "const registrar = async () => await triggers.list({});",
    )
    .await;
    assert_eq!(
        terminal,
        lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!([])
        )),
        "the process answers the trigger registry it listed"
    );
    assert!(subscriptions.is_empty(), "{subscriptions:#?}");
    world.shutdown().await;
}

/// A helper local to a process body reaches the trigger command handler as
/// the body itself does.
async fn process_local_helper_reaches_trigger_command_handler(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let (terminal, subscriptions) = run_trigger_process(
        &world,
        "process-trigger-helper",
        "const registrar = async () => {\n  \
           const listRegistrations = () => triggers.list({});\n  \
           return await listRegistrations();\n};",
    )
    .await;
    assert_eq!(
        terminal,
        lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
            serde_json::json!([])
        )),
        "the helper's listing is the process's answer"
    );
    assert!(subscriptions.is_empty(), "{subscriptions:#?}");
    world.shutdown().await;
}

tiered_laws!(
    process_admission_names_each_call,
    tool_call_limit_counts_what_a_process_holds,
    process_tool_call_limit_admits_the_limit_and_refuses_the_group_past_it,
    process_tool_call_limit_staged_calls,
    process_body_uses_trigger_command_handler,
    process_local_helper_reaches_trigger_command_handler,
);
