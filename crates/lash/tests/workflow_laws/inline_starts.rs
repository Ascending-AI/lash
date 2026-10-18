//! START-ONCE (FIG-6382): a start of a function carried inline realizes it
//! exactly once under its key, with a complete retained closure, whichever
//! write of its realization a crash stops it after, on SQLite.
//!
//! Realization runs in a fixed order: its engine checks the function, the
//! document and the declarations the store lacks are published under
//! `Start(key)`, then the descriptor, then the row registers, then the row
//! adopts what `Start(key)` held. A cut stops the start right at one of
//! those writes, before it or once it committed, and never answers: the
//! node died there. Recovery is the redrive: a host retries the same request
//! under the same key, and a second node recovers a tool's call and realizes
//! the start it declared again. The registration cut stops the start after its row
//! committed, for good; the host's retry, and the retry after the key's
//! staging was ended (its fence), answer that row.
//!
//! Each law checks: one process holds the key and runs the function once;
//! nothing holds its closure but that process, which still reads its
//! document, declarations and descriptor; and a refused or uncommitted start
//! leaves no process and publishes nothing.

use std::sync::Mutex;

use lash::workflow::document::{FunctionValue, parse_document};
use lash_core::ReferrerClaim;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_artifacts::ModuleArtifactHold;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;

use super::*;

/// `run(n)`: answers `[n, "inline"]`. The function a host holds as data.
const RUN: &str = "kernel 1\nnumbers float\nentry run(n: Any) -> Any\n\nfn run(n) {\n  return [n, \"inline\"]\n}\n\nmain {\n  finish null\n}\n";

/// The same function answering `[n, "other"]`: other bytes.
const OTHER: &str = "kernel 1\nnumbers float\nentry run(n: Any) -> Any\n\nfn run(n) {\n  return [n, \"other\"]\n}\n\nmain {\n  finish null\n}\n";

fn function(text: &str) -> FunctionValue {
    FunctionValue {
        document: parse_document(text).expect("the function's document parses"),
        entry: Name::new("run"),
    }
}

fn transport(value: &FunctionValue) -> serde_json::Value {
    value.transport_json().expect("the function value encodes")
}

/// A write a cut stops a start at.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cut {
    /// The document's write is refused: the node died before it.
    BeforeDocument(String),
    /// The document's write committed; its caller gets no answer.
    AfterDocument(String),
    /// The descriptor's write committed; its caller gets no answer.
    AfterDescriptor,
}

/// The armed cut, and every document and descriptor the stores published.
#[derive(Default)]
struct Cuts {
    armed: Mutex<Option<Cut>>,
    reached: tokio::sync::Notify,
    fired: Mutex<Vec<Cut>>,
    /// The referrer a document cut found the document staged under.
    staged_under: Mutex<Option<lash_core::ArtifactReferrer>>,
    published: Mutex<Vec<String>>,
    /// Whether the function's document was unpublished when a crossing
    /// reached the tool, at each `before-declaring` cut.
    crossed_unpublished: Mutex<Vec<bool>>,
}

impl Cuts {
    fn arm(&self, cut: Cut) {
        *self.armed.lock().unwrap() = Some(cut);
    }

    fn fires(&self, at: &Cut) -> bool {
        let mut armed = self.armed.lock().unwrap();
        if armed.as_ref() != Some(at) {
            return false;
        }
        *armed = None;
        self.fired.lock().unwrap().push(at.clone());
        self.reached.notify_one();
        true
    }

    /// Wait until the armed cut stopped a start.
    async fn reached(&self) {
        tokio::time::timeout(std::time::Duration::from_secs(120), self.reached.notified())
            .await
            .expect("the cut is reached");
    }

    fn fired(&self) -> Vec<Cut> {
        self.fired.lock().unwrap().clone()
    }

    fn published(&self, artifact: &str) -> bool {
        self.published
            .lock()
            .unwrap()
            .iter()
            .any(|published| published == artifact)
    }
}

/// The write a start stopped at: the node died, so it never answers.
async fn died<T>() -> T {
    std::future::pending().await
}

struct CutModules {
    inner: Arc<dyn ModuleArtifactStore>,
    cuts: Arc<Cuts>,
}

#[lash::async_trait]
impl ModuleArtifactStore for CutModules {
    fn durability_tier(&self) -> lash_execution_contracts::module_artifacts::DurabilityTier {
        self.inner.durability_tier()
    }

