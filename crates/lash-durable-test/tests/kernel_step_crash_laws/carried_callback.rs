//! L-PIN: the owner retains live predecessor code debt at the carry commit.
use super::*;
use lash::package::*;

const SOURCE: &str = r#"kernel 1
numbers float
import 0 operation typescript.sleep(milliseconds: Int) -> Null
main {
  let callback = fn() { do perform 0(1) as Null return "creation-code" }
  do perform 0(5000) as Null
  let answered = apply callback()
  return answered
}
"#;

const SERVICES: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("carry-services", lash::plugins::FormatVersion::ONE);

fn services_component() -> lash::process::StaticProcessComponent {
    let services = lash_vm_runtime::dialect_services::Services::new(
        lash_vm_runtime::dialect_services::Dialect::TypeScript,
        SERVICES.tool_provider(lash::tool_provider_key!("timer")),
        Duration::from_secs(30),
        Arc::new(|| 0),
    )
    .unwrap();
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(SERVICES.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider_registration(
                lash::tool_provider_key!("timer"),
                services.registration(),
            )
            .with_package_provider(lash::tool_provider_key!("timer"), services),
    )
    .as_component(SERVICES.format())
    .unwrap()
}

const NEGATIVE: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("carry-negative", lash::plugins::FormatVersion::ONE);
#[derive(Clone, Copy, PartialEq, Eq)]
enum Discovery {
    None,
    Negative,
    Resolved,
    Committed,
}
struct Negative(Arc<std::sync::atomic::AtomicUsize>, Discovery);

fn resolved_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "carry:resolved",
        "wip.call",
        "Returns the captured request.",
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .unwrap()
    .with_execution(Duration::from_secs(30))
}
struct Target;
#[lash::async_trait]
impl lash::tools::StaticToolExecute for Target {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(call.args.clone()).into()
    }
}
#[async_trait::async_trait]
impl lash_core::plugin::packages::PackageProvider for Negative {
    fn roots(&self) -> std::collections::BTreeSet<Segment> {
        [Segment::new("wip").unwrap()].into()
    }
    fn advertise(
        &self,
        _: &lash_core::OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<
        Vec<lash_core::plugin::packages::PackageContribution>,
        lash_execution_contracts::PluginError,
    > {
        let advertisement = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
            path: PackagePath::new(Segment::new("wip").unwrap()),
            visibility: Visibility::Public,
            provider: ProviderName {
                owner: NEGATIVE.component().into(),
                key: "negative".into(),
            },
            config: ProviderConfig {
                component: Some(
                    lash_vocabulary::providers::ProcessComponentId::new(NEGATIVE.component())
                        .unwrap(),
                ),
                format: lash::plugins::FormatVersion::ONE,
                value: serde_json::Value::Null,
                payload: None,
            },
            resolution_policy: Default::default(),
        }));
        let publication =
            lash_core::plugin::packages::operation_publication(context, &advertisement)
                .map_err(lash_execution_contracts::PluginError::from)?;
        Ok(vec![lash_core::plugin::packages::PackageContribution {
            advertisement,
            publication,
            routes: Vec::new(),
        }])
    }
    async fn hold(
        &self,
        _: &lash_core::ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl lash_core::plugin::packages::DeferredPackageProvider for Negative {
    fn validate_grant(
        &self,
        grant: &DeferredGrant,
        registry: lash_vocabulary::claims::RegistryKind,
    ) -> Result<(), lash_execution_contracts::process::linking::AdmissionRefusal> {
        if registry == lash_vocabulary::claims::RegistryKind::Process
            && grant.provider.owner == NEGATIVE.component()
            && grant.provider.key == "negative"
        {
            Ok(())
        } else {
            Err(
                lash_execution_contracts::process::linking::AdmissionRefusal::ProviderNotHostable {
                    provider: grant.provider.clone(),
                    registry,
                },
            )
        }
    }
    async fn resolve(
        &self,
        _: &lash_core::DeferredResolveContext<'_>,
        _: &DeferredGrant,
        _: &QualifiedName,
    ) -> Result<
        lash_core::plugin::packages::DeferredAnswer,
        lash_core::plugin::packages::ResolverFault,
    > {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.1 == Discovery::Resolved {
            let definition = resolved_definition();
            let route = lash_core_store::tool_surface::OperationRoute::resolved(
                NEGATIVE.tool_provider(lash::tool_provider_key!("target")),
                definition.clone(),
                None,
            );
            Ok(lash_core::plugin::packages::DeferredAnswer::Resolved {
                definition: Box::new(definition),
                route: Box::new(route),
            })
        } else {
            Ok(lash_core::plugin::packages::DeferredAnswer::Unavailable {
                note: Some("creation-negative".into()),
            })
        }
    }
}
fn negative_component(
    calls: Arc<std::sync::atomic::AtomicUsize>,
    discovery: Discovery,
) -> lash::process::StaticProcessComponent {
    let provider = Arc::new(Negative(calls, discovery));
    let spec = lash::plugins::PluginSpec::new().with_tool_provider(
        lash::tool_provider_key!("target"),
        Arc::new(lash::tools::StaticToolProvider::new(
            vec![resolved_definition()],
            Target,
        )),
    );
    let spec = if discovery == Discovery::Committed {
        spec
    } else {
        spec.with_package_provider(lash::tool_provider_key!("negative"), provider.clone())
            .with_deferred_package_provider(lash::tool_provider_key!("negative"), provider)
    };
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(NEGATIVE.component()),
        spec,
    )
    .as_component(NEGATIVE.format())
    .unwrap()
}

