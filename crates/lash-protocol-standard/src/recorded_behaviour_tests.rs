//! FIG-4398 laws: a session runs under the standard-protocol behaviour it
//! recorded at creation — its discovery operation and its `batch` choice and
//! maximum — never under the configuration of the deployment that opens,
//! redrives or resumes it (ADR 0105 §1).
//!
//! The redrive law crashes a root after its config is resolved and before
//! its first model call on the deployment that created the session, then
//! redrives it on a deployment that withholds `batch`. It runs over SQLite
//! file, SQLite memory and PostgreSQL, each on the Restate server double
//! plain and always-replay.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::facade_support::{
    EmbeddedRuntimeHost, LashRuntime, PersistentRuntimeServices, PluginHost, RuntimeHostConfig,
};
use lash_core::plugin::{PluginSessionRequest, SessionAuthorityContext};
use lash_core::testing::TestTurnDrive as _;
use lash_core::{
    CommitBudget, LlmResponse, QueuedWorkBatchingConfig, SessionCreationHead, SessionPolicy,
    SessionRelation, SessionStoreCreateRequest, TurnBudget, TurnInput,
};
use lash_sansio::llm::types::LlmRequest;
use lash_sansio::{SessionId, TurnId};

use super::*;

/// The `batch` maximum the creating deployment offers.
const RECORDED_MAX_MEMBERS: usize = 3;

/// What the model answers when its prompt offers `batch` at the recorded
/// maximum.
const RECORDED_ANSWER: &str = "batch offered at the recorded maximum";

/// What the model answers otherwise: a session that lost its recorded batch
/// choice ends here.
const LOST_ANSWER: &str = "batch withheld or offered at another maximum";

/// The deployment that creates the session: `batch` offered, at most
/// [`RECORDED_MAX_MEMBERS`] per call.
fn creating_config() -> StandardProtocolConfig {
    StandardProtocolConfig::default().batch(BatchSugar::Enabled {
        max_members: std::num::NonZeroUsize::new(RECORDED_MAX_MEMBERS).expect("nonzero maximum"),
    })
}

/// The deployment that redrives it: `batch` withheld.
fn redeploying_config() -> StandardProtocolConfig {
    StandardProtocolConfig::default().batch(BatchSugar::Disabled)
}

fn policy() -> SessionPolicy {
    SessionPolicy {
        model: Some(lash_core::testing::test_llm_profile_config(
            "standard-recorded-behaviour-model",
            lash_core::testing::test_llm_profile_metadata("standard-recorded-behaviour-model"),
        )),
        ..SessionPolicy::new(TurnBudget::Unbounded, lash_core::MaxToolCalls::new(1024))
    }
}

/// The model: it says whether its prompt offered `batch` at the recorded
/// maximum, and records every request it served.
#[derive(Default)]
struct Model {
    calls: AtomicUsize,
    requests: Mutex<Vec<LlmRequest>>,
}

impl Model {
    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("standard-recorded-behaviour-law")
            .complete(move |request| {
                let model = Arc::clone(&model);
                async move {
                    model.calls.fetch_add(1, Ordering::SeqCst);
                    let offered = serde_json::to_string(&request)
                        .expect("request JSON")
                        .contains(&format!("at most {RECORDED_MAX_MEMBERS} per batch"));
                    model.requests.lock().expect("requests").push(request);
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: if offered {
                                RECORDED_ANSWER
                            } else {
                                LOST_ANSWER
                            }
                            .to_string(),
                            response_meta: None,
                        }],
                        ..Default::default()
                    })
                }
            })
            .build()
            .into_handle()
    }
}

fn host(config: StandardProtocolConfig) -> PluginHost {
    PluginHost::new(vec![Arc::new(StandardProtocolPluginFactory::with_config(
        config,
    ))])
}

