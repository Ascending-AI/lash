//! The load behavior evidence on a real Restate handler and SQL stores: a
//! replay after the store moves on returns what the first attempt recorded.

#![allow(deprecated, reason = "the pinned SDK retains the trait workflow API")]

use super::*;
use lash::plugins::{
    PluginExtensionContribution, PluginFactory, PluginRegistrar, PluginSessionContext,
    SessionPlugin,
};
use lash::restate::restate_sdk;
use lash::testing::wait_until;
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{RestateTestBackend, ServerConfig};
use restate_sdk::context::{ContextPromises, SharedWorkflowContext};
use restate_sdk::endpoint::Endpoint;
use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const SERVICE: &str = "LoadBehaviorReplayProbe";
const RESUME: &str = "resume";
const FINISH: &str = "finish";

#[allow(deprecated, reason = "the pinned SDK retains the trait workflow API")]
#[restate_sdk::workflow]
trait LoadBehaviorReplayProbe {
    async fn run() -> HandlerResult<Json<Evidence>>;
    #[shared]
    async fn release(promise: String) -> HandlerResult<String>;
}

/// What the production helpers answered on one pass through the handler.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Evidence {
    external: OccurrenceEvidence,
    promotion: behavior::PromotionEvidence,
    listed_revision: u64,
}

struct Services {
    core: lash::LashCore,
    authority: lash::restate::RestateAuthorityId,
}

/// Bind the production load dispatcher beside the evidence probe. The
/// witness pool is lazy: deleting a session never queries load metrics.
struct DeleteProbe {
    services: Arc<OnceLock<Services>>,
    connection: lash::restate::RestateConnection,
}

impl E2eLoadWorkflow for DeleteProbe {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        request: Json<LoadRequest>,
    ) -> HandlerResult<Json<LoadResponse>> {
        let services = self.services.get().expect("the core precedes the handler");
        let worker = LoadWorker::new(LoadWorkerConfig {
            worker_id: "load-behavior-replay".to_owned(),
            core: services.core.clone(),
            witness: sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://localhost/unused")
                .unwrap(),
            load: LoadContext::named("smoke-v1").unwrap(),
            restate_ingress_url: self.connection.ingress_url().to_owned(),
            restate_authority_id: services.authority.clone(),
            model: lash::LlmProfileConfig::new(lash::RecordedLlmProfile::mint(
                lash::LlmProfileKey::new("mock-model"),
                mock_llm_profile_metadata(),
            )),
            active: ActiveOperations::default(),
        });
        // Preserve the double's transport as well as the live connection.
        assert!(
            worker
                .administration
                .set(lash::restate::RestateSessionAdministration::new(
                    services.core.session_administration().await,
                    self.connection.clone(),
                    services.authority.clone(),
                    services.core.build_generation().clone(),
                ))
                .is_ok()
        );
        worker.run(ctx, request).await
    }
}

struct Probe {
    services: Arc<OnceLock<Services>>,
    /// Every pass's answers, in order: the first attempt's, then each
    /// replay's.
    passes: Arc<Mutex<Vec<Evidence>>>,
}

impl LoadBehaviorReplayProbe for Probe {
    async fn run(&self, context: WorkflowContext<'_>) -> HandlerResult<Json<Evidence>> {
        let services = self.services.get().expect("the core precedes the handler");
        let controller = Controller::new(
            context,
            services.authority.clone(),
            services.core.build_generation().clone(),
        );
        let run = controller.context().key().to_string();
        let session_id = session_of(&run);
        let core = &services.core;
        let external = external_event(&controller, core, &run, "event").await?;
        let promotion = promotion_evidence(&controller, core, &session_id, &external).await?;
        let listed_revision = listed_trigger_revision(&controller, core, &session_id).await?;
        let evidence = Evidence {
            external,
            promotion,
            listed_revision,
        };
        self.passes.lock().unwrap().push(evidence.clone());
        let ctx = controller.context();
        ctx.promise::<String>(RESUME).await?;
        ctx.promise::<String>(FINISH).await?;
        Ok(Json(evidence))
    }

    async fn release(
        &self,
        ctx: SharedWorkflowContext<'_>,
        promise: String,
    ) -> HandlerResult<String> {
        ctx.resolve_promise(&promise, "released".to_owned());
        Ok(promise)
    }
}

/// The one model the probe's core serves and its load worker records.
fn mock_llm_profile_metadata() -> lash::LlmProfileMetadata {
    lash::LlmProfileMetadata::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .unwrap()
}

