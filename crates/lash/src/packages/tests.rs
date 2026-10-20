//! FLEET-SET and RETAIN-SURVEY (FIG-6918) over two lines of host packages:
//! `p`, whose body calls `q.v`, built over `q@1` and again over `q@2`; and
//! PROMOTE-BY-CAPABILITY (FIG-6991), the selector the gate switches to with
//! requirements-based claims, over `q`'s releases; and the stored surface a
//! node that does not hold a set reads it from.

use futures_util::StreamExt;
use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::durable_port::{FormatSet, HeldArtifact, NodeCapability, PackageSetDigest};
use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources};
use lash_vm_runtime::{RetainDependent, RetainSurvey};

use super::{
    FleetPackages, NodeHold, fleet_package_set, held_artifact, members, promoted, serving,
};

fn q(value: i64) -> Package {
    let kernel =
        format!("function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n");
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: "function v() -> Int = q.v\n",
            labels: "v\tvalue\n",
        },
        &[],
    )
    .expect("q builds")
}

fn p(q: &Package) -> Package {
    Package::build(
        "p",
        "1.0.0",
        &PackageSources {
            kernel: "use q.v\n\nfunction p.f() -> Int\nkernel 1\ncharge 2\nbody {\n  let value = invoke q.v()\n  return value\n}\n",
            surface: "function f() -> Int = p.f\n",
            labels: "f\tf\n",
        },
        &[q],
    )
    .expect("p builds")
}

/// `p@1` over `q@1`, and the same source of `p` rebuilt over `q@2`.
struct Lines {
    q1: Package,
    p1: Package,
    q2: Package,
    p2: Package,
}

fn lines() -> Lines {
    let q1 = q(1);
    let p1 = p(&q1);
    let q2 = q(2);
    let p2 = p(&q2);
    Lines { q1, p1, q2, p2 }
}

fn set(packages: &[&Package]) -> PackageSet {
    PackageSet::new(packages.iter().copied()).expect("a closed set")
}

/// A node holding both lines, the newer its own set.
fn both(lines: &Lines) -> Arc<HeldPackages> {
    let old = set(&[&lines.p1, &lines.q1]);
    let new = set(&[&lines.p2, &lines.q2]);
    let own = new.digest();
    Arc::new(
        HeldPackages::new(
            [&lines.q1, &lines.p1, &lines.q2, &lines.p2]
                .into_iter()
                .map(|package| Arc::new(package.clone())),
            [old, new],
            own,
        )
        .expect("both lines"),
    )
}

fn artifacts(packages: &[&Package]) -> BTreeSet<HeldArtifact> {
    packages
        .iter()
        .map(|package| held_artifact(&package.key()))
        .collect()
}

fn node(decodes: &[&FormatSet], held: BTreeSet<HeldArtifact>) -> NodeCapability {
    NodeCapability {
        decodes: decodes.iter().map(|set| (*set).clone()).collect(),
        held,
        capabilities: Default::default(),
    }
}

fn stored(sets: &[(PackageSetDigest, BTreeSet<HeldArtifact>, u64)]) -> super::PackageCatalog {
    super::PackageCatalog {
        artifacts: Vec::new(),
        sets: sets
            .iter()
            .map(
                |(digest, members, sequence)| lash_core::durable_port::StoredSet {
                    digest: *digest,
                    members: members.clone(),
                    sequence: *sequence,
                    requirements: Default::default(),
                },
            )
            .collect(),
    }
}

