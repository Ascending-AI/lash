//! The package gate (FIG-6993) over releases of a host package `q`:
//! PROMOTE-BY-CAPABILITY as a selector and through the store, its typed
//! refusal for a nonempty incompatible fleet, empty-fleet installation,
//! and the stored surface and code a node that did not build a set reads it from.

use futures_util::StreamExt;
use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core::durable_port::{NodeCapability, NodeId, NodeSpec};
use lash_execution_contracts::PackageGate as _;
use lash_kernel_package::{HeldPackages, Package, PackageSet, PackageSources};

use super::{FleetPackages, NodeInstall, members, promoted};

fn q(value: i64) -> Package {
    let kernel =
        format!("function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n");
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: "function v() -> Int = q.v\n",
            labels: r#"{"members": {"v": "value"}}"#,
        },
        &[],
    )
    .expect("q builds")
}

/// OP-INTERNAL / P3: an import and a hand-authored document call an internal
/// operation only through its package body. The gate's older set, stored
/// by the build that installed it and read back by a newer node, pins the
/// published definition and its runtime view, and the newer node runs it.
#[tokio::test]
async fn an_imported_definition_keeps_its_gate_set_and_runs_its_internal_body() {
    Box::pin(imported_internal_body(false)).await;
}

/// EMPTY-FLEET-OWN / INSTALL-BEFORE-ANSWER / BOOTSTRAP-PIN-SURVIVES:
/// authoring installs without a lease, ignores historical releases, and
/// admitted work runs its original internal body on another build.
#[tokio::test]
async fn an_empty_fleet_answers_its_writers_durably_installed_surface() {
    Box::pin(imported_internal_body(true)).await;
}