fn session_of(run: &str) -> String {
    format!("load-{run}-behaviors")
}

/// The register phase's program without the witness tool: the target
/// answers its tick, so the occurrence starts one lashlang process.
fn register_script(run: &str) -> String {
    let schedule = serde_json::to_string(&format!("{run}/behaviors")).unwrap();
    format!(
        "<typescript>\nconst on_event=async(event:load.external.Event)=>{{return {{key:event.tick,revision:1}};}};const registered=await triggers.register({{source:load.external.event({{schedule:{schedule}}}),target:{{definition:on_event}},inputs:(event)=>({{event:event}}),subscription_key:\"load-external\",name:\"external\"}});finish({{revision:registered.revision}});\n</typescript>"
    )
}

/// The edit phase's program without the witness tool.
fn edit_script(run: &str) -> String {
    let schedule = serde_json::to_string(&format!("{run}/behaviors")).unwrap();
    format!(
        "<typescript>\nconst on_event=async(event:load.external.Event)=>{{return {{key:event.tick,revision:2}};}};const edited=await triggers.update({{subscription_key:\"load-external\",expected_revision:1,source:load.external.event({{schedule:{schedule}}}),target:{{definition:on_event}},inputs:(event)=>({{event:event}}),name:\"edited\"}});finish({{revision:edited.revision}});\n</typescript>"
    )
}

/// The production load surface: the external source's constructor and
/// event, and the load compaction hooks.
struct LoadSurfaceFactory;

impl PluginFactory for LoadSurfaceFactory {
    fn id(&self) -> &'static str {
        "load-behavior-replay"
    }

    fn extension_contributions(&self) -> Vec<PluginExtensionContribution> {
        let mut resources = lash::rlm::LashlangHostCatalog::new();
        behavior::register_source(&mut resources).unwrap();
        vec![
            PluginExtensionContribution::new(
                lash::rlm::LASHLANG_SURFACE_EXTENSION_ID,
                lash::rlm::LashlangSurfaceContribution::new(
                    lash::rlm::LashlangAbilities::default(),
                    lash::rlm::LashlangLanguageFeatures::default(),
                    resources,
                ),
            )
            .unwrap(),
        ]
    }

    fn build(
        &self,
        _ctx: &PluginSessionContext,
    ) -> Result<Arc<dyn SessionPlugin>, lash::plugins::PluginError> {
        Ok(Arc::new(LoadSurface))
    }
}

impl lash::plugins::PluginDefinition for LoadSurfaceFactory {
    fn declaration() -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial("load-behavior-replay")
    }
}

struct LoadSurface;

impl SessionPlugin for LoadSurface {
    fn id(&self) -> &'static str {
        "load-behavior-replay"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), lash::plugins::PluginError> {
        behavior::register(reg)
    }
}

