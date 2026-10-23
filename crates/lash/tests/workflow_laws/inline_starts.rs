//! START-ONCE: inline callable starts retain their defining Link and frozen
//! captures exactly once. Cuts interrupt Link acquisition, descriptor publication,
//! registration and staging cleanup on the host and declaring-tool routes.

use std::sync::Mutex;

use lash::workflow::document::FunctionValue;
use lash_core::ReferrerClaim;
use lash_execution_contracts::backend::StoreSet as _;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_artifacts::ModuleArtifactHold;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;

use super::*;

/// Complete data-only callable definitions, published before they cross.
const RUN: &str =
    "function run(n: Json) -> Json\nkernel 1\nmetering statements\nbody {return [n, \"inline\"]}";
const OTHER: &str =
    "function run(n: Json) -> Json\nkernel 1\nmetering statements\nbody {return [n, \"other\"]}";

async fn function(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    text: &str,
) -> FunctionValue {
    let definition = lash_kernel_doc::parse_definition(text).unwrap();
    let document = function_document(definition.clone(), "run");
    let publication = publish(
        core,
        pin,
        &document.clone().into(),
        &Name::new("run"),
        &empty_environment(),
        None,
    )
    .await
    .unwrap();
    FunctionValue {
        written_signature: None,
        signature_origin: None,
        implicit_text_conversions: Vec::new(),
        callable: lash::package::CallableId {
            link: publication.start.link,
            function: definition.identity(),
        },
        representation: lash::workflow::document::FunctionRepresentation::Bare,
        creation: None,
        token: None,
        captures: Default::default(),
        signature: definition.signature.clone(),
        convention: lash::workflow::document::StartConvention::Direct,
        captions: lash::workflow::document::Captions::empty(document.identity()),
    }
}
fn empty_environment() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .build()
        .unwrap()
}