async fn imported_internal_body(bootstrap: bool) {
    let package = |release: &str| {
        Package::build("dataframe", release, &PackageSources {
            kernel: &format!("effect dataframe.run(input?: Json) -> Text contract \"run/1\"\n\nfunction dataframe.execute() -> Text\nkernel 1\ncharge {}\nbody {{\n  let got = perform dataframe.run({{}}) as Text\n  return {}\n}}\n", if release == "old" { 2 } else { 3 }, if release == "old" { "got" } else { "\"new-body\"" }),
            surface: "internal operation run(input?: Json) -> Text contract \"run/1\"\nasync function execute() -> Text = dataframe.execute\n",
            labels: r#"{"members": {"execute": "execute"}}"#,
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
    let held = Arc::new(
        HeldPackages::new(
            shipped.artifacts().values().cloned().chain([new.clone()]),
            new_set,
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
    let old_held = Arc::new(
        HeldPackages::install(
            shipped
                .in_order()
                .into_iter()
                .map(|p| (**p).clone())
                .chain([(*old).clone(), (*legacy).clone()]),
        )
        .unwrap(),
    );
    assert_eq!(old_held.digest(), pin);
    let older = if bootstrap {
        None
    } else {
        // The newer build installs first; a rollback installs after it.
        lash_execution_contracts::backend::NodePackages::install(
            &NodeInstall::new(held.clone()),
            &backend,
        )
        .await
        .unwrap();
        Some(serve(&backend, "old-import-serving-node", &old_held).await)
    };
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
    let make_core = |held: Arc<HeldPackages>, serves: bool| {
        let workers = lash_vm_client::service::Service::default();
        let bounds = workers.config().run_bounds;
        crate::LashCore::standard_builder(backend.clone())
            .serve_sessions(serves)
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
    let core = make_core(
        if bootstrap {
            old_held.clone()
        } else {
            held.clone()
        },
        !bootstrap,
    );
    let chosen = if bootstrap {
        assert!(durable.package_catalog().await.unwrap().sets.is_empty());
        assert!(durable.live_nodes().await.unwrap().is_empty());
        let chosen = core.host_artifacts().cell_packages().await.unwrap();
        assert_eq!(chosen.digest(), pin);
        assert!(
            durable.live_nodes().await.unwrap().is_empty(),
            "authoring grants no lease"
        );
        let stored = durable.package_surface(&pin).await.unwrap().unwrap();
        assert_eq!(stored, chosen.surface().encode());
        let reader = FleetPackages::new(backend.clone(), Arc::new(NodeInstall::new(held.clone())));
        let read = reader.surface(pin).await.unwrap();
        let definitions: BTreeSet<_> = old_held
            .artifacts()
            .values()
            .flat_map(|package| package.code().ids().iter().copied())
            .collect();
        let paths = durable
            .package_paths(&definitions.iter().copied().collect::<Vec<_>>())
            .await
            .unwrap();
        assert_eq!(paths.keys().copied().collect::<BTreeSet<_>>(), definitions);
        assert!(
            definitions
                .iter()
                .all(|id| read.catalog().get(id).is_some()),
            "every helper is stored"
        );
        let before = durable.package_catalog().await.unwrap();
        assert_eq!(core.host_artifacts().cell_packages().await.unwrap(), chosen);
        assert_eq!(
            durable.package_catalog().await.unwrap(),
            before,
            "installation is idempotent"
        );
        let historical = serve(&backend, "newer-historical-release", &held).await;
        durable.release_node(&historical).await.unwrap();
        assert_eq!(
            core.host_artifacts().cell_packages().await.unwrap(),
            chosen,
            "the caller wins even after a newer historical installation"
        );
        chosen
    } else {
        // Include the importing core in the nonempty survey.
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while core.node.registration.snapshot().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the importing core's node registered");
        let chosen = core.host_artifacts().cell_packages().await.unwrap();
        assert!(
            chosen.stored_wire().is_some(),
            "the newer node reads the older stored surface"
        );
        chosen
    };
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
            chosen.clone().declare_all(),
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
    assert_eq!(packages.packages(), &chosen);
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
    }
    drop(artifacts);
    let core = if bootstrap {
        assert!(durable.live_nodes().await.unwrap().is_empty());
        for process in &started {
            let observed = core.processes().get(process).await.unwrap().unwrap();
            assert!(
                observed.lifecycle.terminal().is_none(),
                "admission does not execute work"
            );
        }
        core.shutdown().await.unwrap();
        drop(core);
        make_core(held.clone(), true)
    } else {
        core
    };
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
    let core = make_core(held.clone(), true);
    let current = held.view();
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
    if let Some(older) = older {
        durable.release_node(&older).await.unwrap();
    }
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
    let stored = lash_core::durable_port::StoredSet {
        digest: held.digest(),
        members: members(held.set()),
        sequence,
        requirements: held.surface().requirements().clone(),
    };
    let live = NodeCapability {
        decodes: Vec::new(),
        capabilities: lash_kernel_package::node_capabilities(&held),
    };
    (stored, live)
}

/// `q@value` with a native-only `q.fast`: what a release adding a native
/// requires of every node.
fn q_with_native(value: i64) -> Package {
    q_native(value, "fast")
}

/// `q@value` with the native-only member `q.<native>`.
fn q_native(value: i64, native: &str) -> Package {
    q_natives(value, &[native])
}

/// `q@value` whose `q.v` returns `value`, with a native-only member per
/// name in `natives`.
fn q_natives(value: i64, natives: &[&str]) -> Package {
    let mut kernel =
        format!("function q.v() -> Int\nkernel 1\ncharge 1\nbody {{\n  return {value}\n}}\n");
    let mut surface = "function v() -> Int = q.v\n".to_owned();
    let mut labels = serde_json::json!({"members": {"v": "value"}});
    for native in natives {
        kernel.push_str(&format!(
            "\nfunction q.{native}() -> Int\nkernel 1\ncharge 1\nnative\n"
        ));
        surface.push_str(&format!("function {native}() -> Int = q.{native}\n"));
        labels["members"][native] = (*native).into();
    }
    Package::build(
        "q",
        &format!("{value}.0.0"),
        &PackageSources {
            kernel: &kernel,
            surface: &surface,
            labels: &labels.to_string(),
        },
        &[],
    )
    .expect("q builds")
}

/// PROMOTE-BY-CAPABILITY: the candidates are the own surfaces of the
/// live serving nodes, and the newest by install
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
        promoted(&stored, serving, &a_node.capabilities).map(|set| set.digest)
    };

    assert_eq!(
        a_writes(&[]),
        None,
        "an unregistered writer supplies no live candidate"
    );
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
        promoted(&stored, &[&a_node, &b_node], &b_node.capabilities).map(|set| set.digest),
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
        promoted(&stored, &[&a_node, &c_node], &c_node.capabilities).map(|set| set.digest),
        Some(a.digest),
        "C's node writes under A while A's node is live"
    );
    assert_eq!(
        promoted(&stored, &[&c_node], &c_node.capabilities).map(|set| set.digest),
        Some(c.digest),
        "once every live node has the native, C is promoted"
    );
}

/// A live serving node of `held`'s build named `name`, installed in
/// `backend`'s store: what the gate reads as one of the fleet.
async fn serve(
    backend: &crate::Backend,
    name: &str,
    held: &Arc<HeldPackages>,
) -> lash_core::durable_port::NodeLease {
    let capabilities = lash_execution_contracts::backend::NodePackages::install(
        &NodeInstall::new(Arc::clone(held)),
        backend,
    )
    .await
    .expect("the build installs");
    backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new(name),
            decodes: vec![backend.formats().session().clone()],
            serves: Vec::new(),
            capabilities,
            ttl_millis: 60_000,
        })
        .await
        .expect("the node serves")
}

/// What the gate of a node of `held`'s build answers now: the digest it
/// writes cells under, or the unmet requirements of each candidate its
/// typed refusal names.
async fn answer(
    backend: &crate::Backend,
    held: &Arc<HeldPackages>,
) -> Result<lash_kernel_doc::PackageSetDigest, Vec<(String, Vec<String>)>> {
    FleetPackages::new(
        backend.clone(),
        Arc::new(NodeInstall::new(Arc::clone(held))),
    )
    .writable()
    .await
    .map(|cell| cell.digest())
    .map_err(|refused| {
        assert_eq!(
            refused.code,
            lash_core::RuntimeErrorCode::NoCompatibleSnapshot,
            "{refused:?}"
        );
        refused
            .no_compatible_snapshot_candidates()
            .expect("the refusal names its candidates")
            .iter()
            .map(|candidate| (candidate.surface.clone(), candidate.unmet.clone()))
            .collect()
    })
}

