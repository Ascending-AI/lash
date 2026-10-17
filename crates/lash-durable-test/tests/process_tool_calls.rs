//! The tool calls a process body issues, through a host's `send()` on a
//! served node (ported by FIG-5216 from the deleted lash-conformance
//! `tool_call_identity/admission.rs` and `tool_batch_parallelism/limit.rs`
//! process laws).
//!
//! An RLM turn's cell starts Lash VM processes; the core's node runs each
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
use lash_protocol_types::sync::MutexExt as _;

use served::{Tier, World};

/// The probe a process body calls.
const PROBE: &str = "process_probe";
/// The probe that parks on its completion key and resolves it itself.
const DEFERRED: &str = "process_deferred";

/// One execution of the probe's body.
#[derive(Clone, Debug)]
struct Execution {
    /// The call's `label` argument: which logical call the law meant.
    label: String,
    call_id: lash_core::ToolCallId,
    /// Who the call ran for.
    owner: String,
    /// The process the call names as the one it runs inside.
    enclosing: Option<lash_core::ProcessId>,
    /// The completion key a deferred probe parked on.
    completion_key: Option<String>,
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

fn probe_definition(name: &str) -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    let definition = lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Records its call and answers its label; a held call waits for the law.",
        object.clone(),
        object,
    )
    .expect("the probe's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], name));
    if name != DEFERRED {
        return definition;
    }
    definition
        .with_declaration(
            lash_core::ToolDeclaration::deferring(),
            Some(lash_core::ParkBound::Within(
                std::time::Duration::from_secs(120),
            )),
        )
        .expect("a deferring tool declares its park bound")
}

struct Probe {
    witness: Arc<Witness>,
    backend: lash::Backend,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok({
            [PROBE, DEFERRED]
                .into_iter()
                .map(|name| probe_definition(name).manifest())
                .collect()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        [PROBE, DEFERRED]
            .contains(&name)
            .then(|| Arc::new(probe_definition(name).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        let completion_key = (call.name() == DEFERRED).then(|| {
            call.context
                .completion_key()
                .expect("a deferring call's completion wait is pinned")
        });
        self.witness.executions.lock_recover().push(Execution {
            label: label.clone(),
            call_id: call.context.call_id().clone(),
            owner: call.context.owner().runtime_owner().to_string(),
            enclosing: call.context.enclosing_process().cloned(),
            completion_key: completion_key.as_ref().map(|key| key.as_str().to_owned()),
        });
        // A deferred call resolves its own key with its own label.
        if let Some(key) = completion_key {
            let backend = self.backend.clone();
            tokio::spawn(async move {
                lash_core::waits::resolve_host(
                    &backend,
                    key.as_str(),
                    lash_core::Resolution::Ok(serde_json::json!({ "label": label })),
                )
                .await
                .expect("the deferred probe's wait resolves");
            });
            return lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new());
        }
        if call.args["hold"].as_bool().unwrap_or_default() {
            let mut gate = self.witness.gate.subscribe();
            let _ = gate.wait_for(|open| *open).await;
        }
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// The component that serves the probe to a process body.
const PROBE_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("process-probe", lash_core::FormatVersion::ONE);

/// A core running RLM turns with the probe, its model the registered
/// scripts. A process a cell starts runs under the turn's run with the
/// probe's component.
async fn world(tier: Tier, witness: &Arc<Witness>) -> Option<World> {
    let witness = Arc::clone(witness);
    World::new(tier, move |backend| {
        let probe: Arc<dyn lash_core::ToolProvider> = Arc::new(Probe {
            witness,
            backend: backend.clone(),
        });
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[
                    PROBE_COMPONENT,
                    lash::process_controls::PROCESS_CONTROLS_CONFIG,
                ]),
            ),
        ))
        // A body's `await` of an unknown value links `processes.await`
        // (P2): the env names the process controls that serve it.
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[PROBE_COMPONENT]),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .process_component(
            lash_core::testing::tool_component(PROBE_COMPONENT, Arc::clone(&probe)),
            PROBE_COMPONENT,
        )
        .plugin(lash_core::testing::tool_plugin(PROBE_COMPONENT, probe))
    })
    .await
}