/// FLEET-SET: rebuilding `p` over `q@2` makes a new artifact, and a cell
/// is written under `{p', q@2}` or `{p@1, q@1}`, never a mix: own set
/// first, then the other sets the node holds whole, newest first by the
/// store's install sequence. While a live serving node lacks the own set,
/// the gate falls back to the older set; a node serving only carried
/// sessions counts; once none lacks it, the own set is written.
#[test]
fn a_cell_is_written_under_one_whole_set_every_serving_node_holds() {
    let lines = lines();
    assert_ne!(
        lines.p1.digest(),
        lines.p2.digest(),
        "p rebuilt over q@2 is a new artifact"
    );
    let packages = both(&lines);
    let old = set(&[&lines.p1, &lines.q1]).digest();
    let new = packages.own();
    let hold = NodeHold::new(Arc::clone(&packages));
    *hold.advertised.write().unwrap() =
        Some(artifacts(&[&lines.q1, &lines.p1, &lines.q2, &lines.p2]));
    // A third, older retained set the store installed first: tried last.
    let catalog = stored(&[
        (old, artifacts(&[&lines.p1, &lines.q1]), 2),
        (new, artifacts(&[&lines.p2, &lines.q2]), 3),
    ]);
    let candidates = hold.candidates(&catalog);
    assert_eq!(
        candidates
            .iter()
            .map(|(digest, _)| *digest)
            .collect::<Vec<_>>(),
        vec![new, old],
        "own first, then newest first"
    );
    for (_, members) in &candidates {
        let lines: BTreeSet<&str> = members.iter().map(|member| member.path.as_str()).collect();
        assert_eq!(
            lines.len(),
            members.len(),
            "a candidate is one artifact a line"
        );
    }

    let session = FormatSet::new("session:law");
    let carried = FormatSet::new("session:law-previous");
    let this = node(&[&session], hold.held());
    let older = node(&[&session], artifacts(&[&lines.p1, &lines.q1]));
    let live = vec![this.clone(), older];
    assert_eq!(
        fleet_package_set(&candidates, &serving(&live, &session, &[])),
        Some(old),
        "a live node lacking the own set holds the cell to the older set"
    );
    let carrying = node(&[&carried], artifacts(&[&lines.p1, &lines.q1]));
    let live = vec![this.clone(), carrying.clone()];
    assert_eq!(
        fleet_package_set(
            &candidates,
            &serving(&live, &session, std::slice::from_ref(&carried))
        ),
        Some(old),
        "a node serving only carried sessions co-serves the cell"
    );
    assert_eq!(
        fleet_package_set(&candidates, &serving(&live, &session, &[])),
        Some(new),
        "a node serving no session of this build does not hold the gate back"
    );
    let mixed = node(&[&session], artifacts(&[&lines.p1, &lines.q2]));
    assert_eq!(
        fleet_package_set(&candidates, &serving(&[this.clone(), mixed], &session, &[])),
        None,
        "a node holding a mix holds no candidate whole: nothing is written"
    );
    assert_eq!(
        fleet_package_set(&candidates, &serving(&[this], &session, &[])),
        Some(new)
    );
}

/// RETAIN-SURVEY: an artifact outside the own set is retained while a
/// parked root, an unfinished process (started or not), a saved function
/// or a published definition lists one of its identities, reached through
/// the bodies of what it lists; with none, it is dropped, and a set naming
/// it stops being a candidate with no set row edited. An identity no held
/// artifact declares strands its dependent.
#[test]
fn an_artifact_is_retained_exactly_while_a_dependent_lists_it() {
    let lines = lines();
    let packages = both(&lines);
    let old = set(&[&lines.p1, &lines.q1]);
    let old_f = lines.p1.function("p.f").expect("p@1's f");
    let session = lash_core::SessionId::parse("retain-law").expect("a session id");
    let dependents = [
        RetainDependent::ParkedCell {
            session: session.clone(),
            cell: "exec-code:0".into(),
        },
        RetainDependent::Process {
            process: lash_core::ProcessId::fixture("unstarted"),
            started: false,
        },
        RetainDependent::SavedFunction {
            session: session.clone(),
            name: "advance".into(),
        },
        RetainDependent::PublishedDefinition {
            document: lash_kernel_doc::DocumentId::from_bytes([3; 32]),
        },
    ];
    for dependent in dependents {
        let survey = RetainSurvey::of(&packages, [(dependent.clone(), BTreeSet::from([old_f]))]);
        assert_eq!(
            survey.retained.keys().cloned().collect::<BTreeSet<_>>(),
            old.keys().collect(),
            "{dependent:?} retains p@1 and, through its body, q@1"
        );
        assert!(
            survey
                .retained
                .values()
                .all(|by| by == &vec![dependent.clone()])
        );
        assert!(survey.stranded.is_empty());
        assert_eq!(
            survey.held(&packages),
            packages.artifacts().keys().cloned().collect()
        );
    }

    let dropped = RetainSurvey::of(&packages, []);
    assert!(dropped.retained.is_empty());
    let hold = NodeHold::new(Arc::clone(&packages));
    *hold.advertised.write().unwrap() =
        Some(dropped.held(&packages).iter().map(held_artifact).collect());
    let catalog = stored(&[
        (old.digest(), members(&old), 1),
        (packages.own(), artifacts(&[&lines.p2, &lines.q2]), 2),
    ]);
    assert_eq!(
        hold.candidates(&catalog)
            .iter()
            .map(|(digest, _)| *digest)
            .collect::<Vec<_>>(),
        vec![packages.own()],
        "a set naming a dropped artifact is no candidate"
    );

    let unknown = lash_kernel_doc::FunctionId::from_bytes([5; 32]);
    let stranded = RetainSurvey::of(
        &packages,
        [(
            RetainDependent::Process {
                process: lash_core::ProcessId::fixture("started"),
                started: true,
            },
            BTreeSet::from([unknown]),
        )],
    );
    assert_eq!(stranded.stranded.len(), 1);
    assert_eq!(stranded.stranded[0].function, unknown);
}