/// PROMOTE-BY-CAPABILITY through the store (FIG-6993): a body-only
/// release B is written on A's node once B's node is live, from B's stored
/// surface; when B's nodes stop, A's surface is written again. A release D
/// adding a native A's node lacks is held while A's node is live: both
/// nodes write A, which both run. A release C
/// that replaces A's native with another is a mixed fleet beside A: each
/// surface needs a native the other's node lacks, so a new cell fails with
/// the typed `NoCompatibleSnapshot` refusal naming both, never a partial
/// surface; once A's node stops, C's node writes under C.
#[tokio::test]
async fn the_gate_writes_the_newest_live_surface_every_serving_node_runs_or_refuses_typed() {
    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores);
    let durable = backend.durable();
    let release = |value, native| {
        Arc::new(HeldPackages::install([q_native(value, native)]).expect("a release"))
    };
    let (a, b, c) = (release(1, "fast"), release(2, "fast"), release(3, "quick"));
    let a_node = serve(&backend, "a", &a).await;
    assert_eq!(
        answer(&backend, &a).await,
        Ok(a.digest()),
        "A's fleet writes A"
    );
    let b_node = serve(&backend, "b", &b).await;
    assert_eq!(
        answer(&backend, &a).await,
        Ok(b.digest()),
        "a body-only release is promoted on A's node once B's is live"
    );
    durable.release_node(&b_node).await.expect("B stops");
    assert_eq!(
        answer(&backend, &a).await,
        Ok(a.digest()),
        "once B's nodes stop, A is written again"
    );
    let d = Arc::new(HeldPackages::install([q_natives(4, &["fast", "extra"])]).expect("D"));
    let d_node = serve(&backend, "d", &d).await;
    for (node, held) in [("A", &a), ("D", &d)] {
        assert_eq!(
            answer(&backend, held).await,
            Ok(a.digest()),
            "a release adding a native A's node lacks is held on {node}'s node"
        );
    }
    durable.release_node(&d_node).await.expect("D stops");
    let c_node = serve(&backend, "c", &c).await;
    let refused = answer(&backend, &a)
        .await
        .expect_err("no surface runs on both nodes");
    let named: BTreeSet<String> = refused.iter().map(|(surface, _)| surface.clone()).collect();
    assert_eq!(
        named,
        BTreeSet::from([a.digest().to_string(), c.digest().to_string()]),
        "the refusal names both live surfaces"
    );
    for (surface, unmet) in &refused {
        assert!(
            unmet.len() == 1 && unmet[0].starts_with("native "),
            "{surface} lacks one native: {unmet:?}"
        );
    }
    assert!(answer(&backend, &c).await.is_err(), "C's node refuses too");
    durable.release_node(&a_node).await.expect("A stops");
    assert_eq!(
        answer(&backend, &c).await,
        Ok(c.digest()),
        "once every live node runs C, C is written"
    );
    assert!(
        answer(&backend, &a).await.is_err(),
        "the writer must run the chosen surface too"
    );
    durable.release_node(&c_node).await.expect("C stops");
    assert_eq!(
        answer(&backend, &c).await,
        Ok(c.digest()),
        "an empty fleet installs and answers the writer's own set"
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
            labels: r#"{"members": {"v": "value", "twice": "twice"}}"#,
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
    use lash_core::durable_port::domain::pages::{PageStore as _, RetirementBudget};

    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores.clone());
    let b = Arc::new(HeldPackages::install([q_with_helper()]).expect("B"));
    let installer = NodeInstall::new(Arc::clone(&b));
    let capabilities =
        lash_execution_contracts::backend::NodePackages::install(&installer, &backend)
            .await
            .expect("B installs");
    let durable = backend.durable();
    let lease = durable
        .register_node(&NodeSpec {
            node: lash_core::durable_port::NodeId::new("installer"),
            decodes: Vec::new(),
            serves: Vec::new(),
            capabilities,
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

    let digest = b.digest();
    let surface = b.surface();
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
    let reader = FleetPackages::new(backend.clone(), Arc::new(NodeInstall::new(a)));
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

/// `s.get(name)`, as the source `name` writes it, performing `s.raw`.
fn raw_reader(name: &str) -> String {
    format!(
        "function {name}(name: Text) -> Any\nkernel 1\ncharge 1\n\
         effect s.raw(name: Text) -> Any contract \"raw/1\"\n\
         body {{\n  let value = perform s.raw(name) as Any\n  return value\n}}\n"
    )
}

/// R5 (FIG-6985) over the store an install writes: `s` declares `raw`
/// internal, and its helper `s.get` performs it. A node that never built
/// `s` admits `s.get`'s stored code, which the install indexed as `s`'s,
/// and refuses at admission a stored definition named `s.get2` performing
/// the same operation, which no install indexed, naming it and `s.raw`.
#[tokio::test]
async fn stored_code_performs_an_internal_operation_only_as_its_packages_own() {
    let s = Package::build(
        "s",
        "1.0.0",
        &PackageSources {
            kernel: &raw_reader("s.get"),
            surface: "internal operation raw(name: Text) -> Any contract \"raw/1\"\n\
                      async function get(name: Text) -> Any = s.get\n",
            labels: "",
        },
        &[],
    )
    .expect("s builds");
    let get = s.function("s.get").expect("s's helper");
    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores.clone());
    let installed = Arc::new(HeldPackages::install([s]).expect("s's set"));
    lash_execution_contracts::backend::NodePackages::install(
        &NodeInstall::new(Arc::clone(&installed)),
        &backend,
    )
    .await
    .expect("s installs");
    let stored = |text: &str| {
        let definition = lash_kernel_doc::parse_definition(text).expect("a definition");
        (definition.identity(), Arc::new(definition))
    };
    let (genuine, forged) = (stored(&raw_reader("s.get")), stored(&raw_reader("s.get2")));
    assert_eq!(genuine.0, get, "the stored helper is s's own code");
    let authority = lash_vm_runtime::StoredAuthority::new(stores.clone());
    assert_eq!(
        lash_vm_runtime::admit_internal(&authority, &[genuine.clone()].into_iter().collect()).await,
        Ok(Ok(())),
        "s's own helper performs its internal operation"
    );
    assert_eq!(
        lash_vm_runtime::admit_internal(
            &authority,
            &[genuine, forged.clone()].into_iter().collect()
        )
        .await,
        Ok(Err(lash_vm_runtime::LoadRefusal::InternalOperation {
            function: forged.0,
            operation: lash_kernel_doc::EffectName::new("s.raw").expect("an operation"),
        })),
    );
}

/// NATIVE-ROLLBACK (FIG-6993): a build D adds a native `extra`; work it
/// parks records D's natives. D's nodes stop and the older build A, which
/// lacks `extra`, is redeployed: A's node claims the work whose record it
/// meets and leaves D's parked, and the strand report names D's work with
/// the one native no live node has.
#[tokio::test]
async fn a_rollback_build_serves_what_it_can_and_the_strand_report_names_the_rest() {
    use lash_core::ObligationToken;
    use lash_core::durable_port::{ActorKey, ClaimPurpose, CommitLabel, MailTx, Release};
    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores);
    let durable = backend.durable();
    let a = Arc::new(HeldPackages::install([q_native(1, "fast")]).expect("A"));
    let d = Arc::new(HeldPackages::install([q_natives(2, &["fast", "extra"])]).expect("D"));
    let record = |held: &HeldPackages| super::install_rows(held).1[0].requirements.clone();
    let tokens = |held: &HeldPackages| -> Vec<String> {
        let mut tokens: Vec<String> = ObligationToken::for_requirements(&record(held))
            .map(String::from)
            .collect();
        tokens.sort();
        tokens
    };
    let extra: BTreeSet<_> = record(&d)
        .natives
        .difference(&record(&a).natives)
        .copied()
        .collect();
    assert_eq!(extra.len(), 1, "D adds one native");

    let d_node = serve(&backend, "d", &d).await;
    let (older, newer) = (
        ActorKey::process("parked-under-a").unwrap(),
        ActorKey::process("parked-under-d").unwrap(),
    );
    let mut create = MailTx::new();
    for actor in [&older, &newer] {
        create.create_actor(actor.clone(), backend.formats().session().clone());
    }
    durable
        .commit_mail(create, CommitLabel::new("law.create"))
        .await
        .expect("the actors are created");
    let claimed = durable.claim(&d_node, 8).await.expect("D claims");
    assert_eq!(claimed.len(), 2, "D's node runs both: {claimed:?}");
    for claim in claimed {
        let requires = if claim.actor == newer {
            tokens(&d)
        } else {
            tokens(&a)
        };
        let mut park = durable
            .begin(&claim.actor, claim.epoch)
            .await
            .expect("the owner opens");
        park.set_requires(requires).give_up(Release::Ready);
        durable
            .commit(park, CommitLabel::new("law.park"))
            .await
            .expect("the work parks with its record");
    }
    durable.release_node(&d_node).await.expect("D's nodes stop");

    let a_node = serve(&backend, "a", &a).await;
    let claimed = durable.claim(&a_node, 8).await.expect("A claims");
    assert!(
        claimed.len() == 1 && claimed[0].actor == older && claimed[0].purpose == ClaimPurpose::Run,
        "the rollback build runs only the work it meets: {claimed:?}"
    );
    let page = crate::durable::strand_report(&backend, None, 64)
        .await
        .expect("the strand report reads");
    assert_eq!(page.next, None);
    assert_eq!(
        page.stranded,
        vec![crate::durable::StrandedWork {
            actor: newer.as_str().to_owned(),
            lacks: lash_core::durable_port::ExecutionRequirements {
                natives: extra,
                ..Default::default()
            },
        }],
        "the strand report names D's work and the native no live node has"
    );
    durable.release_node(&a_node).await.expect("A stops");
}

