//! A parked process's operations, judged by their contract alone, through
//! a host's `send()` on a served node (FIG-6915).
//!
//! A cell starts a detached TypeScript process whose body performs the
//! probe, parks on a sleep and performs it again. Its node is replaced by
//! one that serves the probe changed, and the process resumes there.
//!
//! - OP-CONTRACT-PIN: the process performs only under the requirement its
//!   code states. A probe whose input was retyped is served under another
//!   contract: the not-yet-issued perform raises `operation_contract_changed`,
//!   which the body catches, and no body runs.
//! - OP-LINK-ONE: nothing else about the live tool is compared. A probe
//!   whose description was reworded runs; one whose retry bounds alone
//!   changed (`Once` now) runs under the policy its link recorded, so its
//!   failed first attempt is retried.
//! - K-EFF-011: the call the process committed before it parked is never
//!   executed again, whatever the new node serves.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};

use lash_core::{ToolCall, ToolOutcome};
use lash_protocol_types::sync::MutexExt as _;

use served::{Tier, World};

/// The probe a process body calls.
const PROBE: &str = "contract_probe";

/// How long the process parks between its two performs: long enough for
/// its node to be replaced while it sleeps.
const SLEEP_MS: u64 = 5_000;

/// How a node serves the probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    /// As the process's first node served it.
    None,
    /// Its input retyped: another contract.
    Retyped,
    /// Its description reworded: the same contract.
    Reworded,
    /// Served `Once` rather than `Repeatable`: its retry bounds alone.
    Retried,
}

fn probe_definition(change: Change) -> lash_core::ToolDefinition {
    let mut input = serde_json::json!({ "type": "object", "additionalProperties": true });
    if change == Change::Retyped {
        input["properties"] = serde_json::json!({ "label": { "type": "integer" } });
    }
    lash_core::ToolDefinition::raw(
        format!("tool:{PROBE}"),
        format!("tools.{PROBE}"),
        if change == Change::Reworded {
            "Answers its label, reworded since the process started."
        } else {
            "Answers its label; a `fail_first` call fails its first attempt."
        },
        input,
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("the probe's schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(if change == Change::Retried {
        lash_core::ExecutionPolicy::Once
    } else {
        lash_core::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(3).expect("three attempts"),
            1,
            1,
        )
    })
}

/// Every probe body entry, by label and attempt, on every node.
#[derive(Default)]
struct Witness {
    entries: Mutex<Vec<(String, u32)>>,
}

impl Witness {
    fn of(&self, label: &str) -> Vec<u32> {
        self.entries
            .lock_recover()
            .iter()
            .filter(|(entered, _)| entered == label)
            .map(|(_, attempt)| *attempt)
            .collect()
    }
}