    async fn publish_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
        bytes: &[u8],
    ) -> Result<(), ArtifactStoreError> {
        self.inner
            .publish_module_artifact(claim, module_ref, bytes)
            .await
    }

    async fn acquire_module_artifact(
        &self,
        claim: &ReferrerClaim,
        module_ref: &str,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.acquire_module_artifact(claim, module_ref).await
    }

    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        let documents: Vec<String> = artifacts
            .iter()
            .filter_map(|artifact| match artifact {
                ModuleArtifactHold::Publish { module_ref, .. } => Some((*module_ref).to_owned()),
                ModuleArtifactHold::Acquire { .. } => None,
            })
            .collect();
        for document in &documents {
            if self.cuts.fires(&Cut::BeforeDocument(document.clone())) {
                return died().await;
            }
        }
        self.inner.hold_module_artifacts(claim, artifacts).await?;
        self.cuts
            .published
            .lock()
            .unwrap()
            .extend(documents.iter().cloned());
        for document in documents {
            if self.cuts.fires(&Cut::AfterDocument(document)) {
                *self.cuts.staged_under.lock().unwrap() = Some(claim.referrer());
                return died().await;
            }
        }
        Ok(())
    }

    async fn end_module_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.end_module_referrer(cleanup).await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &str,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.inner.get_module_artifact(module_ref).await
    }
}

struct CutDefinitions {
    inner: Arc<dyn lash_execution_contracts::process::ProcessDefinitionStore>,
    cuts: Arc<Cuts>,
}

#[lash::async_trait]
impl lash_execution_contracts::process::ProcessDefinitionStore for CutDefinitions {
    async fn publish_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_core::ProcessDefinitionId,
        descriptor: &[u8],
        manifest: &[lash_core::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.inner
            .publish_process_definition(claim, id, descriptor, manifest)
            .await?;
        self.cuts.published.lock().unwrap().push(id.to_string());
        if self.cuts.fires(&Cut::AfterDescriptor) {
            return died().await;
        }
        Ok(())
    }

    async fn acquire_process_definition(
        &self,
        claim: &ReferrerClaim,
        id: &lash_core::ProcessDefinitionId,
        manifest: &[lash_core::ArtifactName],
    ) -> Result<(), ArtifactStoreError> {
        self.inner
            .acquire_process_definition(claim, id, manifest)
            .await
    }

    async fn end_process_definition_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> Result<(), ArtifactStoreError> {
        self.inner.end_process_definition_referrer(cleanup).await
    }

    async fn get_process_definition(
        &self,
        id: &lash_core::ProcessDefinitionId,
    ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
        self.inner.get_process_definition(id).await
    }
}