/// Open `store`'s session on a deployment whose standard factory states
/// `config`, as a worker that reloads it from its stores does.
async fn open_runtime(
    backend: &lash_core::Backend,
    store: lash_core::store::SessionStore,
    config: StandardProtocolConfig,
    model: &Arc<Model>,
) -> LashRuntime {
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .expect("load the session")
    .expect("the session's head")
    .state;
    let host = host(config);
    let authority = SessionAuthorityContext {
        plugin_config: state.admitted_plugin_config(),
        ..Default::default()
    };
    let plugins = match state.plugin_state() {
        Some(snapshot) => host.build_session(PluginSessionRequest::rematerialization(
            &state.session_id,
            snapshot,
            authority,
        )),
        None => host.build_session(PluginSessionRequest::creation(&state.session_id, authority)),
    }
    .expect("build the standard plugin");
    let mut host_config = RuntimeHostConfig::new(
        backend.clone(),
        CommitBudget::bounded(8 * 1024 * 1024, 1024),
        QueuedWorkBatchingConfig::new(1),
    );
    host_config.providers.models =
        lash_core::testing::llm_profiles_serving(&policy(), model.provider());
    let runtime_host = EmbeddedRuntimeHost::new(host_config);
    let services = PersistentRuntimeServices::new(
        plugins,
        store,
        Arc::clone(&runtime_host.core.durability.attachment_store),
        Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    LashRuntime::from_persistent_embedded_state(
        policy(),
        runtime_host,
        services,
        state,
        lash_core::testing::runtime_lease_owner(),
    )
    .await
    .expect("open the runtime")
}

/// Crashes a root after its config is recorded and before its first model
/// call.
struct CrashBeforeFirstModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeFirstModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the root's config record and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A root interrupted after its config is resolved and redriven on a
/// deployment that withholds `batch` runs under the batch choice and maximum
/// its session recorded.
async fn a_redriven_root_runs_under_its_recorded_behaviour(
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    session: &str,
) {
    let backend = double.lash_backend();
    let session_id = SessionId::fixture(session);
    let mut config: lash_core::PersistedSessionConfig = policy().into();
    config.plugin_config = host(creating_config())
        .resolve_creation_plugin_config(
            Some(STANDARD_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            None,
            true,
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creating deployment records the standard namespace");
    let store = lash_core::runtime::admit_session_view(
        &backend.session_store_factory(),
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: SessionRelation::Root,
            config,
            head: SessionCreationHead::Config,
        },
    )
    .await
    .expect("create the session");
    let model = Arc::new(Model::default());
    let root = TurnId::fixture(format!("{session}-root"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let store = store.clone();
        let model = Arc::clone(&model);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let store = store.clone();
            let model = Arc::clone(&model);
            Box::pin(async move {
                let mut runtime = open_runtime(&backend, store, creating_config(), &model).await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .drive_turn(
                        TurnInput::text("batch it"),
                        lash_core::facade_support::TurnOptions::new(
                            tokio_util::sync::CancellationToken::new(),
                            scoped,
                        ),
                    )
                    .await;
                panic!("the crash fires before the root's first model call");
            })
        })
    };
    let redrive: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let store = store.clone();
        let model = Arc::clone(&model);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let store = store.clone();
            let model = Arc::clone(&model);
            let turn_tx = turn_tx.clone();
            Box::pin(async move {
                let mut runtime = open_runtime(&backend, store, redeploying_config(), &model).await;
                let turn = runtime
                    .drive_turn(
                        TurnInput::text("batch it"),
                        lash_core::facade_support::TurnOptions::new(
                            tokio_util::sync::CancellationToken::new(),
                            scoped,
                        ),
                    )
                    .await;
                let _ = turn_tx.send(turn);
            })
        })
    };
    double
        .run_crashed_then_redriven(
            lash_core::AdmittedScope::turn(&session_id, root),
            crashing,
            redrive,
        )
        .await
        .expect("the root crashes once and is redriven");
    let turn = turn_rx
        .recv()
        .await
        .expect("the redrive ran the root")
        .unwrap_or_else(|error| panic!("the redriven root runs: {error:?}"));
    assert_eq!(
        turn.outcome,
        TurnOutcome::Finished(TurnFinish::AssistantMessage {
            text: RECORDED_ANSWER.to_string(),
        }),
        "the redriven root's prompt offers batch at the recorded maximum"
    );
    assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    let requests = model.requests.lock().expect("requests");
    let request = requests.first().expect("the redriven root's request");
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == batch::BATCH_TOOL_NAME),
        "the redriven root's request carries the recorded batch sugar"
    );
}