/// LIVE-FLEET-STILL-STRICT: serving formats, rather than candidate count,
/// decide whether bootstrap is allowed. Carried sessions count as serving.
#[tokio::test]
async fn only_an_empty_serving_fleet_uses_the_writers_own_surface() {
    use lash_core::durable_port::{FormatSet, NodeCapabilities};
    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores);
    let held = Arc::new(HeldPackages::install([q(1)]).unwrap());
    let formats = backend.formats();
    for (name, decodes, qualifies) in [
        ("unrelated", FormatSet::new("unrelated-format"), false),
        ("current", formats.session().clone(), true),
    ] {
        let lease = backend
            .durable()
            .register_node(&NodeSpec {
                node: NodeId::new(name),
                decodes: vec![decodes],
                serves: vec![],
                capabilities: NodeCapabilities::default(),
                ttl_millis: 60_000,
            })
            .await
            .unwrap();
        if qualifies {
            assert_eq!(
                answer(&backend, &held).await,
                Err(vec![]),
                "a serving node without a candidate still refuses"
            );
        } else {
            assert_eq!(answer(&backend, &held).await, Ok(held.digest()));
        }
        backend.durable().release_node(&lease).await.unwrap();
    }
    let carried = FormatSet::new("carried-session-format");
    let row = NodeCapability {
        decodes: vec![carried.clone()],
        capabilities: NodeCapabilities::default(),
    };
    assert!(super::serving(std::slice::from_ref(&row), formats.session(), &[]).is_empty());
    assert_eq!(
        super::serving(std::slice::from_ref(&row), formats.session(), &[carried]),
        vec![&row]
    );
}