/// OP-INTERNAL / P3: an import and a hand-authored document call an internal
/// operation only through its package body. The gate's older set pins the
/// published definition and its runtime view without a separate actor stamp.
#[tokio::test]
async fn an_imported_definition_keeps_its_gate_set_and_runs_its_internal_body() {
    use lash_core::durable_port::{NodeId, NodeSpec};
    use lash_execution_contracts::PackageGate as _;
    let package = |release: &str| {
        Package::build("dataframe", release, &PackageSources {
            kernel: &format!("effect dataframe.run(input?: Json) -> Text contract \"run/1\"\n\nfunction dataframe.execute() -> Text\nkernel 1\ncharge {}\nbody {{\n  let got = perform dataframe.run({{}}) as Text\n  return got\n}}\n", if release == "old" { 2 } else { 3 }),
            surface: "internal operation run(input?: Json) -> Text contract \"run/1\"\nasync function execute() -> Text = dataframe.execute\n",
            labels: "execute\texecute\n",
        }, &[]).unwrap()
    };
    let old = Arc::new(package("old"));
    let new = Arc::new(package("new"));
    let shipped = lash_vm_releases::lash_packages().unwrap();
    let set_of = |package: &Package| {
        PackageSet::new(
            shipped
                .artifacts()
                .values()
                .map(AsRef::as_ref)
                .chain([package]),
        )
        .unwrap()
    };
    let legacy = Arc::new(
        Package::build(
            "legacy",
            "old",
            &PackageSources {
                surface: "operation lookup(input?: Json) -> Any contract \"legacy/1\"\n",
                ..Default::default()
            },
            &[],
        )
        .unwrap(),
    );
    let old_set = PackageSet::new(
        shipped
            .artifacts()
            .values()
            .map(AsRef::as_ref)
            .chain([old.as_ref(), legacy.as_ref()]),
    )
    .unwrap();
    let new_set = set_of(&new);
    let pin = old_set.digest();
    let own = new_set.digest();
    let held = Arc::new(
        HeldPackages::new(
            shipped
                .artifacts()
                .values()
                .cloned()
                .chain([old.clone(), new.clone(), legacy.clone()]),
            [old_set, new_set],
            own,
        )
        .unwrap(),
    );
    let clock = Arc::new(lash_core::testing::TestClock::new(1_700_000_000_000));
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock.clone())
            .await
            .unwrap(),
    );
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let backend = lash_conformance::backend_over(stores.clone());
    let durable = backend.durable();
    let (artifacts, sets) = super::install_rows(&held);
    durable.install_packages(&artifacts, &sets).await.unwrap();
    let old_held = HeldPackages::install(
        shipped
            .in_order()
            .into_iter()
            .map(|p| (**p).clone())
            .chain([(*old).clone(), (*legacy).clone()]),
    )
    .unwrap();
    let older = durable
        .register_node(&NodeSpec {
            node: NodeId::new("old-import-serving-node"),
            decodes: vec![backend.formats().session().clone()],
            serves: vec![],
            held: super::own_artifacts(&old_held),
            capabilities: Default::default(),
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    let definition = lash_core::ToolDefinition::raw(
        "tool:dataframe",
        "dataframe.run",
        "Runs the private body.",
        serde_json::json!({}),
        serde_json::json!({"type":"string"}),
    )
    .unwrap()
    .declared_contract(lash_core::ContractId::new("run/1"))
    .with_execution(std::time::Duration::from_secs(30));
    struct Runner;
    #[async_trait::async_trait]
    impl crate::tools::StaticToolExecute for Runner {
        async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            lash_core::ToolOutcome::ok(serde_json::json!("served-internal")).into()
        }
    }
    const TOOLS: lash_core::ProcessConfigKey<()> =
        lash_core::ProcessConfigKey::new("internal-runner", lash_core::FormatVersion::ONE);
    let runner = Arc::new(crate::tools::StaticToolProvider::new(
        vec![definition.clone()],
        Runner,
    ));
    struct NamespaceResolver(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl lash_core::ToolProvider for NamespaceResolver {
        fn tool_manifests(
            &self,
            _: &lash_core::OwnerConfigView,
        ) -> Result<Vec<lash_core::ToolManifest>, lash_execution_contracts::PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(
            &self,
            _: &str,
            _: &lash_core::OwnerConfigView,
        ) -> Option<Arc<lash_core::ToolContract>> {
            None
        }
        async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            panic!("a package namespace never dispatches")
        }
    }
    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for NamespaceResolver {
        async fn resolve(
            &self,
            _: &lash_core::DeferredResolveContext<'_>,
            _: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Default::default()
        }
    }
    let resolver = Arc::new(NamespaceResolver(std::sync::atomic::AtomicUsize::new(0)));
    let granted = lash_core::ToolDefinition::raw(
        "host:namespace-call",
        "tools.call_tool",
        "Dispatches a path",
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(30));
    let dispatch = lash_core::DeferredToolProviderRegistration::new(resolver.clone())
        .with_dispatch_grant(lash_core::DispatchGrant::new(granted.clone(), |input| {
            Ok(lash_core::DispatchTarget {
                path: input["path"].as_str().unwrap().to_owned(),
                args: serde_json::json!({}),
            })
        }))
        .unwrap();
    let make_core = |held: Arc<HeldPackages>| {
        let workers = lash_vm_client::service::Service::default();
        let bounds = workers.config().run_bounds;
        crate::LashCore::standard_builder(backend.clone())
            .plugin(Arc::new(
                lash_vm_runtime::KernelProcessPluginFactory::new(workers, "typescript", bounds)
                    .with_packages(held),
            ))
            .process_component(
                Arc::new(
                    lash_core::plugin::StaticPluginFactory::new(
                        lash_core::plugin::PluginDeclaration::initial(TOOLS.component()),
                        lash_core::plugin::PluginSpec::new()
                            .with_tool_provider(crate::tool_provider_key!("tools"), runner.clone())
                            .with_deferred_provider(
                                crate::tool_provider_key!("deferred"),
                                dispatch.clone(),
                            ),
                    )
                    .as_component(TOOLS.format())
                    .unwrap(),
                ),
                TOOLS,
            )
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(crate::DataRetention::standard())
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(lash_core::testing::runtime_lease_owner())
            .unwrap()
    };
    let start_held = Arc::new(
        HeldPackages::new(
            held.artifacts().values().cloned(),
            held.sets().values().cloned(),
            pin,
        )
        .unwrap(),
    );
    let core = make_core(start_held);
    // Include this core's serving node in the fleet query: build starts it
    // asynchronously, so querying sooner can hide a missing package hold.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while core.node.registration.snapshot().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the importing core's node registered");
    let hold = Arc::new(NodeHold::new(held.clone()));
    *hold.advertised.write().unwrap() = Some(held.artifacts().keys().map(held_artifact).collect());
    let chosen = super::FleetPackages::new(backend.clone(), hold)
        .writable()
        .await
        .unwrap();
    assert_eq!(chosen.digest(), pin);
    let env = lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(4))
        .component(TOOLS, &())
        .link_tool(
            TOOLS.tool_provider(crate::tool_provider_key!("tools")),
            &definition,
        )
        .build()
        .unwrap();
    let artifacts = core.host_artifacts();
    let imported = artifacts
        .import_workflow(
            lash_kernel_doc::SourceLanguage::TypeScript,
            "export async function flow() { return await dataframe.execute(); }",
            &env,
            chosen.clone(),
            None,
        )
        .await
        .unwrap();
    let crate::workflow::WorkflowImport::Imported {
        document, packages, ..
    } = imported
    else {
        panic!("{imported:?}");
    };
    assert_eq!(packages, chosen);
    let imported_document = document.to_document().unwrap();
    let old_id = old.code().ids()[old.code().position("dataframe.execute").unwrap()];
    assert!(imported_document.manifest.functions.contains_key(&old_id));
    // The runtime witness is deliberately a handwritten document, independent of lowering.
    let text = format!(
        "kernel 1\nnumbers float\neffect dataframe.run(input?: Json) -> Text contract \"run/1\"\nuse dataframe.execute = @{old_id}\nentry flow() -> Text\nfn flow() {{ do sleep 5 let got = invoke dataframe.execute() return got }}\nmain {{ finish null }}\n"
    );
    let handwritten =
        lash_kernel_doc::DocumentBytes::from(lash_kernel_doc::parse_document(&text).unwrap());
    let host_pin = crate::process::HostArtifactPin::mint();
    let env_ref = artifacts
        .publish_process_env(&host_pin, &env)
        .await
        .unwrap();
    let mut started = Vec::new();
    for (key, document) in [("imported", document), ("handwritten", handwritten)] {
        let publication = artifacts
            .publish_workflow(
                &host_pin,
                &document,
                &lash_kernel_doc::Name::new("flow"),
                &env,
                packages.clone(),
                None,
            )
            .await
            .unwrap();
        let crate::workflow::WorkflowPublish::Published(publication) = publication else {
            panic!("{publication:?}");
        };
        let resolved = artifacts
            .resolved_definition(&publication.definition.id)
            .await
            .unwrap()
            .unwrap();
        let payload = resolved.start_payload(&Default::default()).unwrap();
        let request = lash_execution_contracts::process::ProcessStartRequest::definition(
            publication.definition.id.clone(),
            Some(publication.definition.signature.clone()),
            Default::default(),
            lash_execution_contracts::process::EngineLaunch::new(
                env_ref.clone(),
                core.processes()
                    .engine_defaults(
                        lash_vm_runtime::LASH_VM_ENGINE_KIND,
                        &serde_json::Value::Null,
                    )
                    .unwrap(),
            ),
            lash_execution_contracts::process::ProcessOriginator::host(),
            lash_execution_contracts::process::LifetimeDecision::Detached,
        )
        .with_host_start_key(key);
        let receipt = core
            .processes()
            .start(request, core.effect_host())
            .await
            .unwrap();
        let record = backend
            .process_registry()
            .get_process(&receipt.process_id)
            .await
            .unwrap()
            .unwrap();
        let linked_env = lash_execution_contracts::process::load_process_env(
            backend.process_env_store().as_ref(),
            record.env_ref().unwrap(),
        )
        .await
        .unwrap();
        assert!(linked_env.package_roots().contains("dataframe"));
        assert!(linked_env.package_roots().contains("kernel"));
        assert!(
            linked_env.package_roots().contains("legacy"),
            "the linked old snapshot, not the node's own newer set, supplies roots"
        );
        started.push(receipt.process_id);
        let absent = Arc::new(
            HeldPackages::install(
                shipped
                    .in_order()
                    .into_iter()
                    .map(|p| (**p).clone())
                    .chain([(*new).clone()]),
            )
            .unwrap(),
        );
        // Independent backend: the missing-set producer has no inherited registration.
        let workers = lash_vm_client::service::Service::default();
        let factory = lash_vm_runtime::KernelProcessPluginFactory::new(
            workers.clone(),
            "typescript",
            workers.config().run_bounds,
        )
        .with_packages(absent);
        let trace = lash_core::trace::TraceRuntime::default();
        let host = lash_core::facade_support::PluginHost::new(
            vec![],
            lash_core::ExecutionBudgets::recommended(),
            trace.clone(),
        );
        let context =
            lash_core::plugin::ProcessEngineContributionContext::new(&host, &backend, &trace, true)
                .unwrap();
        let registry = lash_core::ProcessEngineRegistry::new()
            .with_registration(factory.bind_process_engine(&context).unwrap());
        assert!(
            matches!(registry.document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND).unwrap().definition_packages(&payload),Err(lash_execution_contracts::PluginError::PackagesNotHeld { packages }) if packages==pin)
        );
    }
    let parked = started.last().unwrap();
    let due = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        let observable = core.processes().observe(parked);
        let snapshot = observable.snapshot().await.unwrap();
        let mut feed = observable.subscribe_and_recover(snapshot.cursor);
        loop {
            let observed = core.processes().get(parked).await.unwrap().unwrap();
            if let Some(until) =
                observed
                    .lifecycle
                    .waits()
                    .iter()
                    .find_map(|wait| match wait.kind {
                        lash_execution_contracts::process::WaitKind::Sleep { until_ms } => {
                            Some(until_ms)
                        }
                        _ => None,
                    })
            {
                break until;
            }
            assert!(observed.lifecycle.terminal().is_none(), "{observed:?}");
            feed.next().await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
    // Only the handwritten flow parks at sleep. Let the imported flow's
    // Once effect settle before shutdown, so the restart cannot interrupt it.
    for process in &started {
        if process != parked {
            tokio::time::timeout(
                std::time::Duration::from_secs(120),
                core.processes().await_output(process),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
    core.shutdown().await.unwrap();
    drop(core);
    let core = make_core(held.clone());
    let current = held.view(&held.own()).unwrap();
    assert!(!lash_kernel_dialect::ExportView::namespace_roots(&current).contains("legacy"));
    let record = backend
        .process_registry()
        .get_process(parked)
        .await
        .unwrap()
        .unwrap();
    let env = Arc::new(
        lash_execution_contracts::process::load_process_env(
            backend.process_env_store().as_ref(),
            record.env_ref().unwrap(),
        )
        .await
        .unwrap(),
    );
    // Use the namespace facts read from the parked process, with a grant
    // this resumed node serves. Its current package view omits legacy.
    let effect = lash_core::EffectName::new("tools.call_tool").unwrap();
    let env = Arc::new(
        lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(1))
            .component(TOOLS, &())
            .link(
                effect.clone(),
                dispatch
                    .dispatch_link(
                        TOOLS.tool_provider(crate::tool_provider_key!("deferred")),
                        granted.id(),
                    )
                    .unwrap(),
            )
            .build()
            .unwrap()
            .with_package_roots(env.package_roots().clone()),
    );
    let composition = core
        .host_process_engines
        .components()
        .compose(&env, Some(parked))
        .unwrap();
    let owner = lash_core::ExecutionOwner::Process {
        process_id: parked.clone(),
    };
    assert!(
        matches!(composition.resolve_dispatch(&env, &owner, &effect, &serde_json::json!({"path":"legacy.lookup"})).await.unwrap(), Err(lash_core::DispatchRefusal::PackageNamespace { path }) if path == "legacy.lookup")
    );
    assert_eq!(resolver.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    clock.set(u64::try_from(due).unwrap());
    durable.release_node(&older).await.unwrap();
    for process in started {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(120),
            core.processes().await_output(&process),
        )
        .await
        .unwrap()
        .unwrap();
        let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output
        else {
            panic!("{output:?}");
        };
        assert!(output.is_success(), "{output:?}");
        assert_eq!(
            output.value_for_projection(),
            serde_json::json!("served-internal")
        );
    }
    core.shutdown().await.unwrap();
}

/// One node of each of `releases`' builds: its own set holding that release
/// of `q`, as the store keeps it at `sequence`, and the node as the gate
/// reads it live.
fn build(release: Package, sequence: u64) -> (lash_core::durable_port::StoredSet, NodeCapability) {
    let held = HeldPackages::install([release]).expect("a node's packages");
    let surface = held.surface(&held.own()).expect("its own surface");
    let stored = lash_core::durable_port::StoredSet {
        digest: held.own(),
        members: members(&held.sets()[&held.own()]),
        sequence,
        requirements: surface.requirements().clone(),
    };
    let live = NodeCapability {
        decodes: Vec::new(),
        held: BTreeSet::new(),
        capabilities: lash_kernel_package::node_capabilities(&held),
    };
    (stored, live)
}

/// `q@value` with a native-only `q.fast`: what a release adding a native
/// requires of every node.
fn q_with_native(value: i64) -> Package {
    let kernel = format!(
        "function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n\nfunction q.fast() -> Int\nkernel 1\ncharge 1\nnative\n"
    );
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: "function v() -> Int = q.v\nfunction fast() -> Int = q.fast\n",
            labels: "v\tvalue\nfast\tfast\n",
        },
        &[],
    )
    .expect("q builds")
}

