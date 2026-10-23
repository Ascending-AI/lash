//! Pinned work runs from its stored definitions (FIG-6985, K-MACH-008):
//! a node resumes, starts and replays work without the package
//! registrations that built it.
//!
//! Two builds share one SQLite store. Build A publishes the host package
//! `slow`, whose helper sleeps twice and answers its argument and 1. Build B
//! publishes different code under that same path, answering its argument and
//! 2. B claims A's work because B meets its recorded capability requirements
//! (FIG-6993); execution keeps the Link A retained.
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

use lash::package::*;
use lash_core::plugin::packages::PublishPackage;
#[path = "support/kernel_namespace.rs"]
mod kernel_namespace;
use lash_durable::DurableStore;
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_execution_contracts::backend::StoreSet;

/// How many virtual-time steps a law takes before it gives up.
const STEPS: usize = 200;

/// Each build authors its own code; the stored Link, not its package path,
/// selects the helper used by an already-published process.
fn slow(step: i64) -> lash_kernel_doc::Document {
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function slow.wait(n: Int) -> Json\nkernel 1\nmetering statements\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nbody {{ do perform 0(1000) as Null do perform 0(1000) as Null return [n, {step}] }}"
    )).unwrap();
    let mut document = lash_kernel_doc::parse_document(
        "kernel 1\nnumbers float\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nmain { finish null }"
    ).unwrap();
    document.main = None;
    document.exports.insert(
        Name::new("wait"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    document.functions.insert(definition.identity(), definition);
    document
}

const SERVICES: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("pinned-code-services", lash::plugins::FormatVersion::ONE);

fn services() -> lash::process::StaticProcessComponent {
    let provider = lash_vm_runtime::dialect_services::Services::new(
        lash_vm_runtime::dialect_services::Dialect::TypeScript,
        SERVICES.tool_provider(lash::tool_provider_key!("timer")),
        std::time::Duration::from_secs(30),
        Arc::new(|| 0),
    )
    .unwrap();
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(SERVICES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider_registration(
                lash::tool_provider_key!("timer"),
                provider.registration(),
            )
            .with_package_provider(lash::tool_provider_key!("timer"), provider),
    )
    .as_component(SERVICES.format())
    .unwrap()
}

/// One build's core over the shared store set.
fn core(stores: &Arc<dyn StoreSet>, clock: &Arc<SimClock>, name: &str) -> lash::LashCore {
    let backend = served::configured_backend(Arc::clone(stores), sim::settings(), Vec::new());
    let workers = sim::workers(clock);
    let bounds = workers.config().run_bounds;
    lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            "typescript",
            bounds,
        )))
        .process_component(Arc::new(services()), SERVICES)
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

/// Publish an explicit code Package, link the worker against that exact
/// advertisement and retain its resulting execution environment.
async fn publication(
    core: &lash::LashCore,
    helper: &lash_kernel_doc::Document,
) -> (serde_json::Value, lash::process::ProcessEnv) {
    let artifacts = core.host_artifacts();
    let pin = lash_core::HostArtifactPin::mint();
    let env = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(4))
        .component(SERVICES, &())
        .build()
        .unwrap();
    let (mut packages, mut environment) = kernel_namespace::namespace(core, &pin, &env).await;
    let helper_start = artifacts
        .publish_workflow(
            &pin,
            &helper.clone().into(),
            &StartSelector::Export(Name::new("wait")),
            &environment,
            None,
            None,
        )
        .await
        .unwrap();
    let package = Package {
        path: PackagePath::parse("slow").unwrap(),
        members: [(
            Segment::new("wait").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: helper_start.link,
                    export: ExportName {
                        name: Name::new("wait"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let context = PublicationContext {
        root: Segment::new("slow").unwrap(),
        ..environment.context.clone()
    };
    let publisher = artifacts
        .package_publisher(context.clone(), environment.snapshot.clone())
        .unwrap();
    let claim =
        lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(pin.clone()))
            .unwrap();
    let documents = lash_vm_runtime::KernelDocuments::new(
        core.backend().module_artifacts(),
        core.backend().package_store(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    );
    use lash_execution_contracts::process::linking::{ContentRef, Fulfilled, ReachSelector};
    let link = documents
        .load(&ContentRef::Link(helper_start.link))
        .await
        .unwrap()
        .material
        .downcast::<Link>()
        .unwrap();
    let inspection = documents
        .inspect_link(helper_start.link, &ReachSelector::AllExports)
        .await
        .unwrap();
    let publication = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: package.clone(),
                code: vec![Fulfilled {
                    content: link,
                    inspection,
                }],
            },
        )
        .await
        .unwrap();
    packages
        .snapshot
        .advertisements
        .push(AcceptedAdvertisement {
            publication,
            advertisement: PackageAdvertisement::Enumerated(package.clone()),
        });
    environment.snapshot = packages.snapshot.clone();
    environment.attachment.packages.insert(package.path);
    let document = lash_kernel_doc::parse_document(
        "kernel 1\nnumbers float\nimport 0 code slow.wait(n: Int) -> Json direct\nmain { let v = invoke import 0(5) return v }"
    ).unwrap();
    let start = artifacts
        .publish_workflow(
            &pin,
            &document.into(),
            &StartSelector::Main,
            &environment,
            None,
            None,
        )
        .await
        .unwrap();
    let env = artifacts
        .link_process_env(
            &env,
            &lash::process::LinkTarget::Linked(start.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        code: lash_vm_runtime::process::InlineRequest::Linked { start },
        args: Default::default(),
    })
    .unwrap();
    (payload, env)
}

async fn start(
    core: &lash::LashCore,
    helper: &lash_kernel_doc::Document,
) -> lash_vocabulary::identity::ProcessId {
    let (payload, env) = publication(core, helper).await;
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

/// `process`'s terminal output, encoded, once it has one.
async fn terminal(
    core: &lash::LashCore,
    process: &lash_vocabulary::identity::ProcessId,
) -> Option<String> {
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
    let (a, b) = (slow(1), slow(2));
    assert!(
        a.functions.keys().all(|id| !b.functions.contains_key(id)),
        "build B authors none of release 1's helper"
    );
    let (core_a, core_b) = (core(&stores, &clock, "a"), core(&stores, &clock, "b"));
    let _new_build_publication = publication(&core_b, &b).await;
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

    // A runs the process into release 1's helper, past its first sleep,
    // whose result A commits and delivers, to its second.
    let parked = start(&core_a, &a).await;
    nodes.start("a");
    let due = || async {
        backend_a
            .process_registry()
            .get_process(&parked)
            .await
            .unwrap()
            .and_then(|record| {
                record.waits().iter().find_map(|wait| match wait.kind {
                    lash_execution_contracts::process::WaitKind::Sleep { until_ms } => {
                        Some(until_ms)
                    }
                    lash_execution_contracts::process::WaitKind::Call { resume_at_ms, .. } => {
                        resume_at_ms
                    }
                    _ => None,
                })
            })
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
    let unstarted = start(&core_a, &a).await;

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
