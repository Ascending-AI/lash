//! Pinned work runs from its stored definitions (FIG-6985, K-MACH-008):
//! a node resumes, starts and replays work without the package
//! registrations that built it.
//!
//! Two builds share one SQLite store. Build A holds release 1 of the host
//! package `slow`, whose helper `slow.wait` sleeps twice and answers its
//! argument and 1; build B holds release 2 of the same line, whose helper
//! answers its argument and 2, and none of release 1's registrations.
//! Both builds install their packages and serve: B claims A's work
//! because B's capabilities meet the work's hard requirements, whatever
//! set A linked it under (FIG-6993).
//!
//! - A process A started performs `slow.wait`'s first sleep, whose result
//!   A commits and delivers, and parks on its second; A stops; B resumes
//!   it inside the helper from release 1's stored definitions, and it
//!   answers as release 1 does.
//! - A process A published and started, which no node of A ran, runs on B
//!   from the definitions its publication stored, as release 1.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;

use lash::package::{HeldPackages, Package, PackageSet, PackageSources};
use lash_durable::{ActorKey, ActorState, DurableStore};
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_execution_contracts::backend::StoreSet;
use lash_kernel_doc::FunctionId;

/// How many virtual-time steps a law takes before it gives up.
const STEPS: usize = 200;

/// Release `release` of `slow`: its helper sleeps twice, then answers its
/// argument and `step`.
fn slow(release: &str, step: i64) -> Package {
    Package::build(
        "slow",
        release,
        &PackageSources {
            kernel: &format!(
                "function slow.wait(n: Int) -> Any\nkernel 1\ncharge 3\nbody {{\n  do sleep 1000\n  do sleep 1000\n  return [n, {step}]\n}}\n"
            ),
            surface: "async function wait(n: Int) -> Any = slow.wait\n",
            labels: "wait\twait\n",
        },
        &[],
    )
    .expect("slow builds")
}

/// lash's own packages with `slow`, as the node's own set.
fn held(slow: Package) -> Arc<HeldPackages> {
    let shipped = lash_vm_runtime::lash_packages().expect("shipped packages");
    let slow = Arc::new(slow);
    let set = PackageSet::new(
        shipped
            .artifacts()
            .values()
            .map(AsRef::as_ref)
            .chain([slow.as_ref()]),
    )
    .expect("a closed set");
    Arc::new(
        HeldPackages::new(shipped.artifacts().values().cloned().chain([slow]), set)
            .expect("the node's packages"),
    )
}

/// A process document whose entry calls `wait`, release 1's helper.
fn document(wait: FunctionId) -> lash_kernel_doc::Document {
    lash::workflow::document::parse_document(&format!(
        "kernel 1\nnumbers float\nuse slow.wait = @{wait}\nentry worker() -> Any\nfn worker() {{\n  let v = invoke slow.wait(5)\n  return v\n}}\nmain {{\n  finish null\n}}\n"
    ))
    .expect("the document parses")
}

/// One build's core over the shared store set.
fn core(
    stores: &Arc<dyn StoreSet>,
    clock: &Arc<SimClock>,
    packages: &Arc<HeldPackages>,
    name: &str,
) -> lash::LashCore {
    let backend = served::configured_backend(Arc::clone(stores), sim::settings(), Vec::new());
    lash::LashCore::rlm_builder(
        backend.clone(),
        served::rlm(&backend, sim::workers(clock)).with_packages(Arc::clone(packages)),
    )
    .serve_sessions(false)
    .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
    .data_retention(lash::DataRetention::standard())
    .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
    .execution_budgets(lash::ExecutionBudgets::recommended())
    .delta_coalescing(lash::DeltaCoalescing::recommended())
    .build(lash::persistence::LeaseOwnerIdentity::opaque(
        lash::persistence::LeaseOwnerId::new(format!("pinned-code-{name}")),
        lash::persistence::LeaseIncarnationId::new(format!("pinned-code-{name}-boot")),
    ))
    .expect("the core builds")
}

/// Publishes `document` under a host pin, with its declarations as
/// `packages` holds them, and starts its entry `worker` on `core`.
async fn start(
    core: &lash::LashCore,
    packages: &HeldPackages,
    document: &lash_kernel_doc::Document,
) -> lash_core::ProcessId {
    let identity = lash_vm_runtime::KernelDocuments::new(core.backend().module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &lash_kernel_doc::IdentifiedDocument::new(document.clone()),
            &lash::workflow::document::CheckedDeclarations::required(
                document,
                packages.functions().as_ref(),
            )
            .expect("the writer holds the document's helpers"),
        )
        .await
        .expect("the document publishes");
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        document: identity,
        functions: document.manifest.functions.keys().copied().collect(),
        entry: lash::workflow::document::Name::new("worker"),
        packages: packages.digest(),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes");
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(4))
        .build()
        .expect("the env builds");
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &env)
        .await
        .expect("publish the env");
    let kind = lash_vm_runtime::LASH_VM_ENGINE_KIND;
    let settings = core
        .processes()
        .engine_defaults(kind, &payload)
        .expect("the kernel's settings");
    core.processes()
        .start(
            lash_execution_contracts::process::ProcessStartRequest::engine(
                kind.to_owned(),
                payload,
                lash_execution_contracts::process::EngineLaunch::new(env_ref, settings),
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::LifetimeDecision::Detached,
            ),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id
}