fn core(
    stores: Arc<dyn StoreSet>,
    boot: &str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    discovery: Discovery,
) -> lash::LashCore {
    let backend = lash::durable::DurableBackendBuilder::new(stores)
        .build()
        .unwrap();
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
            workers,
            "typescript",
            bounds,
        )))
        .process_component(Arc::new(services_component()), SERVICES)
        .process_component(Arc::new(negative_component(calls, discovery)), NEGATIVE)
        .commit_budget(lash::CommitBudget::bounded(16 * 1024 * 1024, 4096))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("callback-carry"),
            lash::persistence::LeaseIncarnationId::new(boot),
        ))
        .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_preexisting_callback_authorizes_after_carry_and_a_cold_reopen_on_sqlite() {
    Box::pin(run_callback(SOURCE, Discovery::None)).await;
}

/// L-DEFERRED-PIN: committed negatives are never rediscovered by carries or a cold callback.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_negative_resolution_stays_pinned_across_carry_and_a_cold_reopen_on_sqlite() {
    let source = SOURCE
        .replace(
            "main {",
            "import 1 operation wip.call(input: Json) -> Json\nmain {",
        )
        .replace(
            "return \"creation-code\"",
            "try { do perform 1(null) as Json } catch e {} return \"creation-code\"",
        )
        .replace(
            "  do perform 0(5000)",
            "  let before = apply callback()\n  do perform 0(5000)",
        );
    Box::pin(run_callback(&source, Discovery::Negative)).await;
}

/// L-DEFERRED-PIN: a resolved callback keeps its captured argument and exact
/// committed target through two carries and a cold reopen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_resolved_callback_keeps_its_capture_and_target_across_carry_and_a_cold_reopen_on_sqlite()
{
    let source = SOURCE.replace("main {", "import 1 operation wip.call(input: Json) -> Json\nmain {")
        .replace("  let callback = fn() { do perform 0(1) as Null return \"creation-code\" }",
            "  let captured = {value: \"creation-code\"}\n  let callback = fn() { do perform 0(1) as Null let answer = perform 1(captured) as Json return answer }")
        .replace("  do perform 0(5000)", "  let before = apply callback()\n  do perform 0(5000)");
    Box::pin(run_callback(&source, Discovery::Resolved)).await;
}