fn services(
    backend: lash::Backend,
    authority: lash::restate::RestateAuthorityId,
    run: &str,
) -> Services {
    let responses = Arc::new(tokio::sync::Mutex::new(VecDeque::from([
        register_script(run),
        edit_script(run),
    ])));
    let provider = lash::testing::TestProvider::builder()
        .kind("load-behavior-replay")
        .complete(move |_| {
            let responses = Arc::clone(&responses);
            async move {
                let text = responses
                    .lock()
                    .await
                    .pop_front()
                    .expect("a queued program");
                Ok::<_, lash::provider::LlmTransportError>(lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        Arc::new(lash::rlm::TypescriptDialect),
        &backend,
    );
    let core = lash::LashCore::rlm_builder(backend, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .serve_test_llm_profile(provider, mock_llm_profile_metadata())
        .plugin(Arc::new(LoadSurfaceFactory))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "load-behavior-replay",
            "law",
        ))
        .unwrap();
    Services { core, authority }
}

/// One turn of `session` that must finish with `revision`.
async fn turn(core: &lash::LashCore, session: &str, phase: &str, revision: u64) {
    let output = core
        .session(lash::SessionId::fixture(session.to_string()))
        .open()
        .await
        .unwrap()
        .send(TurnInput::text(format!("load behavior {phase}")))
        .output()
        .await
        .unwrap();
    assert_eq!(
        output
            .final_value()
            .and_then(|value| value["revision"].as_u64()),
        Some(revision),
        "{phase}: {:?} {:?}",
        output.result.outcome,
        output.result.errors
    );
}

/// How the store moves on between the recorded attempt and its replay.
#[derive(Clone, Copy, Debug)]
enum Advance {
    /// The promotion process is pruned and its tombstone compacted.
    PrunePromotion,
    /// The behavior session is deleted.
    DeleteSession,
    /// The subscription advances to a new revision.
    EditSubscription,
}

async fn prune_promotion(core: &lash::LashCore, process: &str) {
    let id = process.parse::<lash::ProcessId>().unwrap();
    // The production retention pass: it releases the bound delivery's pin,
    // then prunes the retired process.
    let pruned = core
        .processes()
        .prune(
            u64::MAX / 2,
            None,
            lash::process::ProjectionWatermark::NoProjector,
        )
        .await
        .unwrap();
    assert!(pruned.pruned_processes >= 1, "{pruned:?}");
    assert!(
        matches!(
            core.process_registry().get_process(&id).await,
            Err(lash::plugins::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the promotion process is pruned"
    );
}

async fn advance(
    core: &lash::LashCore,
    ingress: &lash::restate::RestateIngressClient,
    run: &str,
    advance: Advance,
    recorded: &Evidence,
) {
    let session = session_of(run);
    match advance {
        Advance::PrunePromotion => {
            prune_promotion(core, &recorded.promotion.process_id).await;
        }
        Advance::DeleteSession => {
            let request = LoadRequest::DeleteSession {
                run: run.to_owned(),
                session_id: session.clone(),
            };
            let response = ingress
                .call_workflow_json::<_, LoadResponse>(
                    "E2eLoadWorkflow",
                    &request.workflow_key(),
                    "run",
                    &request,
                )
                .await
                .unwrap();
            let LoadResponse::DeleteSession(report) = response else {
                panic!("the workload delete must return its delete report: {response:?}");
            };
            assert_eq!(report.session_id, session);
            assert_eq!(report.deletion, DeletionOutcome::Deleted, "{report:?}");
            assert!(report.reopen_refusal.is_some(), "{report:?}");
            assert!(
                core.session(lash::SessionId::fixture(session.clone()))
                    .open()
                    .await
                    .is_err()
            );
            assert!(
                core.backend()
                    .trigger_store()
                    .list_subscriptions(lash::triggers::TriggerSubscriptionFilter::for_session(
                        lash::SessionId::fixture(session)
                    ))
                    .await
                    .unwrap()
                    .is_empty(),
                "the workload delete removes the behavior subscription before replay"
            );
        }
        Advance::EditSubscription => {
            turn(core, &session, "edit", 2).await;
        }
    }
}

/// The stored facts a repeated emission or an extra start would move.
#[derive(Debug, PartialEq)]
struct Starts {
    processes: BTreeSet<String>,
    occurrences: BTreeSet<String>,
    /// Each delivery with the process it is bound to.
    deliveries: BTreeSet<(String, Option<String>)>,
}

async fn starts(core: &lash::LashCore) -> Starts {
    let processes = core
        .process_registry()
        .list_processes(&lash::process::ProcessListFilter {
            status: lash::process::ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.id.to_string())
        .collect();
    let store = core.backend().trigger_store();
    let occurrences = store
        .list_occurrences(lash::triggers::TriggerOccurrenceFilter::default())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.occurrence_id)
        .collect();
    let deliveries = store
        .list_deliveries()
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.occurrence.occurrence_id,
                row.process_id.map(|id| id.to_string()),
            )
        })
        .collect();
    Starts {
        processes,
        occurrences,
        deliveries,
    }
}

/// No replay starts a process, emits an occurrence or writes a trigger row:
/// the registry and the trigger store hold exactly what they held after the
/// advance, so an occurrence and a delivery retention reclaimed stay gone
/// (FIG-4503), and every row left is the recorded one, bound to the recorded
/// start.
fn assert_no_new_starts(
    storage: &str,
    case: Advance,
    recorded: &Starts,
    advanced: &Starts,
    replayed: &Starts,
) {
    assert_eq!(
        replayed.processes, advanced.processes,
        "{storage} {case:?}: the replay started or revived a process"
    );
    assert!(
        advanced.occurrences.is_subset(&recorded.occurrences)
            && advanced.deliveries.is_subset(&recorded.deliveries)
            && advanced.deliveries.iter().all(|(_, bound)| bound.is_some()),
        "{storage} {case:?}: the advance kept only recorded rows: {advanced:?} over {recorded:?}"
    );
    if matches!(case, Advance::PrunePromotion) {
        assert!(
            advanced.occurrences.is_empty() && advanced.deliveries.is_empty(),
            "{storage} {case:?}: retention reclaimed the pruned process's occurrence and \
             delivery: {advanced:?}"
        );
    }
    assert_eq!(
        replayed.occurrences, advanced.occurrences,
        "{storage} {case:?}: the replay wrote an occurrence row"
    );
    assert_eq!(
        replayed.deliveries, advanced.deliveries,
        "{storage} {case:?}: the replay wrote a delivery row"
    );
}

fn assert_original(storage: &str, case: Advance, passes: &[Evidence]) -> Evidence {
    let original = passes.first().expect("the first attempt answered").clone();
    assert_eq!(original.listed_revision, 1, "{storage} {case:?}");
    assert_eq!(original.external.started.len(), 1, "{storage} {case:?}");
    assert_eq!(
        original.promotion.process_id, original.external.started[0],
        "{storage} {case:?}"
    );
    assert!(original.promotion.session_origin, "{storage} {case:?}");
    assert!(
        !original.promotion.record_definition.is_empty()
            && original.promotion.record_definition == original.promotion.artifact_definition,
        "{storage} {case:?}: {:?}",
        original.promotion
    );
    for pass in passes {
        assert_eq!(
            pass, &original,
            "{storage} {case:?}: a replay changed the evidence"
        );
    }
    original
}

async fn witness(double: RestateTestBackend<dyn lash::StoreSet>, storage: &str, case: Advance) {
    let run = format!("replay-{case:?}").to_lowercase();
    let cell = Arc::new(OnceLock::from(services(
        double.lash_backend(),
        double
            .restate()
            .restate_effect_host()
            .authority_id()
            .clone(),
        &run,
    )));
    let passes = Arc::new(Mutex::new(Vec::new()));
    double
        .server()
        .register(
            Endpoint::builder()
                .bind(
                    Probe {
                        services: cell.clone(),
                        passes: passes.clone(),
                    }
                    .serve(),
                )
                .bind(
                    DeleteProbe {
                        services: cell.clone(),
                        connection: double.connection(),
                    }
                    .serve(),
                )
                .build(),
        )
        .await
        .unwrap();
    let core = &cell.get().unwrap().core;
    double.install_process_worker(
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap(),
    );
    let session = session_of(&run);
    core.session(lash::SessionId::fixture(session.clone()))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            mock_llm_profile_metadata().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .unwrap();
    turn(core, &session, "register", 1).await;

    let target = format!("{SERVICE}/{run}/run");
    let ingress = double.ingress();
    let handler = tokio::spawn({
        let ingress = ingress.clone();
        let run = run.clone();
        async move {
            ingress
                .call_workflow_empty::<Evidence>(SERVICE, &run, "run")
                .await
        }
    });
    let server = double.server();
    let invocation = || {
        server
            .invocations()
            .into_iter()
            .find(|i| i.target == target)
    };
    let promises = |id: &str| {
        server
            .journal(id)
            .unwrap_or_default()
            .iter()
            .filter(|e| e.ty == lash_restate_test::protocol::MessageType::GetPromiseCommand)
            .count()
    };
    wait_until("the evidence is recorded and the handler parks", || {
        invocation().is_some_and(|i| promises(&i.id) == 1)
    })
    .await;
    let parked = invocation().unwrap();
    let recorded = server.journal(&parked.id).unwrap();
    let original = assert_original(storage, case, &passes.lock().unwrap());
    let emitted = starts(core).await;
    assert_eq!(
        emitted.processes.len(),
        1,
        "{storage} {case:?}: {emitted:?}"
    );
    advance(core, &ingress, &run, case, &original).await;
    let advanced = starts(core).await;
    // Kill even on always_replay: its suspended and streaming attempts must
    // both replay the evidence after the store moved on.
    if !server.crash(&parked.id) {
        assert!(
            parked.suspensions > 0,
            "the suspended handler replays when resumed"
        );
    }
    ingress
        .call_workflow_json::<_, String>(SERVICE, &run, "release", &RESUME)
        .await
        .unwrap();
    wait_until("the replay reaches its second promise", || {
        let current = invocation().unwrap();
        if let Some((code, message)) = &current.last_failure {
            assert!(
                *code != 570,
                "{storage} {case:?}: replay diverged [{code}]: {message}"
            );
        }
        promises(&current.id) == 2
    })
    .await;
    let replayed = server.journal(&parked.id).unwrap();
    assert_eq!(
        replayed.get(..recorded.len()),
        Some(&recorded[..]),
        "{storage} {case:?}: the replay keeps the original journal prefix"
    );
    ingress
        .call_workflow_json::<_, String>(SERVICE, &run, "release", &FINISH)
        .await
        .unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(60), handler)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answered, original, "{storage} {case:?}");
    let finished = invocation().unwrap();
    assert_eq!(finished.status, "completed");
    assert!(finished.attempts >= 2, "{finished:?}");
    let passes = passes.lock().unwrap().clone();
    assert!(
        passes.len() >= 2,
        "{storage} {case:?}: the handler replayed"
    );
    assert_original(storage, case, &passes);
    assert_no_new_starts(storage, case, &emitted, &advanced, &starts(core).await);
    eprintln!(
        "{storage} {case:?}: replay kept {} journal entries over {} passes",
        recorded.len(),
        passes.len()
    );
}