fn actor(process: &lash_core::ProcessId) -> ActorKey {
    ActorKey::process(process.as_str()).expect("a process actor key")
}

/// `process`'s terminal output, encoded, once it has one.
async fn terminal(core: &lash::LashCore, process: &lash_core::ProcessId) -> Option<String> {
    core.backend()
        .process_registry()
        .get_process(process)
        .await
        .ok()
        .flatten()
        .and_then(|record| record.terminal().cloned())
        .map(|terminal| {
            serde_json::to_string(&lash_protocol_types::Durable(&terminal.into_await_output()))
                .expect("a terminal encodes")
        })
}

/// Steps virtual time until `done` holds.
async fn until<F, Fut>(nodes: &SimNodes, clock: &SimClock, what: &str, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..STEPS {
        nodes.quiesce().await;
        if done().await {
            return;
        }
        if nodes.step().await.is_none() {
            clock.advance_by(100).await;
        }
    }
    panic!("{what} never happened\n{}", nodes.script().rendered_trace());
}

/// A build that holds none of release 1's registrations resumes a run
/// parked inside release 1's helper, and starts a process published and
/// started under release 1, each from the definitions the store keeps:
/// both answer as release 1 does, not as the build's own release.
#[tokio::test]
async fn a_build_without_the_writers_registrations_resumes_and_starts_its_pinned_work() {
    let clock = SimClock::new();
    let stores = sim::memory(Arc::clone(&clock)).await;
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let stores: Arc<dyn StoreSet> = Arc::new(stores);
    let (r1, r2) = (slow("1.0.0", 1), slow("2.0.0", 2));
    let wait = r1.function("slow.wait").expect("release 1's helper");
    let (a, b) = (held(r1), held(r2));
    assert!(
        b.functions().get(&wait).is_none(),
        "build B holds none of release 1's helper"
    );
    let (core_a, core_b) = (
        core(&stores, &clock, &a, "a"),
        core(&stores, &clock, &b, "b"),
    );
    let activation = |core: &lash::LashCore| {
        lash::testing::node_activation(core, Arc::new(Tripwire::default()))
            .expect("the core's node activation")
    };
    let (backend_a, activation_a) = activation(&core_a);
    let (backend_b, activation_b) = activation(&core_b);
    let nodes = SimNodes::new(
        database,
        Arc::clone(&clock),
        Script::new(),
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend_a.formats().decodes(),
            serves: lash::testing::node_serves(&core_a),
            max_active: 4,
        },
        activation_a,
    );
    nodes.decode_on("b", backend_b.formats().decodes());
    nodes.activate_on("b", activation_b);
    nodes.serve_on("b", lash::testing::node_serves(&core_b));
    let document = document(wait);

    // A runs the process into release 1's helper, past its first sleep,
    // whose result A commits and delivers, to its second.
    let parked = start(&core_a, &a, &document).await;
    nodes.start("a");
    let due = || async {
        match nodes.database().actor(&actor(&parked)).await {
            Ok(Some(snapshot)) if snapshot.state == ActorState::Waiting => snapshot.next_due,
            _ => None,
        }
    };
    until(
        &nodes,
        &clock,
        "the run sleeps inside the helper",
        || async { due().await.is_some() },
    )
    .await;
    let first = due().await;
    until(
        &nodes,
        &clock,
        "the run parks on the helper's second sleep",
        || async { due().await.is_some_and(|next| Some(next) != first) },
    )
    .await;
    assert_eq!(terminal(&core_a, &parked).await, None);
    nodes.kill("a");

    // A process published and started under release 1 that no node of A
    // runs.
    let unstarted = start(&core_a, &a, &document).await;

    nodes.start("b");
    for process in [&parked, &unstarted] {
        until(&nodes, &clock, "the process ends on B", || async {
            terminal(&core_b, process).await.is_some()
        })
        .await;
        let ended = terminal(&core_b, process).await.expect("a terminal");
        assert!(
            ended.contains("[5,1]"),
            "the process answers as release 1's helper does, not B's own: {ended}"
        );
    }
}