/// A cell that defines one process body, one probe call under the label it
/// is started with, and starts it once per label. The cell answers every
/// terminal.
fn process_cell(labels: &[&str]) -> lash_core::llm::types::LlmResponse {
    let body = format!(
        "const body = async (label: string) => {{\n  return await tools.{PROBE}({{ label: label }});\n}};"
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
    served::cell(&format!("{starts}\nawait control.finish([{awaits}]);"))
}

/// A process body's call is named by the process it runs in: the same
/// statement at the same position of two processes is two identities, each
/// owned by its process and naming it as its enclosing process, and each
/// body runs once.
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
    for execution in [&one, &two] {
        assert_eq!(
            execution
                .enclosing
                .as_ref()
                .map(|process| format!("process:{process}")),
            Some(execution.owner.clone()),
            "a call names the process it runs inside: {execution:?}"
        );
    }
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
        "{body}\n\
         const held = await processes.start({{ definition: body }});\n\
         await control.finish(await held);"
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
    let processes = lash_core::testing::process_roster_records_for_fixture(
        world.backend.process_registry().as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
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
        "const body = async () => {{\n{awaits}  return \"done\";\n}};\n\
         const held = await processes.start({{ definition: body }});\n\
         await control.finish(await held);"
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

/// A TypeScript process body reads the clock and the random source through
/// the runtime: `new Date().toISOString()`, `Date.now()` and `Math.random()`
/// answer real values and the process completes (FIG-3079, ported by FIG-5307
/// from the deleted lash-protocol-rlm `typescript_runtime_values.rs`).
async fn typescript_process_body_resolves_journaled_clock_and_randomness(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let cell = served::cell(
        "const worker = async () => {\n  \
            const stamp = new Date().toISOString();\n  \
            const ms = Date.now();\n  \
            const roll = Math.random();\n  \
            return { stamp: stamp, ms: ms, roll: roll };\n};\n\
         const held = await processes.start({ definition: worker });\n\
         await control.finish(JSON.stringify(await held));",
    );
    let output = world
        .run("process-runtime-values", served::spec(64), vec![cell])
        .await;
    served::assert_answered("the turn that awaits the process", &output);
    let answer = output
        .assistant_message()
        .map(str::to_owned)
        .or_else(|| {
            output
                .finished()
                .map(|(_, value)| value)
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| panic!("the turn answers the process's output: {output:?}"));
    let value: serde_json::Value =
        serde_json::from_str(&answer).unwrap_or_else(|error| panic!("{error}: {answer}"));
    let stamp = value["stamp"].as_str().expect("the ISO stamp is a string");
    assert!(
        stamp.ends_with('Z') && stamp.len() == 24 && stamp.as_bytes()[10] == b'T',
        "`new Date().toISOString()` returns an ISO-8601 stamp, got {stamp}"
    );
    let ms = value["ms"].as_f64().expect("`Date.now()` returns a number");
    assert!(ms > 0.0, "`Date.now()` returns a positive epoch, got {ms}");
    let roll = value["roll"]
        .as_f64()
        .expect("`Math.random()` returns a number");
    assert!(
        (0.0..1.0).contains(&roll),
        "`Math.random()` stays in [0, 1), got {roll}"
    );
    world.shutdown().await;
}

/// A pruned process takes its ledger and its waits with it (FIG-5939): the
/// run facts of the call its body parked, and the completion wait it parked
/// on, are gone once the process is pruned, and so is the call the host
/// read back from them before.
async fn a_pruned_process_leaves_no_run_fact_or_wait(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(world) = world(tier, &witness).await else {
        return;
    };
    let body = format!(
        "const body = async (label: string) => {{\n  return await tools.{DEFERRED}({{ label: label }});\n}};\n\
         const handle = await processes.start({{ definition: body, args: {{ label: \"pruned\" }} }});\n\
         await control.finish(await handle);"
    );
    let output = world
        .run(
            "pruned-process",
            served::spec(64),
            vec![served::cell(&body)],
        )
        .await;
    served::assert_answered("the turn that awaits a parked process", &output);
    let call = witness.only("pruned");
    let process = call.enclosing.clone().expect("the call names its process");
    let wait = lash_durable::domain::WaitId::parse_hex(
        call.completion_key.as_deref().expect("the call parked"),
    )
    .expect("a completion key is its wait's id");
    let owner = lash_durable::domain::OwnerKey::Process(process.clone());
    let durable = world.backend.durable();
    let processes = world.core.processes();
    assert!(
        processes
            .tool_call(&process, &call.call_id)
            .await
            .unwrap()
            .is_some(),
        "the host reads the ended process's call"
    );
    assert!(!durable.run_facts(&owner).await.unwrap().rows.is_empty());
    assert!(durable.wait(&wait).await.unwrap().is_some());

    let now = world.backend.clock().timestamp_ms();
    let report = processes
        .prune(now + 1, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .unwrap();
    assert_eq!(report.pruned_processes, 1, "{report:?}");
    assert!(
        durable.run_facts(&owner).await.unwrap().rows.is_empty(),
        "the pruned process's run facts remain"
    );
    assert!(
        durable.wait(&wait).await.unwrap().is_none(),
        "the pruned process's wait remains"
    );
    assert!(
        processes
            .tool_call(&process, &call.call_id)
            .await
            .unwrap()
            .is_none(),
        "the pruned process's call still reads"
    );
    world.shutdown().await;
}

/// A process environment store that fails, once, the first environment read
/// a tool step's body makes: what the body would meet rebuilding the
/// composition its activation already prepared. A read is the body's when
/// the worker's tool step is on its stack; `activation_reads` counts the
/// activation's own reads, so the law fails loudly where frames are not
/// symbolized.
struct BodyReadFault {
    inner: Arc<dyn lash_core::ProcessEnvStore>,
    armed: std::sync::atomic::AtomicBool,
    activation_reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl BodyReadFault {
    fn over(
        inner: Arc<dyn lash_core::ProcessEnvStore>,
        activation_reads: Arc<std::sync::atomic::AtomicUsize>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inner,
            armed: std::sync::atomic::AtomicBool::new(true),
            activation_reads,
        })
    }
}

impl lash_core::store::FleetFormatStore for BodyReadFault {
    fn fleet_format(&self) -> lash_core::FleetFormat {
        self.inner.fleet_format()
    }

    fn plugin_writers(&self) -> lash_core::store::PluginWriterRangesFuture<'_> {
        self.inner.plugin_writers()
    }

    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [lash_core::store::plugin_writers::PluginWriterRegistration],
    ) -> lash_core::store::PluginWriterRangesFuture<'a> {
        self.inner.provision_plugin_writers(registrations)
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessEnvStore for BodyReadFault {
    async fn publish_process_env(
        &self,
        claim: &lash_core::ReferrerClaim,
        env_ref: &lash_core::ProcessEnvRef,
        bytes: &[u8],
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.inner.publish_process_env(claim, env_ref, bytes).await
    }

    async fn acquire_process_env(
        &self,
        claim: &lash_core::ReferrerClaim,
        env_ref: &lash_core::ProcessEnvRef,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.inner.acquire_process_env(claim, env_ref).await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), lash_core::ArtifactStoreError> {
        self.inner.end_process_env_referrer(cleanup).await
    }

    async fn get_process_env(
        &self,
        env_ref: &lash_core::ProcessEnvRef,
    ) -> Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        let stack = std::backtrace::Backtrace::force_capture().to_string();
        if stack.contains("ProcessActivation>::pass") {
            self.activation_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        if stack.contains("DurableProcessWorker>::tool_step")
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(lash_core::ArtifactStoreError::Backend(
                "the law's connection reset".to_owned(),
            ));
        }
        self.inner.get_process_env(env_ref).await
    }
}

/// An activation's steps run on the tools its `prepare` built (FIG-6061): a
/// retryable store fault a step body would meet rebuilding them never
/// settles the step. The process's body issues one tool step, and the
/// environment store fails the first read a tool step's body makes; the
/// body reads none, so the tool runs once and the process answers its
/// result.
async fn a_store_fault_after_prepare_never_settles_a_step(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let probe_witness = Arc::clone(&witness);
    let activation_reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let store_reads = Arc::clone(&activation_reads);
    let Some(world) = World::new(tier, move |backend| {
        let layered = lash_core::testing::runtime_helpers::LayeredBackend::over(backend.clone())
            .map_process_env_store(|inner| {
                BodyReadFault::over(inner, store_reads) as Arc<dyn lash_core::ProcessEnvStore>
            })
            .into_backend();
        let probe: Arc<dyn lash_core::ToolProvider> = Arc::new(Probe {
            witness: probe_witness,
            backend: layered.clone(),
        });
        lash::LashCore::rlm_builder(
            layered.clone(),
            served::rlm(&layered, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[
                    PROBE_COMPONENT,
                    lash::process_controls::PROCESS_CONTROLS_CONFIG,
                ]),
            ),
        ))
        // A body's `await` of an unknown value links `processes.await`
        // (P2): the env names the process controls that serve it.
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[PROBE_COMPONENT]),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .process_component(
            lash_core::testing::tool_component(PROBE_COMPONENT, Arc::clone(&probe)),
            PROBE_COMPONENT,
        )
        .plugin(lash_core::testing::tool_plugin(PROBE_COMPONENT, probe))
    })
    .await
    else {
        return;
    };
    let output = world
        .run(
            "store-fault-after-prepare",
            served::spec(64),
            vec![process_cell(&["after-prepare"])],
        )
        .await;
    served::assert_answered("the turn that awaits the process", &output);
    let processes = lash_core::testing::process_roster_records_for_fixture(
        world.backend.process_registry().as_ref(),
        &lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        },
    )
    .await
    .expect("the registry lists its processes");
    let [process] = processes.as_slice() else {
        panic!("one process: {processes:#?}");
    };
    let Some(lash_core::ProcessAwaitOutput::Settled { output }) = process.outcome() else {
        panic!("the process settled: {process:#?}");
    };
    assert!(
        matches!(&output.outcome, lash_core::ToolCallOutcome::Success(_)),
        "the process answers its step's result, not a settled build fault: {output:?}"
    );
    witness.only("after-prepare");
    assert!(
        activation_reads.load(std::sync::atomic::Ordering::SeqCst) > 0,
        "the fault tells the activation's reads from a body's"
    );
    world.shutdown().await;
}

