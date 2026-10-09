//! One worker slot, shared by a turn's cells and its process engine as a
//! production RLM registration shares it (FIG-4275, FIG-4707; ported by
//! FIG-5308 from the deleted lash-protocol-rlm `one_slot_process_await.rs`
//! cell laws).
//!
//! A cell that starts a process and awaits it parks while the process runs,
//! so the one slot runs both; a `processes.create` that finds the slot
//! saturated is a retryable host fault the engine attempts again, never a
//! recorded tool refusal.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::{Arc, Mutex};

use lash_core::ToolDefinitionBindingExt as _;
use lash_sansio::sync::MutexExt as _;

use served::{Tier, World};

/// A worker service of exactly one slot, with a checkout deadline of
/// `checkout`: long enough for any legitimate wait, short enough to report a
/// deadlock or a saturated slot.
fn one_slot_workers(checkout: std::time::Duration) -> lash::vm::WorkerService {
    let mut config = sim::untimed_workers().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    config.deadlines.checkout = checkout;
    lash::vm::WorkerService::new(config)
}

/// A core running RLM turns whose cells and process engine share `workers`,
/// with the session process controls and `tools`.
async fn world(
    tier: Tier,
    workers: lash::vm::WorkerService,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Option<World> {
    World::new(tier, move |backend| {
        let builder =
            lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, None, workers))
                .plugin(Arc::new(
                    lash::process_controls::SessionProcessAdminPluginFactory::new(
                        lash_core::lifetime::starter,
                    ),
                ));
        match tools {
            Some(tools) => builder.tools(tools),
            None => builder,
        }
    })
    .await
}

/// A cell that creates a process, starts it and awaits it completes on one
/// slot: the cell parks while the started body runs, so the body takes the
/// slot the cell gave back and the cell resumes with its terminal.
async fn one_slot_cell_that_starts_and_awaits_a_process_completes(tier: Tier) {
    let workers = one_slot_workers(std::time::Duration::from_secs(10));
    let Some(world) = world(tier, workers.clone(), None).await else {
        return;
    };
    let output = world
        .run(
            "one-slot-await",
            served::spec(64),
            vec![served::cell(
                "const worker = await processes.create({ dialect: \"typescript\", \
                 source: 'const worker = async () => { return \"done\"; };' });\n\
                 const handle = await processes.start({ definition: worker });\n\
                 finish(await handle);",
            )],
        )
        .await;
    served::assert_answered("the cell that awaits its process on one slot", &output);
    assert_eq!(
        output.final_value(),
        Some(&serde_json::json!("done")),
        "the cell resumes with the awaited body's terminal"
    );
    assert_eq!(
        workers
            .pool()
            .expect("the shared pool")
            .config()
            .max_workers,
        1,
        "one slot ran the cell and the process body"
    );
    world.shutdown().await;
}

/// The name the law's create tool is called by: the production create tool,
/// under its own name so the law observes each of its attempts.
const CREATE: &str = "create_saturated";