/// The markers of every `tools.launch` attempt.
static LAUNCH_ATTEMPTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// A tool that starts the function it is handed inline with `{n: 3}`, under
/// its enclosing process's environment restricted to what the function
/// performs. Handed `{function, marker, cut}`, its first attempt for a
/// marker arms `cut` once it checked the function: `before-declaring` faults
/// the attempt there, before it declares anything (the node died after the
/// crossing reached the tool), and the others cut the realization of the
/// start it declares at that write of the function's document or
/// descriptor.
struct Launch {
    cuts: Arc<Cuts>,
}

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Launch {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        let context = call.context;
        let marker = call.args["marker"].as_str().unwrap_or_default().to_owned();
        let refuse = |message: String| {
            lash::tools::ToolAttemptOutcome::done_without_intents(
                lash_core::ToolOutcomeDone::failure(lash_core::ToolFailure::new(
                    "tool_error",
                    message,
                )),
            )
        };
        let mut args = serde_json::Map::new();
        args.insert("n".to_owned(), serde_json::json!(3));
        let resolved = match context
            .resolve_inline_start(
                lash::process::LASH_VM_ENGINE_KIND,
                call.args["function"].clone(),
                args,
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => return refuse(format!("{error:?}")),
        };
        let first = {
            let mut attempts = LAUNCH_ATTEMPTS.lock().unwrap();
            let first = !attempts.contains(&marker);
            attempts.push(marker.clone());
            first
        };
        let document = function_document(&call.args["function"]);
        if first {
            match call.args["cut"].as_str() {
                Some("before-declaring") => {
                    self.cuts
                        .crossed_unpublished
                        .lock()
                        .unwrap()
                        .push(!self.cuts.published(&document));
                    self.cuts
                        .fired
                        .lock()
                        .unwrap()
                        .push(Cut::BeforeDocument(String::new()));
                    return lash::tools::ToolAttemptOutcome::host_failed(
                        lash_core::RuntimeEffectControllerError::from(
                            lash_execution_contracts::plugin_error::PluginError::attempt_fault(
                                "the node died before declaring",
                            ),
                        )
                        .retryable_uncommitted_derivation(),
                    );
                }
                Some("before-document") => self.cuts.arm(Cut::BeforeDocument(document)),
                Some("after-document") => self.cuts.arm(Cut::AfterDocument(document)),
                Some("after-descriptor") => self.cuts.arm(Cut::AfterDescriptor),
                _ => {}
            }
        }
        let cx = match context.start_cx() {
            Ok(cx) => cx,
            Err(error) => return refuse(error.to_string()),
        };
        // The child outlives the spawner, which answers its handle at once.
        let lifetime = lash_execution_contracts::process::lifetime::detached(&cx);
        let originator = context
            .process_spawn_provenance()
            .expect("the tool runs inside a process")
            .originator
            .clone();
        let choice = match context.start_policy_cx(&resolved.target, None).restrict() {
            Ok(choice) => choice,
            Err(error) => return refuse(format!("{error:?}")),
        };
        let launch = match context.declare_inline_launch(&choice, &resolved).await {
            Ok(launch) => launch,
            Err(error) => return refuse(format!("{error:?}")),
        };
        let identity = context.intent_identity(0);
        let declaration = lash_execution_contracts::process::ProcessStartDeclaration::inline(
            resolved.target.args.clone(),
            launch,
            originator,
            lifetime,
        );
        let intents = lash_execution_contracts::tool_intent::ToolIntents::v3(vec![
            lash_execution_contracts::tool_intent::ToolIntent::StartProcess(Box::new(
                lash_execution_contracts::tool_intent::StartProcessIntent {
                    owner: context.owner().runtime_owner(),
                    declaration,
                },
            )),
        ]);
        let counted = shipped(&call.args["function"])
            .counted_bytes()
            .expect("the function value encodes");
        let declared = intents
            .declared_canonical_bytes()
            .expect("the intents encode");
        LAUNCH_SIZES
            .lock()
            .unwrap()
            .push((marker, counted, declared));
        lash::tools::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(lash_protocol_types::handle::process_start_slot_json(
                identity.intent_index,
            )),
            intents,
        )
    }
}

/// Each declaring attempt's marker, its function's counted bytes and its
/// intents' canonical bytes.
static LAUNCH_SIZES: std::sync::Mutex<Vec<(String, u64, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// The function value a tool was handed.
fn shipped(transported: &serde_json::Value) -> FunctionValue {
    serde_json::from_value(transported[lash::workflow::document::FUNCTION_VALUE_TAG].clone())
        .expect("a function value")
}

/// The identity of the document a transported function value carries.
fn function_document(transported: &serde_json::Value) -> String {
    shipped(transported).document.identity().to_string()
}

/// The component that contributes `tools.launch` to a workflow process.
const LAUNCH_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-launch", lash::plugins::FormatVersion::ONE);

/// `tools.launch`, whose call a recovering node runs again, and
/// `tools.launch_once`, whose call it settles as interrupted.
fn launch_definitions() -> Vec<lash::tools::ToolDefinition> {
    use lash::tools::ToolDefinitionBindingExt as _;
    let launch = |name: &str| {
        lash::tools::ToolDefinition::raw(
            format!("tool:{name}"),
            name,
            "Starts the function it is handed.",
            serde_json::json!({"type": "object"}),
            serde_json::json!({"x-lash": {"kind": "process_unknown"}}),
        )
        .expect("tool schema")
        .with_execution(std::time::Duration::from_secs(120))
        .with_declaration(
            lash_core::ToolDeclaration::default()
                .with_intents([lash_core::ToolIntentKind::StartProcess]),
            None,
        )
        .expect("a declaring tool")
    };
    vec![
        launch("launch")
            .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(4).unwrap(),
                1,
                1,
            ))
            .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "launch")),
        launch("launch_once")
            .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "launch_once")),
    ]
}