/// A write a cut stops a start at.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cut {
    MissingRead(String),
    CorruptRead(String),
    /// The Link acquisition is refused: the node died before it.
    BeforeLink(String),
    /// The Link acquisition committed; its caller gets no answer.
    AfterLink(String),
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
        let staged = matches!(claim.referrer(), lash_core::ArtifactReferrer::Start(_))
            && module_ref.starts_with("link:v1:");
        if staged && self.cuts.fires(&Cut::BeforeLink(module_ref.to_owned())) {
            return died().await;
        }
        self.inner
            .acquire_module_artifact(claim, module_ref)
            .await?;
        if staged {
            self.cuts
                .published
                .lock()
                .unwrap()
                .push(module_ref.to_owned());
            if self.cuts.fires(&Cut::AfterLink(module_ref.to_owned())) {
                *self.cuts.staged_under.lock().unwrap() = Some(claim.referrer());
                return died().await;
            }
        }
        Ok(())
    }

    async fn hold_module_artifacts(
        &self,
        claim: &ReferrerClaim,
        artifacts: &[ModuleArtifactHold<'_>],
    ) -> Result<(), ArtifactStoreError> {
        self.inner.hold_module_artifacts(claim, artifacts).await
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
        if self.cuts.fires(&Cut::MissingRead(module_ref.to_owned())) {
            return Ok(None);
        }
        if self.cuts.fires(&Cut::CorruptRead(module_ref.to_owned())) {
            return Ok(Some(vec![0]));
        }
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
                match context.retained_inline_request(serde_json::json!({"kind":"callable","value":{
                    "link":shipped(&call.args["function"]).callable.link,
                    "function":shipped(&call.args["function"]),
                    "origin":match context.callable_origin() { Ok(origin)=>origin,Err(error)=>return refuse(format!("{error:?}")) }
                }})) { Ok(request)=>request,Err(error)=>return refuse(format!("{error:?}")) },
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
        let document = callable_link(&call.args["function"]);
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
                        .push(Cut::BeforeLink(String::new()));
                    return lash::tools::ToolAttemptOutcome::host_failed(
                        lash_core::RuntimeEffectControllerError::from(
                            lash_execution_contracts::plugin_error::PluginError::attempt_fault(
                                "the node died before declaring",
                            ),
                        )
                        .retryable_uncommitted_derivation(),
                    );
                }
                Some("before-link") => self.cuts.arm(Cut::BeforeLink(document)),
                Some("after-link") => self.cuts.arm(Cut::AfterLink(document)),
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
        let policy = lash::process::env::policy::restrict_or(lash::process::env::policy::refuse(
            "the child requires its retained parent",
        ));
        let choice = match policy
            .choose(&context.start_policy_cx(&resolved.target, None))
            .await
        {
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
fn callable_link(transported: &serde_json::Value) -> String {
    format!("link:v1:{}", shipped(transported).callable.link)
}

/// The component that contributes `tools.launch` to a workflow process.
const LAUNCH_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-launch", lash::plugins::FormatVersion::ONE);

/// `tools.launch`, whose call a recovering node runs again, and
/// `tools.launch_once`, whose call it settles as interrupted.
fn launch_definitions() -> Vec<lash::tools::ToolDefinition> {
    let launch = |name: &str| {
        lash::tools::ToolDefinition::raw(
            format!("tool:{name}"),
            format!("tools.{name}"),
            "Starts the function it is handed.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "function": {"x-lash": {"kind": "function"}}
                }
            }),
            serde_json::json!({"x-lash": {"kind": "process_unknown"}}),
        )
        .expect("tool schema")
        // The contract the spawner's kernel text pins.
        .declared_contract(lash_vocabulary::operations::ContractId::new("1"))
        .with_execution(std::time::Duration::from_secs(120))
        .with_declaration(
            lash_vocabulary::tool_declaration::ToolDeclaration::default()
                .with_intents([lash_vocabulary::ToolIntentKind::StartProcess]),
            None,
        )
        .expect("a declaring tool")
    };
    vec![
        launch("launch").with_execution_policy(
            lash_vocabulary::tool_contract::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(4).unwrap(),
                1,
                1,
            ),
        ),
        launch("launch_once"),
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
            .with_tool_provider(lash::tool_provider_key!("launch"), launch.clone())
            .with_package_provider(
                lash::tool_provider_key!("launch"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    LAUNCH_TOOLS.component(),
                    lash::tool_provider_key!("launch"),
                    [lash::package::Segment::new("tools").unwrap()].into(),
                    launch.clone(),
                )),
            ),
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
            &empty_environment(),
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
        lash::vm::KernelInlineRequest::Callable {
            link: value.callable.link,
            function: Box::new(value.clone()),
            origin: lash::package::CallableOrigin {
                authored: [value.callable.link].into(),
                publications: Default::default(),
            },
        }
        .into_request(
            Default::default(),
            lash::package::PublicationContext {
                domain: lash::package::PublicationDomain("inline-host".into()),
                scope: lash::package::PublicationScope::Host,
                publisher: lash::package::PublisherInstance("inline-laws".into()),
                root: lash::package::Segment::new("inline").unwrap(),
            },
            lash::package::Attachment {
                packages: Default::default(),
                members: Default::default(),
                deferred: Default::default(),
            },
        )
        .unwrap(),
        args,
        launch.clone(),
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    )
    .with_host_start_key(key)
}

/// The structured package refusal survives the controller and host boundary.
fn admission_refusal(
    error: lash::EmbedError,
) -> lash_execution_contracts::process::linking::AdmissionRefusal {
    use lash_execution_contracts::PluginError;
    let lash::EmbedError::Plugin(PluginError::RuntimeEffectController(error)) = error else {
        panic!("an inline admission refusal")
    };
    let Some(lash_core::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
        panic!("a typed plugin cause")
    };
    let PluginError::Package(error) = serde_json::from_value(failure.payload).unwrap() else {
        panic!("a package cause")
    };
    let lash_execution_contracts::process::linking::EnginePortError::Refused(refusal) = *error
    else {
        panic!("an admission refusal")
    };
    refusal
}