async fn run_callback(source: &str, discovery: Discovery) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (stores, _keep) = served::stores(served::Tier::SqliteMemory).await.unwrap();
    let first = core(Arc::clone(&stores), "before", Arc::clone(&calls), discovery);
    let mut base =
        lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16)).component(SERVICES, &());
    if discovery != Discovery::None {
        base = base.component(NEGATIVE, &());
    }
    let base = base.build().unwrap();
    let namespace_pin = lash_core::HostArtifactPin::mint();
    let (packages, mut namespace) = kernel_process::namespace(&first, &namespace_pin, &base).await;
    if discovery != Discovery::None {
        namespace
            .attachment
            .deferred
            .insert(PackagePath::new(Segment::new("wip").unwrap()));
    }
    let (payload, predecessor) =
        publish(&first, source, &base, &namespace_pin, &packages, &namespace).await;
    let pin = lash_core::HostArtifactPin::mint();
    let env = first
        .host_artifacts()
        .publish_process_env(&pin, &predecessor)
        .await
        .unwrap();
    let settings = first
        .processes()
        .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &payload)
        .unwrap();
    let process = first
        .processes()
        .start(
            lash::process::ProcessStartRequest::engine(
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                payload,
                lash::process::EngineLaunch::new(env, settings),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            ),
            first.effect_host(),
        )
        .await
        .unwrap()
        .process_id;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let record = first.processes().get(&process).await.unwrap().unwrap();
            assert!(
                record.terminal().is_none(),
                "the predecessor ended before parking: {record:?}"
            );
            if !record.waits().is_empty()
                && (discovery == Discovery::None
                    || calls.load(std::sync::atomic::Ordering::SeqCst) > 0)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the predecessor commits its timer park");
    if discovery != Discovery::None {
        let use_ = *predecessor
            .execution()
            .uses
            .iter()
            .find(|(_, binding)| {
                matches!(
                    binding,
                    lash_core_store::tool_surface::UseBinding::Deferred(_)
                )
            })
            .unwrap()
            .0;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if first
                    .backend()
                    .stores()
                    .execution_resolutions()
                    .load(&lash_core_store::resolutions::ResolutionKey {
                        execution: lash_core_store::resolutions::ExecutionId::process(&process),
                        use_,
                    })
                    .await
                    .unwrap()
                    .is_some()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the negative resolution commits before carry");
    }
    let use_ = *predecessor
        .execution()
        .uses
        .keys()
        .find(|use_| predecessor.execution().authored.contains(&use_.link))
        .unwrap();
    let addition = lash_kernel_doc::parse_definition(
        "function carry.unused() -> Int\nkernel 1\ncharge 1\nbody { return 99 }",
    )
    .unwrap();
    let next = format!(
        "{source}\ndefinition {}\n",
        serde_json::to_string(&lash_kernel_doc::print_definition(&addition)).unwrap()
    );
    let (payload, successor) =
        publish(&first, &next, &base, &namespace_pin, &packages, &namespace).await;
    let input: lash_vm_runtime::KernelProcessInput =
        serde_json::from_value(payload.clone()).unwrap();
    let lash_vm_runtime::process::InlineRequest::Linked {
        start:
            LinkedStart {
                link: next_link,
                selector: StartSelector::Main,
            },
    } = input.code
    else {
        panic!("successor Main")
    };
    assert_ne!(use_.link, next_link);
    let env = first
        .host_artifacts()
        .publish_process_env(&pin, &successor)
        .await
        .unwrap();
    assert_eq!(
        first
            .processes()
            .succeed(&process, lash::process::ProcessSuccessor::new(payload, env))
            .await
            .unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    let merged = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = first
                .processes()
                .environment(&process)
                .await
                .unwrap()
                .unwrap();
            let lash::process::ProcessAuthority::Engine { env, .. } = view.authority else {
                panic!("engine env")
            };
            if env.execution().authored.contains(&next_link) {
                break env;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the successor pair commits");
    assert_eq!(
        merged.execution().uses.get(&use_),
        predecessor.execution().uses.get(&use_),
        "CARRY-DEBT: the owner commit retains the pre-carry callback's exact use binding"
    );
    assert!(
        merged.execution().roots.contains(&use_.link),
        "live callable code remains retained"
    );
    // A second carry still owes the creation code from before the first one.
    let addition = lash_kernel_doc::parse_definition(
        "function carry.second() -> Int\nkernel 1\ncharge 1\nbody { return 100 }",
    )
    .unwrap();
    let again = format!(
        "{next}\ndefinition {}\n",
        serde_json::to_string(&lash_kernel_doc::print_definition(&addition)).unwrap()
    );
    let (payload, successor) =
        publish(&first, &again, &base, &namespace_pin, &packages, &namespace).await;
    let input: lash_vm_runtime::KernelProcessInput =
        serde_json::from_value(payload.clone()).unwrap();
    let lash_vm_runtime::process::InlineRequest::Linked {
        start: LinkedStart {
            link: final_link, ..
        },
    } = input.code
    else {
        panic!("final Main")
    };
    let env = first
        .host_artifacts()
        .publish_process_env(&pin, &successor)
        .await
        .unwrap();
    assert_eq!(
        first
            .processes()
            .succeed(&process, lash::process::ProcessSuccessor::new(payload, env))
            .await
            .unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = first
                .processes()
                .environment(&process)
                .await
                .unwrap()
                .unwrap();
            let lash::process::ProcessAuthority::Engine { env, .. } = view.authority else {
                panic!("engine env")
            };
            if env.execution().authored.contains(&final_link) {
                assert_eq!(
                    env.execution().uses.get(&use_),
                    predecessor.execution().uses.get(&use_),
                    "successive carries preserve the original callable use"
                );
                assert!(
                    !env.execution().roots.contains(&next_link),
                    "the intermediate start has no surviving values or frames"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second carry commits");
    first.shutdown().await.unwrap();
    drop(first);
    let second = core(
        stores,
        "after",
        Arc::clone(&calls),
        if discovery == Discovery::Resolved {
            Discovery::Committed
        } else {
            discovery
        },
    );
    if discovery == Discovery::Resolved {
        let actor = lash_durable::ActorKey::process(process.as_str()).unwrap();
        let snapshot = second
            .backend()
            .stores()
            .durable_store()
            .claim_facts(None, &actor)
            .await
            .unwrap()
            .actor
            .unwrap();
        let serves = lash::testing::node_serves(&second);
        let missing: Vec<_> = snapshot
            .requires
            .iter()
            .filter(|required| !serves.contains(required))
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "a pinned target requires no retired discovery callback: {missing:?}"
        );
    }
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        second.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    let lash::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the callback settles")
    };
    assert!(
        output.is_success(),
        "the pre-carry callback authorizes and runs: {output:?}"
    );
    assert_eq!(
        output.value_for_projection(),
        if discovery == Discovery::Resolved {
            serde_json::json!({"value":"creation-code"})
        } else {
            serde_json::json!("creation-code")
        }
    );
    if discovery != Discovery::None {
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "committed outcomes stay pinned through both carries and cold resume"
        );
    }
    second.shutdown().await.unwrap();
}

async fn publish(
    core: &lash::LashCore,
    text: &str,
    env: &lash::process::ProcessEnv,
    pin: &lash_core::HostArtifactPin,
    packages: &lash::package::RecordedPackages,
    environment: &lash_vm_runtime::WorkflowEnvironmentRequest,
) -> (serde_json::Value, lash::process::ProcessEnv) {
    let mut document = lash_kernel_doc::parse_document(text).unwrap();
    for import in document.imports.values_mut() {
        if let lash_vocabulary::ImportKind::Operation { signature } = &mut import.kind {
            for accepted in &packages.snapshot.advertisements {
                if let PackageAdvertisement::Enumerated(package) = &accepted.advertisement
                    && package.path == import.name.package
                    && let Some(Member {
                        kind: MemberKind::Operation(offer),
                        ..
                    }) = package.members.get(&import.name.member)
                {
                    *signature = offer.requirement.signature.clone();
                }
            }
        }
    }
    let start = core
        .host_artifacts()
        .publish_workflow(
            pin,
            &lash_kernel_doc::DocumentBytes::from(document),
            &StartSelector::Main,
            environment,
            None,
        )
        .await
        .unwrap();
    let env = core
        .host_artifacts()
        .link_process_env(env, &start, packages, &Default::default())
        .await
        .unwrap();
    let payload = serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        code: lash_vm_runtime::process::InlineRequest::Linked { start },
        args: Default::default(),
    })
    .unwrap();
    (payload, env)
}