/// A node over `stores`, cut by `cuts`, whose workers cross functions, with
/// `tools.launch` installed: the node and the registry decorator a law arms
/// its registration cut on.
async fn cut_core(
    stores: &Arc<lash_sqlite_store::SqliteStoreSet>,
    cuts: &Arc<Cuts>,
    boot: &str,
) -> (lash::LashCore, lash_core::testing::ProcessRegistryFaults) {
    let mut registry = None;
    let layered = lash_core::testing::runtime_helpers::LayeredStores::over(
        Arc::clone(stores) as Arc<dyn lash_execution_contracts::backend::StoreSet>
    )
    .map_module_artifacts(|inner| {
        Arc::new(CutModules {
            inner,
            cuts: Arc::clone(cuts),
        })
    })
    .map_definition_store(|inner| {
        Arc::new(CutDefinitions {
            inner,
            cuts: Arc::clone(cuts),
        })
    })
    .map_process_registry(|inner| {
        let faults = lash_core::testing::ProcessRegistryFaults::new(inner);
        registry = Some(faults.clone());
        Arc::new(faults)
    })
    .into_store_set();
    let defaults = lash::vm::WorkerService::default();
    let config = defaults.config().clone();
    let launch = Arc::new(lash::tools::StaticToolProvider::new(
        launch_definitions(),
        Launch {
            cuts: Arc::clone(cuts),
        },
    ));
    let component = lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(LAUNCH_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_tool_provider(lash::tool_provider_key!("launch"), launch.clone()),
    )
    .as_component(LAUNCH_TOOLS.format())
    .expect("the launch tool is a process component");
    let core = builder_on_workers(
        layered,
        false,
        EchoComponent::Installed,
        lash::vm::WorkerService::new(config),
    )
    .await
    .tools(lash::tool_provider_key!("launch"), launch)
    .process_component(Arc::new(component), LAUNCH_TOOLS)
    .build(node_identity(boot))
    .expect("the core builds");
    (core, registry.expect("the registry is decorated"))
}

async fn sqlite() -> Arc<lash_sqlite_store::SqliteStoreSet> {
    Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    )
}

/// The launch an inline start runs under: an environment of no effects.
async fn empty_launch(core: &lash::LashCore) -> lash_execution_contracts::process::EngineLaunch {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &linking(&environment(false), &[]),
        )
        .await
        .expect("publish the empty environment");
    lash_execution_contracts::process::EngineLaunch::new(
        env_ref,
        core.processes()
            .engine_defaults(lash::process::LASH_VM_ENGINE_KIND, &serde_json::Value::Null)
            .expect("the kernel states its settings"),
    )
}

fn inline_request(
    value: &FunctionValue,
    n: i64,
    launch: &lash_execution_contracts::process::EngineLaunch,
    key: &str,
) -> lash_execution_contracts::process::ProcessStartRequest {
    let mut args = serde_json::Map::new();
    args.insert("n".to_owned(), serde_json::json!(n));
    lash_execution_contracts::process::ProcessStartRequest::inline(
        lash::process::LASH_VM_ENGINE_KIND,
        transport(value),
        args,
        launch.clone(),
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    )
    .with_host_start_key(key)
}

async fn host_start(
    core: &lash::LashCore,
    request: lash_execution_contracts::process::ProcessStartRequest,
) -> lash::Result<lash_core::ProcessId> {
    core.processes()
        .start(request, core.effect_host())
        .await
        .map(|receipt| receipt.process_id)
}

/// Every process the registry holds, whatever its status.
async fn every_process(
    stores: &Arc<lash_sqlite_store::SqliteStoreSet>,
) -> Vec<lash_execution_contracts::process::ProcessRecord> {
    use lash_execution_contracts::process::ProcessQuery as _;
    stores
        .process_registry()
        .list_processes_page(
            &lash::persistence::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
            std::num::NonZeroUsize::new(64).unwrap(),
            None,
        )
        .await
        .expect("the registry lists")
        .records
}

/// Settle every due cleanup: `Start(key)`'s staging is carried onto the
/// row it registered, and ends.
async fn settle_cleanup(core: &lash::LashCore) {
    lash::testing::quiesce_artifact_cleanup(core)
        .await
        .settle()
        .await
        .expect("the cleanup settles");
}

/// The one process holds its whole closure: with `Start(key)` ended and no
/// pin, a node of a build holding no library reads its document and the
/// declarations it lists, and its descriptor reads.
async fn holds_its_closure(
    core: &lash::LashCore,
    stores: &Arc<lash_sqlite_store::SqliteStoreSet>,
    process: &lash_core::ProcessId,
    value: &FunctionValue,
) {
    settle_cleanup(core).await;
    let reader = super::documents::retired_reader(Arc::clone(stores));
    let WorkflowRead::Inspected(inspection) = reader
        .processes()
        .graph(process)
        .await
        .expect("the reader reads the process")
    else {
        panic!("the process holds its document");
    };
    assert_eq!(inspection.document.document(), &value.document);
    assert_eq!(
        inspection
            .document
            .declarations()
            .keys()
            .collect::<Vec<_>>(),
        value.document.manifest.functions.keys().collect::<Vec<_>>(),
        "the process holds the declarations its document lists"
    );
    assert!(
        core.host_artifacts()
            .get_definition(&inspection.definition.id)
            .await
            .expect("the descriptor reads")
            .is_some(),
        "the process holds its descriptor"
    );
    reader.shutdown().await.expect("the reader stops");
}