/// PROMOTE-BY-CAPABILITY: the candidates are the own surfaces of the
/// writer and of the live serving nodes, and the newest by install
/// sequence whose requirements every one of them meets is chosen. A
/// release changing only a body is chosen as soon as one node of it is
/// live, on every node; once its nodes stop it is no candidate, and the
/// older surface is chosen again. A release adding a native-only function
/// waits until no live node lacks the native.
#[test]
fn the_newest_live_surface_every_serving_node_can_run_is_promoted() {
    let (a, a_node) = build(q(1), 1);
    let (b, b_node) = build(q(2), 2);
    let (c, c_node) = build(q_with_native(3), 3);
    assert_eq!(
        a.requirements, b.requirements,
        "a body-only release requires what its predecessor did"
    );
    assert!(
        !c.requirements.met_by(&a_node.capabilities),
        "a node of A lacks C's native"
    );
    let stored = [a.clone(), b.clone(), c.clone()];
    let a_writes = |serving: &[&NodeCapability]| {
        promoted(&stored, serving, (&a_node.capabilities, a.digest)).map(|set| set.digest)
    };

    assert_eq!(
        a_writes(&[&a_node]),
        Some(a.digest),
        "a fleet of A alone writes under A, though B and C are stored"
    );
    assert_eq!(
        a_writes(&[&a_node, &b_node]),
        Some(b.digest),
        "a body-only release is promoted on A's node once B's is live"
    );
    assert_eq!(
        promoted(
            &stored,
            &[&a_node, &b_node],
            (&b_node.capabilities, b.digest)
        )
        .map(|set| set.digest),
        Some(b.digest),
        "B's node writes under B too"
    );
    assert_eq!(
        a_writes(&[&a_node]),
        Some(a.digest),
        "once B's nodes stop, the fleet rolls back to A"
    );
    assert_eq!(
        a_writes(&[&a_node, &c_node]),
        Some(a.digest),
        "a new native waits while A's node lacks it"
    );
    assert_eq!(
        promoted(
            &stored,
            &[&a_node, &c_node],
            (&c_node.capabilities, c.digest)
        )
        .map(|set| set.digest),
        Some(a.digest),
        "C's node writes under A while A's node is live"
    );
    assert_eq!(
        promoted(&stored, &[&c_node], (&c_node.capabilities, c.digest)).map(|set| set.digest),
        Some(c.digest),
        "once every live node has the native, C is promoted"
    );
}