/// The production `processes.create` tool, answering as `CREATE`: each
/// attempt is recorded, and the slot held at the first is released after it.
struct CreateAttemptProbe {
    inner: Arc<dyn lash_core::ToolProvider>,
    workers: lash::vm::WorkerService,
    /// Whether the first attempt has run: it holds the only slot while the
    /// production create checks one out.
    first_ran: Mutex<bool>,
    /// Each attempt's outcome: whether it ended, held a failure, or faulted
    /// as a host fault, and whether that fault is retryable.
    attempts: Mutex<Vec<Attempt>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Attempt {
    Done { success: bool },
    Pending,
    HostFailed { retryable: bool },
}

fn create_definition() -> lash_core::ToolDefinition {
    let production = lash_vm_runtime::process_create_tool_definition();
    lash_core::ToolDefinition::new(
        format!("tool:{CREATE}"),
        CREATE,
        production.manifest.description,
        production.contract.input_schema,
        production.contract.output_schema,
    )
    .with_execution(std::time::Duration::from_secs(120))
    .with_declaration(production.manifest.declaration)
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], CREATE))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CreateAttemptProbe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![create_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == CREATE).then(|| Arc::new(create_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let first = !std::mem::replace(&mut *self.first_ran.lock_recover(), true);
        // The cell is parked on this call, so its slot is free: the first
        // attempt holds it, saturating the pool the create checks out from.
        let held = first.then(|| held_worker(&self.workers));
        let outcome = self.inner.execute(call).await;
        if let Some(held) = held {
            held.release()
                .expect("release before the engine's next attempt");
        }
        self.attempts.lock_recover().push(match &outcome {
            lash_core::ToolAttemptOutcome::Done { result, .. } => Attempt::Done {
                success: result.clone().into_output().is_success(),
            },
            lash_core::ToolAttemptOutcome::Pending(_) => Attempt::Pending,
            lash_core::ToolAttemptOutcome::HostFailed(error) => Attempt::HostFailed {
                retryable: error.code.is_retryable() && error.is_attempt_fault(),
            },
        });
        outcome
    }
}

fn held_worker(workers: &lash::vm::WorkerService) -> lash_vm_client::Checkout {
    use lash_vm_client::WorkerPoolRuntimeOps as _;
    workers
        .pool()
        .expect("pool")
        .checkout(
            4096,
            lash_vm_protocol::OwnerEpoch(0),
            lash_vm_protocol::FrameEpoch(0),
            lash_vm_client::ExecutionBudget::default(),
        )
        .expect("hold the sole worker")
}

/// A create whose checkout finds the only slot held is a retryable host
/// fault: the engine attempts the call again once the slot is free, the
/// retry creates the definition, and the turn is shown no refusal.
async fn saturated_process_create_retries_without_recording_a_tool_refusal(tier: Tier) {
    let workers = one_slot_workers(std::time::Duration::from_millis(100));
    let probe = Arc::new(CreateAttemptProbe {
        inner: Arc::new(lash_vm_runtime::process_create_tool_provider(
            "typescript",
            lash_vm_runtime::LashVmSurface::default(),
            workers.clone(),
        )),
        workers: workers.clone(),
        first_ran: Mutex::new(false),
        attempts: Mutex::new(Vec::new()),
    });
    let Some(world) = world(
        tier,
        workers,
        Some(Arc::clone(&probe) as Arc<dyn lash_core::ToolProvider>),
    )
    .await
    else {
        return;
    };
    let output = world
        .run(
            "create-retry",
            served::spec(64),
            vec![served::cell(&format!(
                "const created = await tools.{CREATE}({{ dialect: \"typescript\", \
                 source: 'const answer = async (): Promise<number> => {{ return 42; }};' }});\n\
                 finish(created);"
            ))],
        )
        .await;
    served::assert_answered("the turn whose create met a saturated slot", &output);
    assert_eq!(
        *probe.attempts.lock_recover(),
        vec![
            Attempt::HostFailed { retryable: true },
            Attempt::Done { success: true },
        ],
        "the saturated attempt is a retryable host fault, attempted again before any result; \
         the model was shown {:?}",
        world.requests("create-retry")
    );
    assert!(
        output
            .final_value()
            .is_some_and(|created| created.get("id").is_some()),
        "the cell holds the created definition: {:?}",
        output.final_value()
    );
    let fault = lash_vm_client::PoolError::CheckoutTimedOut.to_string();
    let shown = world.requests("create-retry").join("\n");
    assert!(
        !shown.contains(&fault) && !format!("{:?}", output.result.state).contains(&fault),
        "no refusal is recorded or shown: {shown}"
    );
    world.shutdown().await;
}

tiered_laws!(
    one_slot_cell_that_starts_and_awaits_a_process_completes,
    saturated_process_create_retries_without_recording_a_tool_refusal,
);