/// START-ONCE on the host route: a start whose node died before the
/// document's write, after it, or after the descriptor's is retried by its
/// host under the same key and realizes once.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_inline_start_realizes_once_across_a_crash_at_each_realization_write() {
    let value = function(RUN);
    let document = value.document.identity().to_string();
    for (index, cut) in [
        Cut::BeforeDocument(document.clone()),
        Cut::AfterDocument(document.clone()),
        Cut::AfterDescriptor,
    ]
    .into_iter()
    .enumerate()
    {
        let stores = sqlite().await;
        let cuts = Arc::new(Cuts::default());
        let (core, _) = cut_core(&stores, &cuts, &format!("host-cut-{index}")).await;
        let launch = empty_launch(&core).await;
        let key = format!("host-inline-{index}");
        cuts.arm(cut.clone());
        tokio::select! {
            started = host_start(&core, inline_request(&value, 3, &launch, &key)) => {
                panic!("{cut:?}: the start died at its cut: {started:?}")
            }
            () = cuts.reached() => {}
        }
        assert_eq!(cuts.fired(), vec![cut.clone()], "the cut stopped the start");
        let process = host_start(&core, inline_request(&value, 3, &launch, &key))
            .await
            .expect("the retry starts the function");
        assert_eq!(
            every_process(&stores)
                .await
                .iter()
                .map(|record| record.id.clone())
                .collect::<Vec<_>>(),
            vec![process.clone()],
            "{cut:?}: the key realized one process"
        );
        assert_eq!(
            finished(&core, &process).await,
            serde_json::json!([3, "inline"])
        );
        holds_its_closure(&core, &stores, &process, &value).await;
        core.shutdown().await.expect("the node stops");
    }
}

/// START-ONCE's registration and fence cuts on the host route: a start
/// whose row committed and whose caller died before adopting it is
/// answered that row by its host's retry, which stages nothing; once its
/// key's staging has been carried onto the row and ended, a retry meets
/// that fence, adds nothing under it, and is still answered the row.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_inline_start_cut_after_registration_is_answered_its_row_before_and_after_the_fence()
{
    let value = function(RUN);
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, registry) = cut_core(&stores, &cuts, "host-registered").await;
    let launch = empty_launch(&core).await;
    let reached = Arc::new(tokio::sync::Notify::new());
    let notify = Arc::clone(&reached);
    registry.hold_next_registration(
        lash_core::testing::RegistrationHoldPoint::AfterRegistering,
        Arc::new(move || notify.notify_one()),
    );
    let request = inline_request(&value, 3, &launch, "host-registered");
    tokio::select! {
        started = host_start(&core, request.clone()) => {
            panic!("the start died after registering: {started:?}")
        }
        () = reached.notified() => {}
    }
    let registered = every_process(&stores).await;
    assert_eq!(
        registered.len(),
        1,
        "the row committed before the node died"
    );
    let process = host_start(&core, request.clone())
        .await
        .expect("the retry answers");
    assert_eq!(process, registered[0].id, "the retry is answered the row");
    settle_cleanup(&core).await;
    let published = cuts.published.lock().unwrap().len();
    let fenced = host_start(&core, request)
        .await
        .expect("the retry after the fence answers");
    assert_eq!(fenced, process, "the fenced retry is answered the row");
    assert_eq!(
        cuts.published.lock().unwrap().len(),
        published,
        "the fenced retry publishes nothing"
    );
    assert_eq!(every_process(&stores).await.len(), 1);
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!([3, "inline"])
    );
    holds_its_closure(&core, &stores, &process, &value).await;
    core.shutdown().await.expect("the node stops");
}