struct Probe {
    change: Change,
    witness: Arc<Witness>,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok(vec![probe_definition(self.change).manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == format!("tools.{PROBE}"))
            .then(|| Arc::new(probe_definition(self.change).contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let label = call.args["label"].as_str().unwrap_or_default().to_owned();
        let attempt = call.context.attempt_number();
        self.witness
            .entries
            .lock_recover()
            .push((label.clone(), attempt));
        if call.args["fail_first"].as_bool().unwrap_or_default() && attempt == 1 {
            return ToolOutcome::failure(lash_core::ToolFailure::with_suggested_delay(
                lash_core::ToolFailureClass::External,
                "transient",
                "the probe's first attempt fails",
                Some(1),
            ))
            .into();
        }
        ToolOutcome::ok(serde_json::json!({ "label": label })).into()
    }
}

/// The component that serves the probe to a process body.
const PROBE_COMPONENT: lash_core::ProcessConfigKey<()> =
    lash_core::ProcessConfigKey::new("contract-probe", lash_core::FormatVersion::ONE);

/// A core running RLM turns whose processes outlive them, serving the probe
/// as `change` makes it.
fn contract_core(
    backend: &lash::Backend,
    change: Change,
    witness: &Arc<Witness>,
) -> lash::LashCoreBuilder {
    let probe: Arc<dyn lash_core::ToolProvider> = Arc::new(Probe {
        change,
        witness: Arc::clone(witness),
    });
    let policy = lash_core::testing::run_policy_naming(&[
        PROBE_COMPONENT,
        lash::process_controls::PROCESS_CONTROLS_CONFIG,
    ]);
    lash::LashCore::rlm_builder(
        backend.clone(),
        served::rlm(backend, sim::untimed_workers()),
    )
    .plugin(Arc::new(
        lash::process_controls::SessionProcessAdminPluginFactory::new(
            lash_execution_contracts::process::lifetime::detached,
            policy,
        ),
    ))
    .process_component(
        Arc::new(lash::process_controls::ProcessControlsComponent::new(
            lash_execution_contracts::process::lifetime::detached,
            lash_core::testing::run_policy_naming(&[PROBE_COMPONENT]),
        )),
        lash::process_controls::PROCESS_CONTROLS_CONFIG,
    )
    .process_component(
        lash_core::testing::tool_component(PROBE_COMPONENT, Arc::clone(&probe)),
        PROBE_COMPONENT,
    )
    .plugin(lash_core::testing::tool_plugin(PROBE_COMPONENT, probe))
}

/// The cell that starts the process and finishes with its id. The process
/// commits one probe call, parks on a sleep, and then performs the probe
/// once more, catching what that perform raises.
fn start_cell() -> lash_core::llm::types::LlmResponse {
    served::cell(&format!(
        "const worker = async () => {{\n  \
           const first = await tools.contractProbe({{ label: \"committed\" }});\n  \
           await sleep({SLEEP_MS});\n  \
           let later = \"\";\n  \
           try {{\n    \
             later = (await tools.contractProbe({{ label: \"later\", fail_first: true }})).label;\n  \
           }} catch (e) {{ later = String(e); }}\n  \
           return {{ first: first.label, later: later }};\n\
         }};\n\
         const handle = await processes.start({{ definition: worker }});\n\
         await control.finish({{ process: handle.process_id }});"
    ))
}

/// Start the process on a node serving the probe as it was, wait for its
/// first call to commit, replace the node with one serving the probe under
/// `change`, and answer what the process returned there.
async fn resumed_under(tier: Tier, change: Change, witness: &Arc<Witness>) -> Option<String> {
    let built = Arc::clone(witness);
    let mut world = World::new(tier, move |backend| {
        contract_core(backend, Change::None, &built)
    })
    .await?;
    let name = format!("contract-{change:?}").to_lowercase();
    let output = world.run(&name, served::spec(64), vec![start_cell()]).await;
    served::assert_answered("the turn that starts the process", &output);
    let finish = output
        .finished()
        .map(|(_, value)| value.clone())
        .unwrap_or_else(|| panic!("the cell finished: {:?}", world.requests(&name).last()));
    let process = lash_core::ProcessId::parse(finish["process"].as_str().expect("a process id"))
        .expect("the handle names a process");
    tokio::time::timeout(served::WATCHDOG, async {
        while witness.of("committed").is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process's first call runs");
    // The first call's outcome commits within the group-commit window and
    // the process parks on its sleep.
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;

    let rebuilt = Arc::clone(witness);
    world
        .restart("contract-node-2", move |backend| {
            contract_core(backend, change, &rebuilt)
        })
        .await;
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        world.core.processes().await_output(&process),
    )
    .await
    .expect("the process ends within two minutes")
    .expect("its end is read");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = answer else {
        panic!("the process ended without an answer: {answer:?}");
    };
    assert!(
        output.is_success(),
        "the body caught what it met: {output:?}"
    );
    let value = output.value_for_projection();
    assert_eq!(value["first"], serde_json::json!("committed"), "{value}");
    assert_eq!(
        witness.of("committed"),
        [1],
        "the committed call never runs again (K-EFF-011)"
    );
    world.shutdown().await;
    Some(
        value["later"]
            .as_str()
            .expect("the later perform's answer")
            .to_owned(),
    )
}

/// OP-CONTRACT-PIN: a parked process whose tool changed its input schema
/// raises `operation_contract_changed`, naming the tool, at its
/// not-yet-issued perform, and no body runs for it.
async fn a_parked_process_meets_a_changed_contract_at_its_next_perform(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(later) = resumed_under(tier, Change::Retyped, &witness).await else {
        return;
    };
    // `operation_contract_changed` is the failure's code, its data; its
    // message alone says the tool serves another contract than the pin.
    assert!(
        later.starts_with("tool_failed")
            && later.contains("the call is pinned to; nothing ran")
            && later.contains(PROBE),
        "the perform raised the contract change, naming the tool: {later}"
    );
    assert!(witness.of("later").is_empty(), "no body ran for it");
}

/// OP-LINK-ONE: a parked process whose tool changed only its description
/// runs its next perform on the new node.
async fn a_parked_process_runs_on_a_reworded_tool(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(later) = resumed_under(tier, Change::Reworded, &witness).await else {
        return;
    };
    assert_eq!(later, "later");
    assert_eq!(
        witness.of("later"),
        [1, 2],
        "the call ran under its link's retry bounds"
    );
}

/// OP-LINK-ONE (C1): a parked process whose tool changed only its retry
/// bounds runs under the policy its link recorded: the live tool is `Once`,
/// and the call's failed first attempt is retried all the same.
async fn a_parked_process_runs_under_its_links_policy(tier: Tier) {
    let witness = Arc::new(Witness::default());
    let Some(later) = resumed_under(tier, Change::Retried, &witness).await else {
        return;
    };
    assert_eq!(later, "later", "the retried attempt answered");
    assert_eq!(
        witness.of("later"),
        [1, 2],
        "the link's `Repeatable` policy retried the failed first attempt"
    );
}

tiered_laws!(
    a_parked_process_meets_a_changed_contract_at_its_next_perform,
    a_parked_process_runs_on_a_reworded_tool,
    a_parked_process_runs_under_its_links_policy,
);