/// HARD-CLAIM-REMAINS: a bootstrap publication's reached native is a real
/// start requirement. A node missing it cannot claim; a capable build
/// later executes the original pinned definition.
#[tokio::test]
async fn a_bootstrap_native_waits_for_a_capable_node() {
    let stores = crate::tests::sqlite_memory_store_set().await;
    let backend = lash_conformance::backend_over(stores);
    let held = lash_vm_releases::lash_packages().unwrap();
    let make_core = |serves| {
        let workers = lash_vm_client::service::Service::default();
        let bounds = workers.config().run_bounds;
        crate::LashCore::standard_builder(backend.clone())
            .serve_sessions(serves)
            .plugin(Arc::new(
                lash_vm_runtime::KernelProcessPluginFactory::new(workers, "typescript", bounds)
                    .with_packages(held.clone()),
            ))
            .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
            .data_retention(crate::DataRetention::standard())
            .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(crate::ExecutionBudgets::recommended())
            .delta_coalescing(crate::DeltaCoalescing::recommended())
            .build(lash_core::testing::runtime_lease_owner())
            .unwrap()
    };
    let publisher = make_core(false);
    let native = held
        .functions()
        .iter()
        .find(|(_, function)| function.name() == "regex.ecma.compile_check")
        .map(|(id, _)| *id)
        .unwrap();
    let document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\nuse regex.ecma.compile_check = @{native}\nentry flow() -> Text\nfn flow() {{ let checked = invoke regex.ecma.compile_check(\"a\", \"\") return \"native-ran\" }}\nmain {{ finish null }}\n"
    )).unwrap();
    let env = lash_core::ProcessEnv::builder(lash_core::MaxToolCalls::new(1))
        .build()
        .unwrap();
    let artifacts = publisher.host_artifacts();
    let packages = artifacts.cell_packages().await.unwrap();
    let pin = crate::process::HostArtifactPin::mint();
    let publication = artifacts
        .publish_workflow(
            &pin,
            &document.into(),
            &lash_kernel_doc::Name::new("flow"),
            &env,
            packages.declare_all(),
            None,
        )
        .await
        .unwrap();
    let crate::workflow::WorkflowPublish::Published(publication) = publication else {
        panic!("{publication:?}");
    };
    let env_ref = artifacts.publish_process_env(&pin, &env).await.unwrap();
    let receipt = publisher
        .processes()
        .start(
            lash_execution_contracts::process::ProcessStartRequest::definition(
                publication.definition.id,
                Some(publication.definition.signature),
                Default::default(),
                lash_execution_contracts::process::EngineLaunch::new(
                    env_ref,
                    publisher
                        .processes()
                        .engine_defaults(
                            lash_vm_runtime::LASH_VM_ENGINE_KIND,
                            &serde_json::Value::Null,
                        )
                        .unwrap(),
                ),
                lash_execution_contracts::process::ProcessOriginator::host(),
                lash_execution_contracts::process::LifetimeDecision::Detached,
            ),
            publisher.effect_host(),
        )
        .await
        .unwrap();
    assert!(backend.durable().live_nodes().await.unwrap().is_empty());
    let mut incapable = lash_kernel_package::node_capabilities(&held);
    assert!(incapable.natives.remove(&native));
    let lease = backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new("missing-bootstrap-native"),
            decodes: publisher.backend().formats().decodes(),
            serves: vec![],
            capabilities: incapable,
            ttl_millis: 60_000,
        })
        .await
        .unwrap();
    assert!(
        backend
            .durable()
            .claim(&lease, 16)
            .await
            .unwrap()
            .is_empty(),
        "a missing native prevents claiming an actual admitted start"
    );
    let stranded = crate::durable::strand_report(&backend, None, 16)
        .await
        .unwrap();
    let actor = lash_core::durable_port::ActorKey::process(receipt.process_id.as_str()).unwrap();
    let stranded = stranded
        .stranded
        .iter()
        .find(|row| row.actor == actor.as_str())
        .unwrap();
    assert_eq!(stranded.lacks.natives, BTreeSet::from([native]));
    assert!(stranded.lacks.kernels.is_empty());
    assert!(stranded.lacks.providers.is_empty());
    assert!(stranded.lacks.codecs.is_empty());
    backend.durable().release_node(&lease).await.unwrap();
    drop(artifacts);
    publisher.shutdown().await.unwrap();
    drop(publisher);
    let core = make_core(true);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&receipt.process_id),
    )
    .await
    .unwrap()
    .unwrap();
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("{output:?}");
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!("native-ran")
    );
    core.shutdown().await.unwrap();
}