/// An exact retry under a host key is answered the same process; other
/// bytes, other arguments or another launch under it are a key conflict,
/// which publishes nothing and starts nothing; a function its engine
/// refuses starts nothing and publishes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_inline_start_retry_is_exact_and_a_conflicting_or_refused_one_starts_nothing() {
    let value = function(RUN);
    let other = function(OTHER);
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, _) = cut_core(&stores, &cuts, "host-conflict").await;
    let launch = empty_launch(&core).await;
    let process = host_start(&core, inline_request(&value, 3, &launch, "exact"))
        .await
        .expect("the function starts");
    let again = host_start(&core, inline_request(&value, 3, &launch, "exact"))
        .await
        .expect("the exact retry answers");
    assert_eq!(again, process, "an exact retry is the same start");

    let other_launch = empty_launch(&core).await;
    let other_launch = lash_execution_contracts::process::EngineLaunch::new(
        other_launch.env,
        Some(lash_execution_contracts::process::EngineSettings::new({
            let mut settings = launch
                .settings
                .clone()
                .expect("the kernel states settings")
                .into_value();
            settings["charge_limit"] = serde_json::json!(4_000_000);
            settings
        })),
    );
    for (what, request) in [
        ("other bytes", inline_request(&other, 3, &launch, "exact")),
        (
            "other arguments",
            inline_request(&value, 4, &launch, "exact"),
        ),
        (
            "another launch",
            inline_request(&value, 3, &other_launch, "exact"),
        ),
    ] {
        let refused = host_start(&core, request)
            .await
            .expect_err("a conflicting start under the key is refused");
        assert!(
            format!("{refused:?}").contains("StartKeyConflict"),
            "{what}: a key conflict, got {refused:?}"
        );
    }
    assert!(
        !cuts.published(&other.document.identity().to_string()),
        "a conflicting start publishes nothing"
    );

    let mut unstartable = function(RUN);
    unstartable.entry = Name::new("absent");
    let refused = host_start(&core, inline_request(&unstartable, 3, &launch, "refused"))
        .await
        .expect_err("a function with no such entry is refused");
    assert_eq!(
        refused.artifact_refusal(),
        Some(lash::process::HostArtifactRefusal::DefinitionRefused),
        "{refused:?}"
    );
    let ids: Vec<_> = every_process(&stores)
        .await
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(ids, vec![process.clone()], "only the first start started");
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!([3, "inline"])
    );
    core.shutdown().await.expect("the node stops");
}

/// `spawner(base, marker, cut)`: hands `tools.<launch>` a closure over
/// `base`, which the tool starts inline; answers the tool's handle.
fn spawner(launch: &str) -> lash::workflow::document::DocumentBytes {
    lash::workflow::document::DocumentBytes::from(
        parse_document(&format!(
            r#"kernel 1
numbers float
effect tools.{launch}(input: Any) -> Any
entry spawner(base: Any, marker: Any, cut: Any) -> Any

fn spawner(base, marker, cut) {{
  let add = fn(n) start(n: Any) -> Any bare direct {{
    return [base, n]
  }}
  let input = {{function: add, marker: marker, cut: cut}}
  let handle = perform tools.{launch}(input) as Any
  return handle
}}

main {{
  finish null
}}
"#
        ))
        .expect("the spawner parses"),
    )
}

/// The environment a spawner of `tools.<launch>` runs under: that tool
/// alone.
fn launching(launch: &str) -> lash::process::ProcessEnv {
    let mut builder = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(LAUNCH_TOOLS, &());
    for tool in launch_definitions()
        .iter()
        .filter(|tool| tool.manifest.name == launch)
    {
        builder = builder.link_tool(
            LAUNCH_TOOLS.tool_provider(lash::tool_provider_key!("launch")),
            tool,
        );
    }
    builder.build().expect("the environment builds")
}