#[derive(Clone, Copy)]
enum Storage {
    Memory,
    File,
    Postgres,
}

async fn law(storage: Storage, replay: bool, case: Advance) {
    let config = ServerConfig::default().always_replay(replay);
    let root = tempfile::tempdir().unwrap();
    let seed = 4484;
    match storage {
        Storage::Memory => {
            witness(
                lash_restate_test::backend(seed, config)
                    .await
                    .unwrap()
                    .erase_store_type(),
                "sqlite-memory",
                case,
            )
            .await
        }
        Storage::File => {
            let double = lash_restate_test::backend_with_store_set(
                seed,
                config,
                Default::default(),
                |clock| async {
                    Ok(Arc::new(
                        lash::sqlite::SqliteStoreSet::open_with_clock(root.path(), clock)
                            .await
                            .unwrap(),
                    ) as Arc<dyn lash::StoreSet>)
                },
            )
            .await
            .unwrap();
            witness(double, "sqlite-file", case).await;
        }
        Storage::Postgres => {
            let url = std::env::var("LASH_POSTGRES_DATABASE_URL")
                .expect("the PostgreSQL gate supplies its URL");
            let database = lash::postgres::testing::IsolatedDatabase::create(&url).await;
            let storage = lash::postgres::PostgresStorage::connect(database.url())
                .await
                .unwrap();
            let double = lash_restate_test::backend_with_store_set(
                seed,
                config,
                Default::default(),
                |clock| async {
                    Ok(Arc::new(lash::postgres::PostgresStoreSet::with_clock(
                        &storage,
                        Arc::new(lash::persistence::FileAttachmentStore::new(root.path())),
                        Default::default(),
                        clock,
                    )) as Arc<dyn lash::StoreSet>)
                },
            )
            .await
            .unwrap();
            witness(double, "postgres", case).await;
        }
    }
}