/// The all-optional tool the omission law calls: no field is required, and
/// one declares a default that an omitted input must not be given.
const SEARCH: &str = "search";

fn search_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:workflows_search",
        SEARCH,
        "Answers the input it was called with.",
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": { "type": "string" },
                "limit": { "type": "integer", "default": 10 },
            },
        }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("the search tool's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["workflows"], "search"))
}

/// The inputs the search tool was prepared and executed with, each execution
/// with whether a process body made the call.
#[derive(Default)]
struct SearchInputs {
    prepared: Mutex<Vec<serde_json::Value>>,
    executed: Mutex<Vec<(bool, serde_json::Value)>>,
}

struct Search(Arc<SearchInputs>);

#[async_trait::async_trait]
impl lash_core::ToolProvider for Search {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash_core::PluginError> {
        Ok(vec![search_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == SEARCH).then(|| Arc::new(search_definition().contract()))
    }

    async fn prepare_tool_call(
        &self,
        call: lash_core::ToolPrepareCall<'_>,
    ) -> Result<lash_core::PreparedToolCall, ToolOutcome> {
        self.0
            .prepared
            .lock_recover()
            .push(call.pending.args.clone());
        Ok(lash_core::PreparedToolCall::identity(
            call.tool_id,
            call.pending,
        ))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.executed.lock_recover().push((
            call.context.enclosing_process().is_some(),
            call.args.clone(),
        ));
        ToolOutcome::ok(serde_json::json!({ "seen": call.args })).into()
    }
}

/// The component that serves the search tool to a process body.
const SEARCH_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("workflows-search", lash_core::FormatVersion::ONE);

/// A core running RLM turns in `dialect` with the search tool, in its cells
/// and in the processes they start.
async fn search_world(
    tier: Tier,
    dialect: lash::rlm::CellDialect,
    inputs: &Arc<SearchInputs>,
) -> Option<World> {
    let search: Arc<dyn lash_core::ToolProvider> = Arc::new(Search(Arc::clone(inputs)));
    World::new(tier, move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            lash::rlm::RlmProtocolPluginFactory::new(
                lash::rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash::rlm::RlmChannel::Cell)
                    .instruction_limit(lash::rlm::InstructionBound::instructions(10_000_000))
                    .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
                    .build(),
                dialect,
            )
            .with_worker_service(sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[
                    SEARCH_COMPONENT,
                    lash::process_controls::PROCESS_CONTROLS_CONFIG,
                ]),
            ),
        ))
        .process_component(
            Arc::new(lash::process_controls::ProcessControlsComponent::new(
                lash_core::lifetime::starter,
                lash_core::testing::run_policy_naming(&[SEARCH_COMPONENT]),
            )),
            lash::process_controls::PROCESS_CONTROLS_CONFIG,
        )
        .process_component(
            lash_core::testing::tool_component(SEARCH_COMPONENT, Arc::clone(&search)),
            SEARCH_COMPONENT,
        )
        .plugin(lash_core::testing::tool_plugin(SEARCH_COMPONENT, search))
    })
    .await
}

