//! FIG-4398 laws: a session runs under the RLM behaviour it recorded at
//! creation — its execution bounds, Lashlang features, prompt features,
//! output limit and soft-warning threshold — never under the configuration
//! of the deployment that opens, redrives or resumes it (ADR 0105 §1).
//!
//! The redrive law crashes a root after its config is resolved and before
//! its first model call on the deployment that created the session, then
//! redrives it on a deployment whose RLM factory states other bounds and
//! features. It runs over SQLite file, SQLite memory and PostgreSQL, each on
//! the Restate server double plain and always-replay; this target's
//! synthetic-next variant runs the same registrations.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::facade_support::{
    EmbeddedRuntimeHost, LashRuntime, PersistentRuntimeServices, PluginHost, RuntimeHostConfig,
};
use lash_core::plugin::{PluginFactory, PluginSessionRequest, SessionAuthorityContext};
use lash_core::testing::TestTurnDrive as _;
use lash_core::{
    CommitBudget, LlmOutputPart, LlmResponse, QueuedWorkBatchingConfig, SessionCreationHead,
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, TurnBudget, TurnInput,
};
use lash_sansio::llm::types::LlmRequest;
use lash_sansio::{SessionId, TurnId};

use crate::plugin::{InstructionBound, MemoryBound, RlmProtocolPluginConfig};
use crate::{RLM_PROTOCOL_PLUGIN_ID, RlmChannel, RlmProtocolPluginFactory};

/// The loop the model's cell runs: about 5,000 iterations, far past the
/// redeployed bound and far inside the recorded one.
pub(super) const LOOP_ITERATIONS: usize = 5_000;

/// What the model answers when its prompt offers `continue_as`.
pub(super) fn looping_cell() -> String {
    format!(
        "<typescript>\nlet i = 0;\nwhile (i < {LOOP_ITERATIONS}) {{ i = i + 1; }}\nfinish(\"ran \" + String(i));\n</typescript>"
    )
}

/// What the model answers when its prompt does not offer `continue_as`: a
/// session that lost its recorded prompt features ends here.
pub(super) const LOST_FEATURES_ANSWER: &str =
    "the prompt carried the redeploying factory's features";

/// The deployment that creates the session: a generous instruction bound,
/// decomposition offered, label annotations on.
pub(super) fn creating_config() -> RlmProtocolPluginConfig {
    RlmProtocolPluginConfig::builder()
        .channel(RlmChannel::Cell)
        .instruction_limit(InstructionBound::instructions(1_000_000))
        .memory_limit(MemoryBound::mebibytes(64))
        .build()
}

/// The deployment that redrives it: an instruction bound the loop exhausts,
/// decomposition withheld, label annotations off, a smaller output limit and
/// no soft warning.
pub(super) fn redeploying_config() -> RlmProtocolPluginConfig {
    let mut config = RlmProtocolPluginConfig::builder()
        .channel(RlmChannel::Cell)
        .instruction_limit(InstructionBound::instructions(50))
        .memory_limit(MemoryBound::mebibytes(1))
        .build();
    config.prompt_features.decomposition = false;
    config.lashlang_language_features.label_annotations = false;
    config.max_output_chars = 100;
    config.continue_as_soft_warn_tokens = None;
    config
}

pub(super) fn policy() -> SessionPolicy {
    SessionPolicy {
        model: Some(lash_core::testing::test_llm_profile_config(
            "rlm-recorded-behaviour-model",
            lash_core::testing::test_llm_profile_metadata("rlm-recorded-behaviour-model"),
        )),
        ..SessionPolicy::new(TurnBudget::Unbounded, lash_core::MaxToolCalls::new(1024))
    }
}

/// The model: it answers with the looping cell when its prompt offers
/// `continue_as`, and records every request it served.
#[derive(Default)]
pub(super) struct Model {
    pub(super) calls: AtomicUsize,
    pub(super) requests: Mutex<Vec<LlmRequest>>,
}