async fn host_start(
    core: &lash::LashCore,
    request: lash_execution_contracts::process::ProcessStartRequest,
) -> lash::Result<lash_vocabulary::identity::ProcessId> {
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
    process: &lash_vocabulary::identity::ProcessId,
    value: &FunctionValue,
) {
    settle_cleanup(core).await;
    let documents = lash::vm::KernelDocuments::new(
        stores.module_artifacts(),
        stores.package_store(),
        std::sync::Arc::clone(lash::vm::WorkerLibrary::built().unwrap().helpers()),
    );
    let content = documents
        .load_linked(value.callable.link)
        .await
        .expect("the child retains its Link");
    let link = &content.links[&value.callable.link];
    let document = &content.documents[&link.document];
    let definition = &document.functions[&value.callable.function];
    lash_kernel_doc::validate_callable_signature(
        &value.signature,
        &value.convention,
        &definition.signature,
    )
    .expect("the retained code implements the callable signature");
    let record = every_process(stores)
        .await
        .into_iter()
        .find(|record| record.id == process)
        .unwrap();
    assert!(
        core.host_artifacts()
            .get_definition(record.identity.definition_id.as_ref().unwrap())
            .await
            .unwrap()
            .is_some(),
        "the child holds its descriptor"
    );
}

/// START-ONCE on the host route: a start whose node died before the
/// Link acquisition, after it, or after the descriptor's is retried by its
/// host under the same key and realizes once.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_inline_start_realizes_once_across_a_crash_at_each_realization_write() {
    // The Link is deterministic, so every independent store tests the same cut.
    let document = super::function_document(lash_kernel_doc::parse_definition(RUN).unwrap(), "run");
    let link = lash::package::Link {
        document: document.identity(),
        fills: Default::default(),
    };
    let document = format!("link:v1:{}", link.identity());
    for (index, cut) in [
        Cut::BeforeLink(document.clone()),
        Cut::AfterLink(document.clone()),
        Cut::AfterDescriptor,
    ]
    .into_iter()
    .enumerate()
    {
        let stores = sqlite().await;
        let cuts = Arc::new(Cuts::default());
        let (core, _) = cut_core(&stores, &cuts, &format!("host-cut-{index}")).await;
        let pin = lash::process::HostArtifactPin::mint();
        let value = function(&core, &pin, RUN).await;
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
        core.host_artifacts().release(pin).await.unwrap();
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
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, registry) = cut_core(&stores, &cuts, "host-registered").await;
    let pin = lash::process::HostArtifactPin::mint();
    let value = function(&core, &pin, RUN).await;
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
    core.host_artifacts().release(pin).await.unwrap();
    holds_its_closure(&core, &stores, &process, &value).await;
    core.shutdown().await.expect("the node stops");
}

/// An exact retry under a host key is answered the same process; other
/// bytes, other arguments or another launch under it are a key conflict,
/// which publishes nothing and starts nothing; a function its engine
/// refuses starts nothing and publishes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_inline_start_retry_is_exact_and_a_conflicting_or_refused_one_starts_nothing() {
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, _) = cut_core(&stores, &cuts, "host-conflict").await;
    let pin = lash::process::HostArtifactPin::mint();
    let value = function(&core, &pin, RUN).await;
    let other = function(&core, &pin, OTHER).await;
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
        !cuts.published(&format!("link:v1:{}", other.callable.link)),
        "a conflicting start publishes nothing"
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
    let tool = launch_definitions()
        .into_iter()
        .find(|tool| tool.manifest.tool() == format!("tools.{launch}"))
        .unwrap();
    let mut imports = Document::new(lash::workflow::document::NumberPolicy::Float, Vec::new());
    let (slot, import) = operation_import(lash::package::ImportSlot(0), &tool);
    imports.imports.insert(slot, import);
    let header = lash_kernel_doc::print_document(&imports)
        .lines()
        .filter(|line| line.starts_with("import "))
        .collect::<Vec<_>>()
        .join("\n");
    let (definition, locals) = lash_kernel_doc::parse_definition_with_locals(&format!(
        r#"function spawner(base: Json, marker: Text, cut: Text) -> Json
kernel 1
metering statements
{header}
body {{
  let add = fn(n) presentation(n: Json) -> Json bare direct {{ return [base, n] }}
  let input = {{function: add, marker: marker, cut: cut}}
  let handle = perform 0(input) as Any
  return handle
}}"#
    ))
    .unwrap();
    let mut document = super::function_document(definition, "spawner");
    document.functions.extend(
        locals
            .into_iter()
            .map(|definition| (definition.identity(), definition)),
    );
    document.into()
}
fn launching(_launch: &str) -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(LAUNCH_TOOLS, &())
        .build()
        .unwrap()
}