/// FIG-6294: a tool whose input schema accepts `{}` may be called with its
/// input omitted, and the omitted input is exactly `{}`: not null, not the
/// schema's defaults. `workflows.search()` and `workflows.search({})` reach
/// the provider's prepare and execute with the same input, and the guest
/// reads the same answer from both: inline in a TypeScript and a Python
/// cell, and in a TypeScript process whose body first parks on a durable
/// sleep and resumes from its saved run. (A Python process body links no
/// catalog tool: its effect `workflows_search` is not the binding's path.)
async fn omitted_tool_input_is_exactly_empty_object(tier: Tier) {
    let typescript = (
        lash::rlm::CellDialect::typescript(),
        "const body = async () => {\n  \
           await sleep(10);\n  \
           return [await workflows.search(), await workflows.search({})];\n\
         };\n\
         const run = await processes.start({ definition: body });\n\
         const cell = [await workflows.search(), await workflows.search({})];\n\
         await control.finish({ cell: cell, process: await run });",
        true,
    );
    let python = (
        lash::rlm::CellDialect::python(),
        "cell = [await workflows_search(), await workflows_search({})]\n\
         await control_finish({'cell': cell})",
        false,
    );
    for (dialect, source, process) in [typescript, python] {
        let name = dialect.name().to_owned();
        let inputs = Arc::new(SearchInputs::default());
        let Some(world) = search_world(tier, dialect, &inputs).await else {
            return;
        };
        let cell = served::response(vec![lash_core::llm::types::LlmOutputPart::Text {
            text: format!("<{name}>\n{source}\n</{name}>"),
            response_meta: None,
        }]);
        let output = world
            .run(
                &format!("omitted-input-{name}"),
                served::spec(64),
                vec![cell],
            )
            .await;
        served::assert_answered(&format!("the {name} turn"), &output);
        let empty = serde_json::json!({});
        let answered = serde_json::json!([{ "seen": {} }, { "seen": {} }]);
        let mut expected = serde_json::json!({ "cell": answered });
        if process {
            expected["process"] = answered;
        }
        assert_eq!(
            output.finished().map(|(_, value)| value.clone()),
            Some(expected),
            "{name}: the guest reads one answer for an omitted and an empty input: {:?}",
            world.requests(&format!("omitted-input-{name}"))
        );
        let calls = if process { 4 } else { 2 };
        assert_eq!(
            *inputs.prepared.lock_recover(),
            vec![empty.clone(); calls],
            "{name}: every call is prepared with exactly {{}}"
        );
        let mut executed = inputs.executed.lock_recover().clone();
        executed.sort_by_key(|(in_process, _)| *in_process);
        let mut expected = vec![(false, empty.clone()); 2];
        if process {
            expected.extend([(true, empty.clone()), (true, empty)]);
        }
        assert_eq!(
            executed, expected,
            "{name}: the cell's and the process's calls execute with exactly {{}}"
        );
        world.shutdown().await;
    }
}

tiered_laws!(
    a_pruned_process_leaves_no_run_fact_or_wait,
    a_store_fault_after_prepare_never_settles_a_step,
    process_admission_names_each_call,
    tool_call_limit_counts_what_a_process_holds,
    process_tool_call_limit_admits_the_limit_and_refuses_the_group_past_it,
    process_tool_call_limit_staged_calls,
    typescript_process_body_resolves_journaled_clock_and_randomness,
    omitted_tool_input_is_exactly_empty_object,
);