/// START-ONCE on the tool route: a process hands a capturing closure to a
/// tool that declares its start inline. A crash after the crossing reached
/// the tool and before it declared publishes nothing, and the call's retry
/// starts the child; a node that died before or after the document's write
/// or after the descriptor's is recovered by a second node, which realizes
/// the same start onto the one child. The child runs the closure as it
/// crossed and holds its closure.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_inline_start_realizes_once_across_a_crash_at_each_realization_write() {
    let spawner = spawner("launch");
    for cut in [
        "uncut",
        "before-declaring",
        "before-document",
        "after-document",
        "after-descriptor",
    ] {
        let stores = sqlite().await;
        let cuts = Arc::new(Cuts::default());
        let (core, _) = cut_core(&stores, &cuts, &format!("tool-{cut}")).await;
        let artifacts = core.host_artifacts();
        let pin = lash::process::HostArtifactPin::mint();
        let environment = launching("launch");
        let publication = published(
            artifacts
                .publish_workflow(&pin, &spawner, &Name::new("spawner"), &environment, None)
                .await
                .expect("the spawner publishes"),
        );
        let env_ref = artifacts
            .publish_process_env(&pin, &environment)
            .await
            .expect("publish the spawner's environment");
        let marker = format!("tool-{cut}");
        let mut args = serde_json::Map::new();
        args.insert("base".to_owned(), serde_json::json!("b"));
        args.insert("marker".to_owned(), serde_json::json!(marker));
        args.insert("cut".to_owned(), serde_json::json!(cut));
        let parent = start_with(&core, &env_ref, &publication.definition, &marker, args).await;
        // A realization cut kills node A; node B recovers the call.
        let core = if matches!(
            cut,
            "before-document" | "after-document" | "after-descriptor"
        ) {
            cuts.reached().await;
            tokio::time::timeout(std::time::Duration::from_secs(120), core.shutdown())
                .await
                .expect("node A stops")
                .expect("node A stops cleanly");
            cut_core(&stores, &cuts, &format!("tool-{cut}-b")).await.0
        } else {
            core
        };
        let artifacts = core.host_artifacts();
        let handle = finished(&core, &parent).await;
        let child = lash_core::process_id_from_handle_json(&handle)
            .unwrap_or_else(|error| panic!("the parent answers its child's handle: {error}"));
        assert_eq!(
            finished(&core, &child).await,
            serde_json::json!(["b", 3]),
            "{cut}: the child runs the closure as it crossed"
        );
        let children: Vec<_> = every_process(&stores)
            .await
            .into_iter()
            .map(|record| record.id)
            .filter(|id| *id != parent)
            .collect();
        assert_eq!(children, vec![child.clone()], "{cut}: one child");
        assert_eq!(
            cuts.fired().len(),
            usize::from(cut != "uncut"),
            "{cut}: the cut stopped the start once"
        );
        if cut == "before-declaring" {
            assert_eq!(
                *cuts.crossed_unpublished.lock().unwrap(),
                vec![true],
                "the crossing published nothing"
            );
        }
        artifacts.release(pin).await.expect("the pin is released");
        let shipped = every_process(&stores)
            .await
            .into_iter()
            .find(|record| record.id == child)
            .expect("the child is registered");
        let lash_execution_contracts::process::ProcessInput::Engine { payload, .. } =
            shipped.input.as_ref()
        else {
            panic!("the child runs an engine input");
        };
        let shipped_document = payload["document"]
            .as_str()
            .expect("the child names its document")
            .to_owned();
        assert!(
            cuts.published(&shipped_document),
            "the child runs the document realization published"
        );
        settle_cleanup(&core).await;
        let reader = super::documents::retired_reader(Arc::clone(&stores));
        let WorkflowRead::Inspected(inspection) = reader
            .processes()
            .graph(&child)
            .await
            .expect("the reader reads the child")
        else {
            panic!("{cut}: the child holds its document");
        };
        assert_eq!(
            inspection.document.document().identity().to_string(),
            shipped_document
        );
        reader.shutdown().await.expect("the reader stops");
        core.shutdown().await.expect("the node stops");
    }
}

/// The other half of START-ONCE on the tool route: a call its node never
/// runs again is uncommitted, and so is the start it declared. A node that
/// died after the closure's document was published under `Start(key)` is
/// recovered by a second node, which settles the `Once` call interrupted:
/// no child is registered, and once the call's journal settled the
/// staging ends and nothing holds the document.
#[tokio::test(flavor = "multi_thread")]
async fn an_uncommitted_declared_inline_start_leaves_no_process_and_holds_nothing() {
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, _) = cut_core(&stores, &cuts, "tool-once").await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = launching("launch_once");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &spawner("launch_once"),
                &Name::new("spawner"),
                &environment,
                None,
            )
            .await
            .expect("the spawner publishes"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the spawner's environment");
    let mut args = serde_json::Map::new();
    args.insert("base".to_owned(), serde_json::json!("b"));
    args.insert("marker".to_owned(), serde_json::json!("tool-once"));
    args.insert("cut".to_owned(), serde_json::json!("after-document"));
    let parent = start_with(&core, &env_ref, &publication.definition, "tool-once", args).await;
    cuts.reached().await;
    let Cut::AfterDocument(document) = cuts.fired().remove(0) else {
        panic!("the document cut fired");
    };
    tokio::time::timeout(std::time::Duration::from_secs(120), core.shutdown())
        .await
        .expect("node A stops")
        .expect("node A stops cleanly");
    let (core, _) = cut_core(&stores, &cuts, "tool-once-b").await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&parent),
    )
    .await
    .expect("the spawner settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the spawner settles with an output: {output:?}");
    };
    assert!(
        !output.is_success(),
        "the interrupted call fails the spawner: {output:?}"
    );
    let ids: Vec<_> = every_process(&stores)
        .await
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(
        ids,
        vec![parent],
        "the uncommitted start registered nothing"
    );
    artifacts.release(pin).await.expect("the pin is released");
    // The staging's guard rechecks its starter after the relay's backoff;
    // the law makes that pass due now.
    use lash_execution_contracts::backend::StoreSet as _;
    let staged_under = cuts
        .staged_under
        .lock()
        .unwrap()
        .clone()
        .expect("the document was staged");
    assert!(
        matches!(staged_under, lash_core::ArtifactReferrer::Start(_)),
        "the document was staged under Start(key): {staged_under}"
    );
    stores
        .artifact_cleanup()
        .nudge(&staged_under, stores.clock().timestamp_ms())
        .await
        .expect("the staging's cleanup is due");
    settle_cleanup(&core).await;
    assert_eq!(
        stores
            .module_artifacts()
            .get_module_artifact(&document)
            .await
            .expect("the module store reads"),
        None,
        "nothing holds the uncommitted start's document"
    );
    core.shutdown().await.expect("the node stops");
}