macro_rules! case {
    ($module:ident, $storage:ident, $replay:expr, $case:ident $(, $ignore:meta)?) => {
        mod $module {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            $(#[$ignore])?
            async fn load_behavior_replay_after_store_advance_keeps_original_evidence() {
                law(Storage::$storage, $replay, Advance::$case).await;
            }
        }
    };
}

macro_rules! law {
    ($module:ident, $storage:ident, $replay:expr $(, $ignore:meta)?) => {
        mod $module {
            use super::*;
            case!(prune_promotion, $storage, $replay, PrunePromotion $(, $ignore)?);
            case!(delete_session, $storage, $replay, DeleteSession $(, $ignore)?);
            case!(edit_subscription, $storage, $replay, EditSubscription $(, $ignore)?);
        }
    };
}
law!(sqlite_memory, Memory, false);
law!(sqlite_memory_replay, Memory, true);
law!(sqlite_file, File, false);
law!(sqlite_file_replay, File, true);
law!(
    postgres,
    Postgres,
    false,
    ignore = "requires the PostgreSQL service gate"
);
law!(
    postgres_replay,
    Postgres,
    true,
    ignore = "requires the PostgreSQL service gate"
);

async fn live_witness(case: Advance) {
    let env = |name| std::env::var(name).unwrap();
    let run = format!(
        "{}-{}",
        format!("live-{case:?}").to_lowercase(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let cell = Arc::new(OnceLock::new());
    let passes = Arc::new(Mutex::new(Vec::new()));
    let backend = LiveRestateBackend::start_with_services(
        LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("LBR_BIND").parse().unwrap(),
            endpoint_url: env("LBR_URL"),
            run_tag: run.clone(),
            namespace: Default::default(),
        },
        {
            let cell = cell.clone();
            let passes = passes.clone();
            move |builder| {
                builder
                    .bind(
                        Probe {
                            services: cell.clone(),
                            passes,
                        }
                        .serve(),
                    )
                    .bind(
                        DeleteProbe {
                            services: cell,
                            connection: lash::restate::RestateConnection::new(env(
                                "RESTATE_INGRESS_URL",
                            )),
                        }
                        .serve(),
                    )
            }
        },
    )
    .await
    .unwrap();
    assert!(
        cell.set(services(
            backend.lash_backend(),
            lash::restate::RestateAuthorityId::new(format!("lash-live-{run}")).unwrap(),
            &run,
        ))
        .is_ok()
    );
    let core = &cell.get().unwrap().core;
    backend.install_process_worker(
        lash::durability::DurableProcessWorker::new(core.durable_process_worker_config().unwrap())
            .unwrap(),
    );
    let session = session_of(&run);
    core.session(lash::SessionId::fixture(session.clone()))
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            mock_llm_profile_metadata().wire_model,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
        )))
        .await
        .unwrap();
    turn(core, &session, "register", 1).await;
    let target = format!("{SERVICE}/{run}/run");
    let ingress = backend.ingress();
    // An ingress response proves the partition serves handlers before its
    // admin journal queries begin. The readiness key is outside the witness.
    ingress
        .call_workflow_json::<_, String>(SERVICE, &format!("ready-{run}"), "release", &"ready")
        .await
        .unwrap();
    let handler = tokio::spawn({
        let ingress = ingress.clone();
        let run = run.clone();
        async move {
            ingress
                .call_workflow_empty::<Evidence>(SERVICE, &run, "run")
                .await
        }
    });
    let promises = |journal: &[String]| {
        journal
            .iter()
            .filter(|e| e.contains("Command: GetPromise"))
            .count()
    };
    let recorded = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            if let Some(i) = backend
                .invocations()
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.target == target)
            {
                let journal = backend.journal(&i.id).await.unwrap();
                if promises(&journal) == 1 {
                    break journal;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let original = assert_original("live", case, &passes.lock().unwrap());
    let emitted = starts(core).await;
    assert_eq!(emitted.processes.len(), 1, "live {case:?}: {emitted:?}");
    advance(core, &ingress, &run, case, &original).await;
    let advanced = starts(core).await;
    backend.stop_serving(true);
    backend.start_serving().await.unwrap();
    ingress
        .call_workflow_json::<_, String>(SERVICE, &run, "release", &RESUME)
        .await
        .unwrap();
    let host = || async {
        backend
            .invocations()
            .await
            .unwrap()
            .into_iter()
            .find(|i| i.target == target)
            .unwrap()
    };
    let replayed = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = host().await;
            if let Some(failure) = &current.last_failure {
                assert!(
                    !failure.contains("Journal mismatch") && !failure.contains("RT0016"),
                    "live {case:?}: replay diverged: {failure}"
                );
            }
            let journal = backend.journal(&current.id).await.unwrap();
            if promises(&journal) == 2 {
                break journal;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        replayed.get(..recorded.len()),
        Some(&recorded[..]),
        "live {case:?}: the replay keeps the original journal prefix"
    );
    ingress
        .call_workflow_json::<_, String>(SERVICE, &run, "release", &FINISH)
        .await
        .unwrap();
    let answered = tokio::time::timeout(Duration::from_secs(60), handler)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(answered, original, "live {case:?}");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let current = host().await;
            assert!(
                current
                    .last_failure
                    .as_ref()
                    .is_none_or(|f| !f.contains("Journal mismatch") && !f.contains("RT0016")),
                "{current:?}"
            );
            if current.status == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let passes = passes.lock().unwrap().clone();
    assert!(passes.len() >= 2, "live {case:?}: the handler replayed");
    assert_original("live", case, &passes);
    assert_no_new_starts("live", case, &emitted, &advanced, &starts(core).await);
    eprintln!(
        "live {case:?}: replay kept {} journal entries over {} passes",
        recorded.len(),
        passes.len()
    );
    backend.finish().await;
}

macro_rules! live {
    ($module:ident, $case:ident) => {
        mod $module {
            use super::*;

            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            #[ignore = "requires the load-behavior-replay live Restate suite"]
            async fn load_behavior_replay_after_store_advance_keeps_original_evidence() {
                live_witness(Advance::$case).await;
            }
        }
    };
}

mod live_restate {
    use super::*;
    live!(prune_promotion, PrunePromotion);
    live!(delete_session, DeleteSession);
    live!(edit_subscription, EditSubscription);
}