impl Model {
    fn provider(self: &Arc<Self>) -> lash_core::facade_support::ProviderHandle {
        let model = Arc::clone(self);
        lash_core::testing::TestProvider::builder()
            .kind("rlm-recorded-behaviour-law")
            .complete(move |request| {
                let model = Arc::clone(&model);
                async move {
                    model.calls.fetch_add(1, Ordering::SeqCst);
                    let offers_continue_as = serde_json::to_string(&request)
                        .expect("request JSON")
                        .contains("continue_as");
                    model.requests.lock().expect("requests").push(request);
                    let text = if offers_continue_as {
                        looping_cell()
                    } else {
                        LOST_FEATURES_ANSWER.to_string()
                    };
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text,
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

pub(super) fn factory(
    config: RlmProtocolPluginConfig,
    backend: &lash_core::Backend,
) -> Arc<dyn PluginFactory> {
    Arc::new(
        RlmProtocolPluginFactory::new(config, Arc::new(crate::TypescriptDialect), backend)
            .with_process_lifecycle(false),
    )
}

/// Open `store`'s session on a deployment whose RLM factory states `config`,
/// as a worker that reloads it from its stores does.
pub(super) async fn open_runtime(
    backend: &lash_core::Backend,
    store: lash_core::store::SessionStore,
    config: RlmProtocolPluginConfig,
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
    let host = PluginHost::new(vec![factory(config, backend)]);
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
    .expect("build the RLM plugin");
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
/// deployment whose RLM factory states other bounds and features runs under
/// the ones its session recorded: its prompt offers `continue_as` (recorded
/// decomposition) and its cell runs a loop the redeploying bound would stop.
async fn a_redriven_root_runs_under_its_recorded_behaviour(
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
    session: &str,
) {
    let backend = double.lash_backend();
    let session_id = SessionId::fixture(session);
    let mut config: lash_core::PersistedSessionConfig = policy().into();
    config.plugin_config = PluginHost::new(vec![factory(creating_config(), &backend)])
        .resolve_creation_plugin_config(
            Some(RLM_PROTOCOL_PLUGIN_ID),
            &lash_core::PluginOptions::default(),
            None,
            true,
            &lash_core::store::plugin_writers::PluginAdmission::default(),
        )
        .expect("the creating deployment records the RLM namespace");
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
                        TurnInput::text("loop it"),
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
                        TurnInput::text("loop it"),
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
    let outcome = serde_json::to_string(&turn.outcome).expect("outcome JSON");
    assert!(
        !outcome.contains(LOST_FEATURES_ANSWER),
        "the redriven root's prompt withheld continue_as: {outcome}"
    );
    assert!(
        matches!(
            turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ) && outcome.contains(&format!("ran {LOOP_ITERATIONS}")),
        "the redriven root's cell ran its loop under the recorded bound: {outcome}"
    );
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        1,
        "one model call: the cell's finish ends the root"
    );
    let request = serde_json::to_string(
        model
            .requests
            .lock()
            .expect("requests")
            .first()
            .expect("the redriven root's request"),
    )
    .expect("request JSON");
    assert!(
        request.contains("continue_as"),
        "the redriven root's prompt offers the recorded decomposition"
    );
}

/// A run nonce, so sessions on a shared PostgreSQL database never collide.
pub(super) fn nonce() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time after epoch")
        .as_nanos();
    (nanos & u128::from(u64::MAX)) as u64
}

pub(super) async fn on_sqlite_file(
    config: lash_restate_test::ServerConfig,
    dir: &tempfile::TempDir,
) -> lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet> {
    let path = dir.path().to_path_buf();
    lash_restate_test::backend_with_store_set(
        0x4398_0001,
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

pub(super) async fn on_sqlite_memory(
    config: lash_restate_test::ServerConfig,
) -> lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet> {
    lash_restate_test::backend_with_store_set(
        0x4398_0002,
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
pub(super) async fn on_postgres(
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

pub(super) fn plain() -> lash_restate_test::ServerConfig {
    lash_restate_test::ServerConfig::default()
}

pub(super) fn always_replay() -> lash_restate_test::ServerConfig {
    lash_restate_test::ServerConfig::default().always_replay(true)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_file() {
    let dir = tempfile::tempdir().expect("SQLite directory");
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_file(plain(), &dir).await,
        "recorded-behaviour-sqlite-file",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_file_always_replay() {
    let dir = tempfile::tempdir().expect("SQLite directory");
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_file(always_replay(), &dir).await,
        "recorded-behaviour-sqlite-file-replay",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_memory() {
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_memory(plain()).await,
        "recorded-behaviour-sqlite-memory",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_sqlite_memory_always_replay() {
    a_redriven_root_runs_under_its_recorded_behaviour(
        on_sqlite_memory(always_replay()).await,
        "recorded-behaviour-sqlite-memory-replay",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_postgres() {
    let Some((double, _attachments)) = on_postgres(plain()).await else {
        return;
    };
    a_redriven_root_runs_under_its_recorded_behaviour(
        double,
        &format!("recorded-behaviour-pg-{}", nonce()),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_redriven_root_runs_under_its_recorded_behaviour_on_postgres_always_replay() {
    let Some((double, _attachments)) = on_postgres(always_replay()).await else {
        return;
    };
    a_redriven_root_runs_under_its_recorded_behaviour(
        double,
        &format!("recorded-behaviour-pg-replay-{}", nonce()),
    )
    .await;
}