/// L-PIN: creation code imported from a code-only Package keeps its accepted
/// publication when the successor does not attach or use that Package.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_code_only_imported_callback_keeps_publication_provenance_across_carry_and_cold_reopen() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (stores, _keep) = served::stores(served::Tier::SqliteMemory).await.unwrap();
    let first = core(
        Arc::clone(&stores),
        "code-before",
        Arc::clone(&calls),
        Discovery::None,
    );
    let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(SERVICES, &())
        .build()
        .unwrap();
    let pin = lash_core::HostArtifactPin::mint();
    let (current, namespace) = kernel_process::namespace(&first, &pin, &base).await;
    let (old_packages, old_namespace, old_publication) = callable_package(
        &first,
        &pin,
        &current,
        &namespace,
        "carried",
        "creation-code",
    )
    .await;
    let old_link = old_packages
        .snapshot
        .advertisements
        .iter()
        .find_map(|accepted| {
            if accepted.publication != old_publication {
                return None;
            }
            let PackageAdvertisement::Enumerated(package) = &accepted.advertisement else {
                return None;
            };
            match &package.members[&Segment::new("make").unwrap()].kind {
                MemberKind::Export { link, .. } => Some(*link),
                _ => None,
            }
        })
        .unwrap();
    let source = "kernel 1\nnumbers float\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\nimport 1 code carried.make() -> Any direct\nmain { let callback = invoke import 1() do perform 0(5000) as Null let answer = apply callback() return answer }";
    let (payload, predecessor) =
        publish(&first, source, &base, &pin, &old_packages, &old_namespace).await;
    let env = first
        .host_artifacts()
        .publish_process_env(&pin, &predecessor)
        .await
        .unwrap();
    let settings = first
        .processes()
        .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &payload)
        .unwrap();
    let process = first
        .processes()
        .start(
            lash::process::ProcessStartRequest::engine(
                lash_vm_runtime::LASH_VM_ENGINE_KIND,
                payload,
                lash::process::EngineLaunch::new(env, settings),
                lash::process::ProcessOriginator::host(),
                lash::process::LifetimeDecision::Detached,
            ),
            first.effect_host(),
        )
        .await
        .unwrap()
        .process_id;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let process = first.processes().get(&process).await.unwrap().unwrap();
            assert!(
                process.terminal().is_none(),
                "the imported callback parks: {process:?}"
            );
            if !process.waits().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The replacement namespace offers different code at a different path.
    // Nothing the host authors for the successor lends the old publication.
    let (new_packages, new_namespace, _) = callable_package(
        &first,
        &pin,
        &current,
        &namespace,
        "replacement",
        "successor-code",
    )
    .await;
    let next = source.replace("code carried.make", "code replacement.make");
    let (payload, successor) =
        publish(&first, &next, &base, &pin, &new_packages, &new_namespace).await;
    assert!(
        !successor
            .execution()
            .publications
            .contains(&old_publication)
    );
    let next_link = *successor.execution().authored.iter().next().unwrap();
    let env = first
        .host_artifacts()
        .publish_process_env(&pin, &successor)
        .await
        .unwrap();
    assert_eq!(
        first
            .processes()
            .succeed(&process, lash::process::ProcessSuccessor::new(payload, env))
            .await
            .unwrap(),
        lash::process::ProcessSuccession::Requested
    );
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let view = first
                .processes()
                .environment(&process)
                .await
                .unwrap()
                .unwrap();
            let lash::process::ProcessAuthority::Engine { env, .. } = view.authority else {
                panic!("engine environment")
            };
            if env.execution().authored.contains(&next_link) {
                assert!(
                    env.execution().publications.contains(&old_publication),
                    "live imported creation code retains its accepted code-only Package publication"
                );
                assert!(
                    env.execution()
                        .retained_code
                        .iter()
                        .any(|callable| callable.link == old_link),
                    "the runtime owes a callback from the original package"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the successor commits");
    first.host_artifacts().release(pin).await.unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    let second = core(stores, "code-after", calls, Discovery::None);
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        second.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    let lash::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the callback settles")
    };
    assert!(
        output.is_success(),
        "the imported callback authorizes after a cold reopen: {output:?}"
    );
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!("creation-code")
    );
    second.shutdown().await.unwrap();
}