/// START-ONCE on the tool route: a process hands a capturing closure to a
/// tool that declares its start inline. A crash after the crossing reached
/// the tool and before it declared publishes nothing, and the call's retry
/// starts the child; a node that died before or after the Link acquisition
/// or after the descriptor's is recovered by a second node, which realizes
/// the same start onto the one child. The child runs the closure as it
/// crossed and holds its closure.
#[tokio::test(flavor = "multi_thread")]
async fn a_declared_inline_start_realizes_once_across_a_crash_at_each_realization_write() {
    let spawner = spawner("launch");
    for cut in [
        "uncut",
        "before-declaring",
        "before-link",
        "after-link",
        "after-descriptor",
    ] {
        let stores = sqlite().await;
        let cuts = Arc::new(Cuts::default());
        let (core, _) = cut_core(&stores, &cuts, &format!("tool-{cut}")).await;
        let pin = lash::process::HostArtifactPin::mint();
        let environment = launching("launch");
        let publication = publish(
            &core,
            &pin,
            &spawner,
            &Name::new("spawner"),
            &environment,
            None,
        )
        .await
        .expect("the spawner publishes");
        let env_ref = publication.env_ref.clone();
        let marker = format!("tool-{cut}");
        let mut args = serde_json::Map::new();
        args.insert("base".to_owned(), serde_json::json!("b"));
        args.insert("marker".to_owned(), serde_json::json!(marker));
        args.insert("cut".to_owned(), serde_json::json!(cut));
        let parent = start_with(&core, &env_ref, &publication.definition, &marker, args).await;
        // A realization cut kills node A; node B recovers the call.
        let core = if matches!(cut, "before-link" | "after-link" | "after-descriptor") {
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
        let code: lash::vm::KernelInlineRequest =
            serde_json::from_value(payload["code"].clone()).unwrap();
        let lash::vm::KernelInlineRequest::Callable { function, .. } = code else {
            panic!("a callable start")
        };
        assert_eq!(
            function.captures.get(&Name::new("base")),
            Some(&lash_kernel_doc::CaptureValue::Data(
                lash::workflow::document::Datum::Text("b".into())
            ))
        );
        assert!(
            cuts.published(&format!("link:v1:{}", function.callable.link)),
            "realization acquired the defining Link"
        );
        holds_its_closure(&core, &stores, &child, &function).await;
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
    let publication = publish(
        &core,
        &pin,
        &spawner("launch_once"),
        &Name::new("spawner"),
        &environment,
        None,
    )
    .await
    .expect("the spawner publishes");
    let env_ref = publication.env_ref.clone();
    let mut args = serde_json::Map::new();
    args.insert("base".to_owned(), serde_json::json!("b"));
    args.insert("marker".to_owned(), serde_json::json!("tool-once"));
    args.insert("cut".to_owned(), serde_json::json!("after-link"));
    let parent = start_with(&core, &env_ref, &publication.definition, "tool-once", args).await;
    cuts.reached().await;
    let Cut::AfterLink(document) = cuts.fired().remove(0) else {
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
    let connection = rusqlite::Connection::open_with_flags(
        stores.database_uri(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let held:i64=connection.query_row("SELECT COUNT(*) FROM artifact_referrer_edges WHERE namespace = 'kernel_document' AND referrer_kind = 'start'",[],|row|row.get(0)).unwrap();
    assert_eq!(
        held, 0,
        "the uncommitted child retains no staging edges after cleanup: {document}"
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

    let function_pin = lash::process::HostArtifactPin::mint();
    let mut large = function(&core, &function_pin, RUN).await;
    large.captures.insert(
        Name::new("base"),
        lash_kernel_doc::CaptureValue::Data(lash::workflow::document::Datum::Text(
            "x".repeat(usize::try_from(ceiling).unwrap()),
        )),
    );
    assert!(large.counted_bytes().unwrap() > ceiling);
    let refused = host_start(&core, inline_request(&large, 3, &launch, "too-large"))
        .await
        .expect_err("a function past the bound is refused");
    assert!(
        matches!(admission_refusal(refused),lash_execution_contracts::process::linking::AdmissionRefusal::CallableTooLarge {bytes,limit} if bytes > limit && limit == ceiling)
    );
    assert!(!cuts.published(&format!("link:v1:{}", large.callable.link)));
    assert!(every_process(&stores).await.is_empty(), "nothing started");

    let pin = lash::process::HostArtifactPin::mint();
    let environment = launching("launch");
    let publication = publish(
        &core,
        &pin,
        &spawner("launch"),
        &Name::new("spawner"),
        &environment,
        None,
    )
    .await
    .expect("the spawner publishes");
    let env_ref = publication.env_ref.clone();
    let mut args = serde_json::Map::new();
    // Keep this value below its own bound while its start declaration
    // exceeds the batch bound. The new caption sidecar consumes part of
    // the fixture's original margin, even when it has no entries.
    let captions_bytes =
        serde_json::to_vec(&large.captions).unwrap().len() + "\"captions\":".len() + 1; // the member separator
    let base = "y".repeat(usize::try_from(ceiling).unwrap() - 700 - captions_bytes);
    args.insert("base".to_owned(), serde_json::json!(base));
    args.insert("marker".to_owned(), serde_json::json!("batch"));
    args.insert("cut".to_owned(), serde_json::json!("uncut"));
    let published = cuts.published.lock().unwrap().len();
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
        published,
        "the refused batch publishes no child descriptor or acquisition: {:?}",
        cuts.published.lock().unwrap()
    );
    core.shutdown().await.expect("the node stops");
}

/// K-CALLABLE-SOURCE-COVERAGE: unavailable retained callable code stays typed at the facade.
#[tokio::test]
async fn a_callable_view_reports_missing_and_corrupt_retained_content() {
    let stores = sqlite().await;
    let cuts = Arc::new(Cuts::default());
    let (core, _) = cut_core(&stores, &cuts, "callable-view-unavailable").await;
    let pin = lash::process::HostArtifactPin::mint();
    let value = function(&core, &pin, RUN).await;
    let (_, environment) = namespace(&core, &pin, &empty_environment()).await;
    let origin = lash::package::CallableOrigin {
        authored: [value.callable.link].into(),
        publications: Default::default(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(&pin, &value, &environment, &origin, None)
        .await
        .unwrap();
    let document = function_document(lash_kernel_doc::parse_definition(RUN).unwrap(), "run")
        .identity()
        .to_string();
    for corrupt in [false, true] {
        cuts.arm(if corrupt {
            Cut::CorruptRead(document.clone())
        } else {
            Cut::MissingRead(document.clone())
        });
        let WorkflowRead::Unavailable(unavailable) = core
            .host_artifacts()
            .definition_graph(&definition.id)
            .await
            .unwrap()
        else {
            panic!("the unavailable retained code cannot be inspected");
        };
        match unavailable {
            lash::workflow::WorkflowUnavailable::Artifact { artifact } if !corrupt => {
                assert_eq!(artifact.artifact_ref, document)
            }
            lash::workflow::WorkflowUnavailable::Corrupt { artifact, .. } if corrupt => {
                assert_eq!(artifact.artifact_ref, document)
            }
            other => panic!("lost typed artifact refusal: {other:?}"),
        }
    }
    core.shutdown().await.unwrap();
}