/// `q@2` whose `q.v` calls the helper `q.base`, no member of its surface,
/// with a new member `q.twice`.
fn q_with_helper() -> Package {
    Package::build(
        "q",
        "2.0.0",
        &PackageSources {
            kernel: "use q.base\n\nfunction q.base() -> Int\nkernel 1\ncharge 1\nbody {\n  return 2\n}\n\nfunction q.v() -> Int\nkernel 1\ncharge 2\nbody {\n  let value = invoke q.base()\n  return value\n}\n\nfunction q.twice() -> Int\nkernel 1\ncharge 2\nbody {\n  return 4\n}\n",
            surface: "function v() -> Int = q.v\nfunction twice() -> Int = q.twice\n",
            labels: "v\tvalue\ntwice\ttwice\n",
        },
        &[],
    )
    .expect("q builds")
}

/// INSTALL-STORES-CODE: the installer of a set stores its surface, every
/// definition (helpers included), the index of which packages declare each
/// definition, and its artifacts' labels. None of it is the installer
/// node's: once that node stops and the store collects, a node that does
/// not hold the set's artifacts reads the whole surface and its code from
/// the store.
#[tokio::test]
async fn a_stored_set_outlives_its_installer_and_collection() {
    use lash_core::durable_port::NodeSpec;
    use lash_core::durable_port::domain::pages::{PageStore as _, RetirementBudget};

    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores.clone());
    let b = Arc::new(HeldPackages::install([q_with_helper()]).expect("B"));
    let installer = NodeHold::new(Arc::clone(&b));
    let holding = lash_execution_contracts::backend::NodePackages::hold(&installer, &backend)
        .await
        .expect("B installs");
    let durable = backend.durable();
    let lease = durable
        .register_node(&NodeSpec {
            node: lash_core::durable_port::NodeId::new("installer"),
            decodes: Vec::new(),
            serves: Vec::new(),
            held: holding.held,
            capabilities: holding.capabilities,
            ttl_millis: 15_000,
        })
        .await
        .expect("the installer serves");
    durable
        .release_node(&lease)
        .await
        .expect("the installer stops");
    let pages = stores.durable_store();
    while {
        let step = pages
            .retire_step(RetirementBudget { nodes: 1024 })
            .await
            .expect("a retirement step");
        step.holds_advanced != 0 || step.nodes_deleted != 0
    } {}
    stores
        .session_store_factory()
        .gc_unreachable()
        .await
        .expect("the collection completes");
    assert!(
        durable.live_nodes().await.expect("live nodes").is_empty(),
        "the installer's advertisement ended with it"
    );

    let digest = b.own();
    let surface = b.surface(&digest).expect("B's surface");
    let q_path =
        lash_kernel_package::PackagePath::new(lash_kernel_package::Segment::new("q").expect("q"));
    let definitions = surface
        .package_definitions(&q_path)
        .expect("q's definitions")
        .clone();
    let helper = b
        .functions()
        .iter()
        .find(|(_, function)| function.name() == "q.base")
        .map(|(id, _)| *id)
        .expect("the helper");
    assert!(
        definitions.contains(&helper),
        "the snapshot lists the helper"
    );
    let paths = durable
        .package_paths(&definitions.iter().copied().collect::<Vec<_>>())
        .await
        .expect("the membership index");
    assert_eq!(
        paths,
        definitions
            .iter()
            .map(|function| (*function, BTreeSet::from(["q".to_owned()])))
            .collect(),
        "every definition of B, the helper included, is indexed to q"
    );
    let artifact = surface.package(&q_path).expect("q").digest();
    let labels = durable.package_labels(&[artifact]).await.expect("labels");
    assert_eq!(
        labels[&artifact]
            .iter()
            .map(|text| lash_kernel_package::PackageLabels::decode(text)
                .expect("labels")
                .release)
            .collect::<Vec<_>>(),
        ["2.0.0"],
        "B's labels"
    );

    let a = Arc::new(HeldPackages::install([q(1)]).expect("A"));
    let reader = FleetPackages::new(backend.clone(), Arc::new(NodeHold::new(a)));
    let cell = reader.surface(digest).await.expect("B's stored surface");
    assert_eq!(cell.digest(), digest);
    assert_eq!(
        cell.stored_definitions()
            .keys()
            .copied()
            .collect::<BTreeSet<_>>(),
        definitions,
        "the reader holds none of B's code and reads all of it"
    );
    let view = cell.view();
    let twice = view
        .packages()
        .find_map(|package| package.function("q.twice"))
        .expect("B's new member");
    assert!(
        view.catalog().get(&twice).is_some(),
        "the new member's code"
    );
    assert!(view.catalog().get(&helper).is_some(), "the helper's code");
}