mod gate_faults {
    use lash_core::durable_port as lash_durable;
    use lash_durable::domain::{
        ExecKey, ExecRootRow, HydratedGraph, OwnerKey, ProcessActorRow, RootOwner, RunFacts,
        ScopeKey, SessionCloseRow, TurnRow, WaitId, WaitRow,
    };
    use lash_durable::{
        ActorCommit, ActorKey, ActorSnapshot, ActorTx, Claimed, CommitLabel, DurableError,
        DurableInstant, DurableReads, DurableStore, Epoch, HeartbeatOutcome, MailCommit, MailTx,
        NodeLease, NodeSpec, Reaped,
    };
    use lash_protocol_types::{ProcessId, SessionId};
    use std::result::Result;
    use std::sync::{Arc, Mutex};

    use super::*;

    struct GateStore {
        inner: Arc<dyn DurableStore>,
        fail: Mutex<Option<&'static str>>,
    }

    impl GateStore {
        fn check(&self, operation: &str) -> Result<(), DurableError> {
            let mut fail = self.fail.lock().unwrap();
            if *fail == Some(operation) {
                *fail = None;
                return Err(DurableError::Store(lash_durable::StoreFailure {
                    kind: lash_durable::StoreFailureKind::Unavailable,
                    message: format!("injected {operation} failure"),
                }));
            }
            Ok(())
        }
    }