/// A run nonce, so sessions on a shared PostgreSQL database never collide.
fn nonce() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    (nanos & u128::from(u64::MAX)) as u64
}

async fn on_sqlite_file(
    config: lash_restate_test::ServerConfig,
    dir: &tempfile::TempDir,
) -> lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet> {
    let path = dir.path().to_path_buf();
    lash_restate_test::backend_with_store_set(
        0x4398_0011,
        config,
        lash_restate_test::DeploymentHooks::default(),
        move |_clock| async move {
            let stores = lash_sqlite_store::SqliteStoreSet::open(&path)
                .await
                .map_err(|error| lash_restate_test::BackendError::Stores(error.to_string()))?;
            Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("the Restate double over SQLite file stores")
}

async fn on_sqlite_memory(
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet> {
    lash_restate_test::backend_with_store_set(
        0x4398_0012,
        config,
        lash_restate_test::DeploymentHooks::default(),
        move |_clock| async move {
            let stores = lash_sqlite_store::SqliteStoreSet::memory()
                .await
                .map_err(|error| lash_restate_test::BackendError::Stores(error.to_string()))?;
            Ok(Arc::new(stores) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("the Restate double over SQLite memory stores")
}

/// The Restate double over the required PostgreSQL service, with its resources.
#[allow(clippy::disallowed_methods)] // FIG-2971: a test is a host; the gate's database URL is host configuration.
async fn on_postgres(
    config: lash_restate_test::ServerConfig,
) -> Option<(
    lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    tempfile::TempDir,
)> {
    let url = lash_postgres_store::testing::required_database_url();
    let attachments = tempfile::tempdir().expect("attachment byte store");
    let bytes = Arc::new(lash_core::facade_support::FileAttachmentStore::new(
        attachments.path(),
    ));
    let double = lash_restate_test::backend_with_store_set(
        nonce(),
        config,
        lash_restate_test::DeploymentHooks::default(),
        move |clock| async move {
            let storage = lash_postgres_store::PostgresStorage::connect(&url)
                .await
                .map_err(|error| lash_restate_test::BackendError::Stores(error.to_string()))?;
            Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                &storage,
                bytes,
                lash_core::WakeDeliveryConfig::default(),
                clock,
            )) as Arc<dyn lash_core::StoreSet>)
        },
    )
    .await
    .expect("the Restate double over PostgreSQL");
    Some((double, attachments))
}

fn plain() -> lash_restate_test::ServerConfig {
    lash_restate_test::ServerConfig::default()
}

fn always_replay() -> lash_restate_test::ServerConfig {
    lash_restate_test::ServerConfig::default().always_replay(true)
}

#[tokio::test]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_file() {
    let dir = tempfile::tempdir().expect("SQLite directory");
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_file(plain(), &dir).await,
        "standard-recorded-behaviour-sqlite-file",
    )
    .await;
}

#[tokio::test]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_file_always_replay() {
    let dir = tempfile::tempdir().expect("SQLite directory");
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_file(always_replay(), &dir).await,
        "standard-recorded-behaviour-sqlite-file-replay",
    )
    .await;
}

#[tokio::test]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_memory() {
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_memory(plain()).await,
        "standard-recorded-behaviour-sqlite-memory",
    )
    .await;
}