/// The bounds of an inline start (FN-BOUND at the start): a function value
/// past the per-value bound is refused at realization, with nothing
/// published and no process. A value within that bound is still recorded in
/// its declaring call's intents, whose canonical bytes the batch bound
/// counts: a declaration that takes the batch past it is refused before
/// realization, publishing nothing and starting nothing. The ledger budget
/// is not raised to fit it.
#[tokio::test(flavor = "multi_thread")]
async fn an_inline_start_past_the_value_or_batch_bound_publishes_and_starts_nothing() {
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, _) = cut_core(&stores, &cuts, "bounds").await;
    let launch = empty_launch(&core).await;
    let ceiling = lash::workflow::document::FunctionValueLimits::CEILING;

    let large = function(&RUN.replace(
        "\"inline\"",
        &format!("\"{}\"", "x".repeat(usize::try_from(ceiling).unwrap())),
    ));
    assert!(large.counted_bytes().unwrap() > ceiling);
    let refused = host_start(&core, inline_request(&large, 3, &launch, "too-large"))
        .await
        .expect_err("a function past the bound is refused");
    assert_eq!(
        refused.artifact_refusal(),
        Some(lash::process::HostArtifactRefusal::DefinitionRefused),
        "{refused:?}"
    );
    assert!(
        format!("{refused:?}").contains("above the bound"),
        "{refused:?}"
    );
    assert!(!cuts.published(&large.document.identity().to_string()));
    assert!(every_process(&stores).await.is_empty(), "nothing started");

    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = launching("launch");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &spawner("launch"),
                &Name::new("spawner"),
                &environment,
                None,
            )
            .await
            .expect("the spawner publishes"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the spawner's environment");
    let mut args = serde_json::Map::new();
    let base = "y".repeat(usize::try_from(ceiling).unwrap() - 700);
    args.insert("base".to_owned(), serde_json::json!(base));
    args.insert("marker".to_owned(), serde_json::json!("batch"));
    args.insert("cut".to_owned(), serde_json::json!("uncut"));
    let parent = start_with(&core, &env_ref, &publication.definition, "batch", args).await;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(&parent),
    )
    .await
    .expect("the spawner settles")
    .expect("the output reads");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the spawner settles with an output: {output:?}");
    };
    let (_, counted, declared) = LAUNCH_SIZES
        .lock()
        .unwrap()
        .iter()
        .find(|(marker, _, _)| marker == "batch")
        .cloned()
        .expect("the tool declared the start");
    assert!(
        counted <= ceiling,
        "the value is within its own bound: {counted}"
    );
    assert!(
        declared
            > lash_execution_contracts::tool_intent::ToolIntentLimits::standard()
                .max_canonical_bytes(),
        "the declaration takes the batch past its bound: {declared}"
    );
    assert!(
        !output.is_success(),
        "the refused start fails the call: {output:?}"
    );
    assert!(
        format!("{output:?}").contains("canonical"),
        "the batch bound refused it: {output:?}"
    );
    let ids: Vec<_> = every_process(&stores)
        .await
        .into_iter()
        .map(|record| record.id)
        .collect();
    assert_eq!(ids, vec![parent], "the refused start started nothing");
    assert_eq!(
        cuts.published.lock().unwrap().len(),
        2,
        "only the spawner's document and descriptor were published: {:?}",
        cuts.published.lock().unwrap()
    );
    core.shutdown().await.expect("the node stops");
}