async fn callable_package(
    core: &lash::LashCore,
    pin: &lash_core::HostArtifactPin,
    packages: &RecordedPackages,
    namespace: &lash_vm_runtime::WorkflowEnvironmentRequest,
    path: &str,
    answer: &str,
) -> (
    RecordedPackages,
    lash_vm_runtime::WorkflowEnvironmentRequest,
    PublicationRecord,
) {
    use lash_core::plugin::packages::{PackagePublisher as _, PublishPackage};
    let (definition, locals) = lash_kernel_doc::parse_definition_with_locals(&format!(
        "function {path}.make() -> Any\nkernel 1\nmetering statements\nbody {{ let capture = {answer:?} let callback = fn() {{ return capture }} return callback }}"
    )).unwrap();
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    document.main = None;
    document.exports.insert(
        Name::new("make"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function: definition.identity(),
            signature: definition.signature.clone(),
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    for definition in locals.into_iter().chain([definition]) {
        document.functions.insert(definition.identity(), definition);
    }
    let claim =
        lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(pin.clone()))
            .unwrap();
    let documents = lash_vm_runtime::KernelDocuments::new(
        core.backend().module_artifacts(),
        core.backend().package_store(),
    );
    let id = documents
        .admit_document(&claim, &document.into())
        .await
        .unwrap();
    let path = PackagePath::parse(path).unwrap();
    let context = PublicationContext {
        root: path.as_slice()[0].clone(),
        ..namespace.context.clone()
    };
    let code = documents
        .fulfil(&FulfilRequest {
            document: id,
            selector: ReachSelector::AllExports,
            snapshot: namespace.snapshot.clone(),
            context: context.clone(),
            purpose: LinkPurpose::PackageCode { path: path.clone() },
        })
        .await
        .unwrap();
    let link = code.content.identity();
    documents
        .hold(&claim, &ContentRef::Link(link))
        .await
        .unwrap();
    let package = Package {
        path: path.clone(),
        members: [(
            Segment::new("make").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link,
                    export: ExportName {
                        name: Name::new("make"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    };
    let publisher = core
        .host_artifacts()
        .package_publisher(context.clone(), namespace.snapshot.clone())
        .unwrap();
    let publication = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: package.clone(),
                code: vec![code],
            },
        )
        .await
        .unwrap();
    let mut packages = packages.clone();
    packages
        .snapshot
        .advertisements
        .push(AcceptedAdvertisement {
            publication: publication.clone(),
            advertisement: PackageAdvertisement::Enumerated(package),
        });
    let mut namespace = namespace.clone();
    namespace.snapshot = packages.snapshot.clone();
    namespace.attachment.packages.insert(path);
    (packages, namespace, publication)
}