#[tokio::test]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_memory_always_replay() {
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_memory(always_replay()).await,
        "standard-recorded-behaviour-sqlite-memory-replay",
    )
    .await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_postgres() {
    let Some((double, _attachments)) = on_postgres(plain()).await else {
        return;
    };
    a_redriven_root_runs_under_its_recorded_behaviour(
        double,
        &format!("standard-recorded-behaviour-pg-{}", nonce()),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_postgres_always_replay() {
    let Some((double, _attachments)) = on_postgres(always_replay()).await else {
        return;
    };
    a_redriven_root_runs_under_its_recorded_behaviour(
        double,
        &format!("standard-recorded-behaviour-pg-replay-{}", nonce()),
    )
    .await;
}

/// A deployment whose configuration differs from [`StandardProtocolConfig`]'s
/// default in every recorded choice.
fn configured_otherwise() -> StandardProtocolConfig {
    let mut config = creating_config();
    config.render.defaults.max_lines = Some(7);
    config
}

/// A child session records its parent's behaviour, whatever the host that
/// creates it is configured with (FIG-4527).
#[test]
fn a_child_session_records_its_parents_behaviour() {
    let parent = StandardConfigOwner {
        behaviour: configured_otherwise().recorded_behaviour(),
    }
    .create(
        None,
        CreationFacts {
            parent: None,
            is_root_session: true,
        },
    )
    .expect("create the parent")
    .expect("the parent records its namespace");
    let creating_host = StandardConfigOwner {
        behaviour: StandardProtocolConfig::default().recorded_behaviour(),
    };
    let child = creating_host
        .create(
            None,
            CreationFacts {
                parent: Some(&parent),
                is_root_session: false,
            },
        )
        .expect("create the child")
        .expect("the child records its namespace");
    assert_eq!(child.behaviour, configured_otherwise().recorded_behaviour());
    assert_ne!(child.behaviour, creating_host.behaviour);
}

/// The render a deployment configures is recorded behaviour: a session's
/// driver resolves a root's render over the recorded one, never over the
/// opening deployment's (FIG-4527).
#[test]
fn the_configured_render_is_recorded_behaviour() {
    let recorded = configured_otherwise().recorded_behaviour();
    assert_eq!(recorded.render, configured_otherwise().render);
    let opened = StandardProtocolConfig::default().under_recorded_behaviour(&recorded);
    assert_eq!(opened.render, configured_otherwise().render);
    let driver = StandardProtocolDriver { config: opened };
    let recording = StandardProtocolDriver {
        config: configured_otherwise(),
    };
    let options = lash_core::ProtocolTurnOptions::default();
    assert_eq!(
        driver.resolve_render(&options).expect("resolve the render"),
        recording
            .resolve_render(&options)
            .expect("resolve the render"),
    );
    assert_ne!(
        driver.resolve_render(&options).expect("resolve the render"),
        StandardProtocolDriver {
            config: StandardProtocolConfig::default(),
        }
        .resolve_render(&options)
        .expect("resolve the render"),
    );
}

/// A recorded namespace under `render` options, with the built-in prompt and
/// the default behaviour.
fn recorded_under(render: Option<StandardRenderConfig>) -> StandardRecordedConfig {
    StandardRecordedConfig {
        prompt: StandardPrompt::default(),
        render,
        behaviour: StandardProtocolConfig::default().recorded_behaviour(),
    }
}

fn defaults(max_lines: Option<usize>, head_share_percent: Option<u8>) -> StandardRenderConfig {
    StandardRenderConfig {
        defaults: render::ToolRenderPatch {
            max_lines,
            head_share_percent,
            ..render::ToolRenderPatch::default()
        },
        ..StandardRenderConfig::default()
    }
}

fn standard_owner() -> StandardConfigOwner {
    StandardConfigOwner {
        behaviour: StandardProtocolConfig::default().recorded_behaviour(),
    }
}

/// FIG-4652: the owner lays a run's render options over the session's,
/// field by field and tool by tool, and nothing else of the namespace moves.
#[test]
fn run_options_apply_over_the_recorded_render_field_by_field() {
    let tool = lash_core::ToolId::new("tool:a");
    let mut session = defaults(Some(10), Some(40));
    session.per_tool.insert(
        tool.clone(),
        render::ToolRenderPatch {
            max_lines: Some(3),
            ..render::ToolRenderPatch::default()
        },
    );
    let recorded = recorded_under(Some(session));
    let mut stated = defaults(None, Some(60));
    stated.per_tool.insert(
        tool.clone(),
        render::ToolRenderPatch {
            head_share_percent: Some(25),
            ..render::ToolRenderPatch::default()
        },
    );
    let applied = standard_owner()
        .apply_run_options(
            &recorded,
            StandardRunOptions {
                render: Some(stated),
            },
        )
        .expect("the run's render options apply");
    let render = applied.render.clone().expect("render options");
    assert_eq!(render.defaults.max_lines, Some(10), "the session's stays");
    assert_eq!(
        render.defaults.head_share_percent,
        Some(60),
        "the run's wins"
    );
    assert_eq!(
        (
            render.per_tool[&tool].max_lines,
            render.per_tool[&tool].head_share_percent
        ),
        (Some(3), Some(25))
    );
    assert_eq!(
        applied,
        StandardRecordedConfig {
            render: applied.render.clone(),
            ..recorded.clone()
        },
        "the prompt and the behaviour stay as recorded"
    );
    assert_eq!(
        standard_owner()
            .apply_run_options(&recorded, StandardRunOptions::default())
            .expect("empty options apply"),
        recorded
    );
}

/// FIG-4652: a run's options have no field for the prompt or the behaviour,
/// so a payload naming one does not decode, even when it restates the
/// recorded value. The list is the recorded namespace's own keys.
#[test]
fn run_options_have_no_field_for_the_prompt_or_the_behaviour() {
    let recorded = serde_json::to_value(recorded_under(Some(defaults(Some(10), None))))
        .expect("the recorded namespace encodes");
    let stated: Vec<&str> = recorded
        .as_object()
        .expect("the namespace is an object")
        .iter()
        .filter(|(key, value)| {
            serde_json::from_value::<StandardRunOptions>(serde_json::json!({ *key: value })).is_ok()
        })
        .map(|(key, _)| key.as_str())
        .collect();
    assert_eq!(stated, ["render"], "a run restates only its render");
}

/// FIG-4652: the driver reads the root's namespace as the recorded type. A
/// render it refuses is a typed refusal in the protocol's own type, and a
/// namespace it cannot read is corruption, not a refused shape.
#[test]
fn a_refused_render_is_typed_and_an_unreadable_namespace_is_corruption() {
    let driver = StandardProtocolDriver {
        config: StandardProtocolConfig::default(),
    };
    let namespace = |recorded: &StandardRecordedConfig| {
        lash_core::ProtocolTurnOptions::typed(recorded).expect("the namespace encodes")
    };
    driver
        .resolve_render(&namespace(&recorded_under(Some(defaults(
            Some(10),
            Some(50),
        )))))
        .expect("a head share within range resolves")
        .expect("the standard protocol records a render");

    let refused = driver
        .resolve_render(&namespace(&recorded_under(Some(defaults(None, Some(101))))))
        .expect_err("a head share over 100 is refused");
    let lash_core::RenderFault::Refused(refusal) = refused else {
        panic!("the render is refused: {refused:?}");
    };
    assert_eq!(
        serde_json::from_value::<StandardRenderRefusal>(refusal.refusal.clone())
            .expect("the protocol's own refusal type"),
        StandardRenderRefusal::HeadShareOutOfRange {
            head_share_percent: 101
        }
    );
    assert_eq!(
        refusal.message,
        StandardRenderRefusal::HeadShareOutOfRange {
            head_share_percent: 101
        }
        .to_string()
    );

    let corrupt = driver
        .resolve_render(&lash_core::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "render": { "defaults": {} } }),
        ))
        .expect_err("a namespace without its prompt and behaviour is unreadable");
    assert!(
        matches!(
            &corrupt,
            lash_core::RenderFault::RecordedCorrupt(error)
                if error.owner == STANDARD_PROTOCOL_PLUGIN_ID
        ),
        "{corrupt:?}"
    );
}