    /// SURVEY-FAILURE-IS-NOT-EMPTY: neither an unreadable survey nor a
    /// failed bootstrap install can yield an uninstalled surface.
    #[tokio::test]
    async fn a_failed_survey_or_install_never_answers_an_own_surface() {
        for operation in ["package_catalog", "live_nodes", "install_packages"] {
            let stores = crate::tests::sqlite_memory_store_set().await;
            let probe = Arc::new(GateStore {
                inner: Arc::new(stores.durable_store()),
                fail: Mutex::new(Some(operation)),
            });
            let wrapped = probe.clone();
            let stores = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
                .map_durable_store(move |_| wrapped)
                .into_store_set();
            let backend = lash_conformance::backend_over(stores);
            let held = Arc::new(HeldPackages::install([q(1)]).unwrap());
            let gate =
                FleetPackages::new(backend.clone(), Arc::new(NodeInstall::new(held.clone())));
            let refused = gate.writable().await.unwrap_err();
            assert_eq!(
                refused.code,
                lash_core::RuntimeErrorCode::RunDefinitionUnavailable
            );
            assert!(
                refused
                    .message
                    .contains(&format!("injected {operation} failure"))
            );
            assert!(
                probe
                    .inner
                    .package_surface(&held.digest())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(probe.inner.live_nodes().await.unwrap().is_empty());
            assert_eq!(gate.writable().await.unwrap().digest(), held.digest());
            assert!(
                probe
                    .inner
                    .package_surface(&held.digest())
                    .await
                    .unwrap()
                    .is_some()
            );
        }
    }
    #[async_trait::async_trait]
    impl DurableReads for GateStore {
        async fn model_call_pins(
            &self,
            session: &lash_protocol_types::SessionId,
            owner: &str,
        ) -> Result<Vec<lash_durable::domain::ModelCallPinRow>, DurableError> {
            self.inner.model_call_pins(session, owner).await
        }

        async fn model_calls(
            &self,
            session: &lash_protocol_types::SessionId,
            owner: &str,
        ) -> Result<Vec<lash_core_store::model_calls::ModelCallRecord>, DurableError> {
            self.inner.model_calls(session, owner).await
        }

        async fn turn(&self, session: &SessionId) -> Result<Option<TurnRow>, DurableError> {
            self.inner.turn(session).await
        }

        async fn turn_namespaces(
            &self,
            session: &SessionId,
            run: &lash_protocol_types::TurnId,
        ) -> Result<Vec<lash_durable::domain::TurnNamespace>, DurableError> {
            self.inner.turn_namespaces(session, run).await
        }

        async fn turn_end(
            &self,
            session: &SessionId,
            run: &lash_protocol_types::TurnId,
        ) -> Result<Option<lash_durable::domain::TurnEnd>, DurableError> {
            self.inner.turn_end(session, run).await
        }

        async fn run_facts(&self, owner: &OwnerKey) -> Result<RunFacts, DurableError> {
            self.inner.run_facts(owner).await
        }

        async fn run_facts_of(
            &self,
            owner: &OwnerKey,
            run: lash_durable::domain::RunSeq,
        ) -> Result<RunFacts, DurableError> {
            self.inner.run_facts_of(owner, run).await
        }
        async fn run_facts_within(
            &self,
            owner: &lash_durable::domain::OwnerKey,
            run: Option<lash_durable::domain::RunSeq>,
            max_rows: usize,
            limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
        ) -> Result<lash_durable::domain::RunFacts, DurableError> {
            self.inner
                .run_facts_within(owner, run, max_rows, limits)
                .await
        }

        async fn exec_root(&self, exec: &ExecKey) -> Result<Option<ExecRootRow>, DurableError> {
            self.inner.exec_root(exec).await
        }

        async fn cell_roots(
            &self,
            session: &lash_protocol_types::SessionId,
            run: &lash_protocol_types::TurnId,
        ) -> Result<Vec<ExecRootRow>, DurableError> {
            self.inner.cell_roots(session, run).await
        }
        async fn cell_roots_within(
            &self,
            session: &lash_protocol_types::SessionId,
            run: &lash_protocol_types::TurnId,
            max_rows: usize,
        ) -> Result<Vec<lash_durable::domain::ExecRootRow>, DurableError> {
            self.inner.cell_roots_within(session, run, max_rows).await
        }

        async fn hydrate(
            &self,
            owner: RootOwner,
            expected: Option<lash_durable::domain::checkpoint_graph::NodeId>,
            limits: lash_durable::domain::checkpoint_graph::DecodeLimits,
        ) -> Result<HydratedGraph, DurableError> {
            self.inner.hydrate(owner, expected, limits).await
        }

        async fn pending_waits(&self, owner: &ActorKey) -> Result<Vec<WaitRow>, DurableError> {
            self.inner.pending_waits(owner).await
        }

        async fn wait(&self, id: &WaitId) -> Result<Option<WaitRow>, DurableError> {
            self.inner.wait(id).await
        }

        async fn process(
            &self,
            process: &ProcessId,
        ) -> Result<Option<ProcessActorRow>, DurableError> {
            self.inner.process(process).await
        }

        async fn live_until_descendants(
            &self,
            scope: &ScopeKey,
            limit: usize,
        ) -> Result<Vec<ProcessId>, DurableError> {
            self.inner.live_until_descendants(scope, limit).await
        }

        async fn until_children(
            &self,
            scope: &ScopeKey,
            after: Option<&ProcessId>,
            limit: usize,
        ) -> Result<Vec<ProcessId>, DurableError> {
            self.inner.until_children(scope, after, limit).await
        }

        async fn session_close(
            &self,
            session: &SessionId,
        ) -> Result<Option<SessionCloseRow>, DurableError> {
            self.inner.session_close(session).await
        }

        async fn ending_scopes(&self, session: &SessionId) -> Result<Vec<ScopeKey>, DurableError> {
            self.inner.ending_scopes(session).await
        }

        async fn session_mailbox(
            &self,
            session: &lash_protocol_types::SessionId,
        ) -> Result<lash_durable::domain::SessionMailbox, DurableError> {
            self.inner.session_mailbox(session).await
        }

        async fn prompt_snapshot(
            &self,
            call: &lash_durable::domain::PromptCallKey,
        ) -> Result<Option<lash_durable::domain::PromptSnapshotRow>, DurableError> {
            self.inner.prompt_snapshot(call).await
        }

        async fn prompt_texts(
            &self,
            hashes: &[String],
        ) -> Result<Vec<lash_durable::domain::PromptText>, DurableError> {
            self.inner.prompt_texts(hashes).await
        }
    }

    #[async_trait::async_trait]
    impl DurableStore for GateStore {
        async fn now(&self) -> Result<DurableInstant, DurableError> {
            self.inner.now().await
        }

        fn pages(&self) -> Option<Arc<dyn lash_durable::domain::pages::PageStore>> {
            self.inner.pages()
        }

        async fn register_node(&self, spec: &NodeSpec) -> Result<NodeLease, DurableError> {
            self.inner.register_node(spec).await
        }

        async fn heartbeat(&self, node: &NodeLease) -> Result<HeartbeatOutcome, DurableError> {
            self.inner.heartbeat(node).await
        }

        async fn reap(&self, reaper: &NodeLease) -> Result<Vec<Reaped>, DurableError> {
            self.inner.reap(reaper).await
        }

        async fn release_node(&self, node: &NodeLease) -> Result<Vec<ActorKey>, DurableError> {
            self.inner.release_node(node).await
        }

        async fn acquire_idle_for_testing(
            &self,
            node: &lash_durable::NodeLease,
            actor: &lash_durable::ActorKey,
            epoch: lash_durable::Epoch,
        ) -> Result<lash_durable::Epoch, lash_durable::DurableError> {
            self.inner
                .acquire_idle_for_testing(node, actor, epoch)
                .await
        }

        async fn readiness(&self) -> Result<lash_durable::Readiness, DurableError> {
            self.inner.readiness().await
        }

        async fn claim(
            &self,
            node: &NodeLease,
            limit: usize,
        ) -> Result<Vec<Claimed>, DurableError> {
            self.inner.claim(node, limit).await
        }

        async fn mark_draining(&self, node: &NodeLease) -> Result<(), DurableError> {
            self.inner.mark_draining(node).await
        }

        async fn live_nodes(&self) -> Result<Vec<lash_durable::NodeCapability>, DurableError> {
            self.check("live_nodes")?;
            self.inner.live_nodes().await
        }

        async fn actor_requirements(
            &self,
            after: Option<&lash_durable::ActorKey>,
            limit: usize,
        ) -> Result<Vec<(lash_durable::ActorKey, Vec<String>)>, DurableError> {
            self.inner.actor_requirements(after, limit).await
        }

        async fn install_packages(
            &self,
            artifacts: &[lash_durable::packages::ArtifactInstall],
            sets: &[lash_durable::packages::SetInstall],
        ) -> Result<(), DurableError> {
            self.check("install_packages")?;
            self.inner.install_packages(artifacts, sets).await
        }

        async fn package_catalog(&self) -> Result<lash_durable::PackageCatalog, DurableError> {
            self.check("package_catalog")?;
            self.inner.package_catalog().await
        }

        async fn package_surface(
            &self,
            digest: &lash_durable::PackageSetDigest,
        ) -> Result<Option<Vec<u8>>, DurableError> {
            self.inner.package_surface(digest).await
        }

        async fn package_paths(
            &self,
            functions: &[lash_durable::packages::FunctionId],
        ) -> Result<
            std::collections::BTreeMap<
                lash_durable::packages::FunctionId,
                std::collections::BTreeSet<String>,
            >,
            DurableError,
        > {
            self.inner.package_paths(functions).await
        }

        async fn internal_operations(
            &self,
            operations: &[lash_durable::packages::EffectName],
        ) -> Result<
            std::collections::BTreeMap<
                lash_durable::packages::EffectName,
                std::collections::BTreeSet<String>,
            >,
            DurableError,
        > {
            self.inner.internal_operations(operations).await
        }

        async fn package_labels(
            &self,
            artifacts: &[lash_durable::ArtifactDigest],
        ) -> Result<
            std::collections::BTreeMap<lash_durable::ArtifactDigest, Vec<String>>,
            DurableError,
        > {
            self.inner.package_labels(artifacts).await
        }

        async fn owned(&self, node: &NodeLease) -> Result<Vec<Claimed>, DurableError> {
            self.inner.owned(node).await
        }

        async fn begin(&self, actor: &ActorKey, epoch: Epoch) -> Result<ActorTx, DurableError> {
            self.inner.begin(actor, epoch).await
        }

        async fn open_process(
            &self,
            actor: &ActorKey,
            epoch: Epoch,
        ) -> Result<(ActorTx, lash_durable::domain::ProcessView), DurableError> {
            self.inner.open_process(actor, epoch).await
        }

        async fn commit(
            &self,
            tx: ActorTx,
            label: CommitLabel,
        ) -> Result<ActorCommit, DurableError> {
            self.inner.commit(tx, label).await
        }

        async fn commit_mail(
            &self,
            tx: MailTx,
            label: CommitLabel,
        ) -> Result<MailCommit, DurableError> {
            self.inner.commit_mail(tx, label).await
        }

        async fn claim_facts(
            &self,
            node: Option<&lash_durable::Owner>,
            actor: &ActorKey,
        ) -> Result<lash_durable::ClaimFacts, DurableError> {
            self.inner.claim_facts(node, actor).await
        }

        async fn actor(&self, actor: &ActorKey) -> Result<Option<ActorSnapshot>, DurableError> {
            self.inner.actor(actor).await
        }

        async fn await_owner_commit(&self, actor: &ActorKey) -> Result<(), DurableError> {
            self.inner.await_owner_commit(actor).await
        }

        async fn actors_in(
            &self,
            formats: &lash_durable::FormatSet,
            after: Option<&ActorKey>,
            limit: usize,
        ) -> Result<Vec<ActorKey>, DurableError> {
            self.inner.actors_in(formats, after, limit).await
        }
    }
}
