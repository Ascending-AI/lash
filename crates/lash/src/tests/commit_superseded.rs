//! A turn whose commit is superseded mid-turn, on the Restate engine
//! (FIG-4010).
//!
//! Another writer moves the session head while a run's turn runs, so the
//! turn's commit is refused as superseded. The run's `LashTurn` retry would
//! replay the admission base its journal recorded and meet the same moved
//! head on every attempt, so the run ends in the attempt that met the
//! refusal, with `StoreCommitSuperseded` as its typed refusal. It is never
//! retried into a replay that re-decides at a recorded position (a journal
//! mismatch, Restate `RT0016`, or a park) and paused: the engine drains.
//!
//! The law runs on lash-restate's engine over the Restate server double, with
//! the session's store decorated so the forcing write lands through the store
//! the engine executes.

use super::*;
use lash_core::testing::runtime_helpers::{LayeredStores, RecordingDeploymentStore};

const SESSION: &str = "commit-superseded";
const TURN: &str = "superseded-turn";
const FIRST_TURN: &str = "first-turn";
const NEXT_TURN: &str = "next-turn";

struct GenerationFactory;

impl lash_core::plugin::PluginFactory for GenerationFactory {
    fn id(&self) -> &'static str {
        "admission_generation"
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(GenerationExecutor))
    }
}

impl lash_core::plugin::PluginDefinition for GenerationFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("admission_generation")
    }
}

struct GenerationExecutor;

impl lash_core::plugin::SessionPlugin for GenerationExecutor {
    fn id(&self) -> &'static str {
        "admission_generation"
    }

    fn register(
        &self,
        reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.execution().code_executor(Arc::new(GenerationExecutor))
    }
}

#[async_trait]
impl lash_core::plugin::CodeExecutorPlugin for GenerationExecutor {
    async fn frame_switch_carries(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _frame: &lash_core::FrameNodeId,
        _nodes: &[lash_core::SessionAppendNode],
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, SessionError> {
        Ok(Vec::new())
    }

    async fn execute_code(
        &self,
        _ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> std::result::Result<lash_core::ExecResponse, SessionError> {
        Err(SessionError::Protocol("this law executes no cells".into()))
    }

    fn executable_generation(&self) -> Option<lash_core::ExecutableGeneration> {
        Some(lash_core::ExecutableGeneration::new(
            "bound-executor-generation",
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_admission_redrives_under_its_bound_executor_generation() -> Result<()> {
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), |stores| stores).await;
    let double = latest_double().expect("generation law double");
    let provider = crate::testing::TestProvider::builder()
        .kind("generation-law")
        .complete(|_| async { Ok(text_response("answered")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .plugin(Arc::new(GenerationFactory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    double.server().crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRunResultStarting {
            prefix: lash_restate::JournalStepKind::RecordedEffect
                .journal_name("lash:generation-redrive:generation-run:1:0:checkpoint:"),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .handler("run"),
    );
    let session = core
        .session(crate::SessionId::parse("generation-redrive").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session
            .send(TurnInput::text("first turn"))
            .id(crate::TurnId::parse("generation-run").expect("nonblank host identity"))
            .output(),
    )
    .await;
    result.expect("the redrive completes")?;
    let root = nonce_root(&double, "generation-redrive");
    let generation = double
        .server()
        .journal(&root.id)
        .expect("root journal")
        .iter()
        .filter_map(|entry| entry.run_completion())
        .filter_map(|answer| answer.ok())
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).expect("recorded JSON"))
        .find_map(|record| {
            record["outcome"]["Ok"]["record"]["generation"]
                .as_str()
                .map(str::to_owned)
        });
    assert_eq!(
        generation.as_deref(),
        Some("bound-executor-generation"),
        "the recorded transition retains the bound executor generation on redrive"
    );
    double.server().settle().await;
    assert_eq!(
        nonce_root(&double, "generation-redrive").attempts,
        2,
        "the same recorded admission survives a redrive after executor binding"
    );
    Ok(())
}

struct PredecessorGenerationFactory(Arc<AtomicUsize>);

impl lash_core::plugin::PluginFactory for PredecessorGenerationFactory {
    fn id(&self) -> &'static str {
        "admission_generation"
    }

    fn migrate_format(
        &self,
        from: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        mut value: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, lash_core::FormatRefusal> {
        assert_eq!(from, lash_core::FormatVersion::ONE);
        assert_eq!(namespace, lash_core::FormatNamespace::State);
        self.0.fetch_add(1, Ordering::SeqCst);
        let object = value.as_object_mut().expect("state namespace");
        let old = object.remove("old").expect("predecessor value");
        object.insert("native".into(), old);
        Ok(value)
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(GenerationExecutor))
    }
}

impl lash_core::plugin::PluginDefinition for PredecessorGenerationFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        let mut declaration = lash_core::plugin::PluginDeclaration::initial("admission_generation");
        declaration.format_version = lash_core::FormatVersion::new(2).expect("native format");
        declaration.writable_formats =
            vec![lash_core::FormatVersion::ONE, declaration.format_version];
        declaration
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_readable_predecessor_root_converts_in_its_transition_before_running() -> Result<()> {
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), |stores| stores).await;
    let conversions = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("predecessor-generation")
        .complete(|_| async { Ok(text_response("converted")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .plugin(Arc::new(PredecessorGenerationFactory(conversions.clone())))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let created = core
        .session(crate::SessionId::parse("predecessor-generation").expect("nonblank host identity"))
        .created()
        .await;
    let raw: Arc<dyn lash_core::RuntimeStore> = core.store_factory.clone();
    let store = lash_core::store::SessionStore::new(
        raw,
        lash_core::SessionId::from("predecessor-generation"),
    )?;
    let mut state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("created state")
    .state;
    let old = lash_core::PluginState {
        plugins: std::collections::BTreeMap::from([(
            "admission_generation".into(),
            lash_core::PluginNamespaceState {
                format_version: lash_core::FormatVersion::ONE,
                generation: 0,
                publication: Default::default(),
                values: std::collections::BTreeMap::from([("old".into(), serde_json::json!(17))]),
            },
        )]),
    };
    if let Some(bytes) = state.plugin_admission_snapshot() {
        let mut view =
            lash_core::plugin::PluginNativeView::decode(&bytes, lash_core::FleetFormat::current())?;
        view.state = old.clone();
        state.set_plugin_admission_snapshot(view.encode(lash_core::FleetFormat::current())?);
    }
    state.set_plugin_state(Some(old));
    store
        .commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(&state))
        .await?;
    let session = created.open().await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session
            .send(TurnInput::text("readable predecessor"))
            .id(crate::TurnId::parse("predecessor-run").expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("a readable predecessor runs")?;
    assert_eq!(
        conversions.load(Ordering::SeqCst),
        1,
        "only the recorded transition converts the predecessor"
    );
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("published state")
    .state;
    let view = lash_core::plugin::PluginNativeView::decode(
        &state
            .plugin_admission_snapshot()
            .expect("published native view"),
        lash_core::FleetFormat::current(),
    )?;
    assert_eq!(
        view.state.plugins["admission_generation"]
            .format_version
            .get(),
        2
    );
    assert_eq!(
        view.state.plugins["admission_generation"].values["native"],
        serde_json::json!(17)
    );
    Ok(())
}

/// FIG-4848, L-S8: the root records its nonce, then one atomic admission.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_root_records_only_its_nonce_and_atomic_admission_before_preparation() -> Result<()> {
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), |stores| stores).await;
    let double = latest_double().expect("the backend's server double");
    let provider = crate::testing::TestProvider::builder()
        .kind("atomic-admission")
        .complete(|_| async { Ok(text_response("answered")) })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("atomic-admission").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("ask once"))
        .id(crate::TurnId::parse("atomic-run").expect("nonblank host identity"))
        .output()
        .await?;
    double.server().settle().await;
    let root = double
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation
                .target
                .starts_with("LashTurn/16:atomic-admission")
                && invocation.target.ends_with("/run")
        })
        .expect("the sent turn's root invocation");
    let names = double
        .server()
        .journal(&root.id)
        .expect("retained root journal")
        .into_iter()
        .filter(|entry| entry.ty == lash_restate_test::protocol::MessageType::RunCommand)
        .filter_map(|entry| entry.name)
        .collect::<Vec<_>>();
    let admission = names
        .iter()
        .filter(|name| {
            name.contains("shift-admission")
                || name.contains("shift-run-start")
                || name.contains("shift-seal")
                || name.contains("shift-admit")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        admission.len(),
        2,
        "the OS nonce and atomic admission are the only admission records: {names:#?}"
    );
    assert!(admission[0].contains("shift-run-start"));
    assert!(admission[1].contains("shift-admission"));
    Ok(())
}

async fn nonce_core() -> (
    LashCore,
    lash_restate_test::RestateTestBackend,
    Arc<AtomicUsize>,
) {
    let backend =
        double_backend_over(lash_restate_test::ServerConfig::default(), |stores| stores).await;
    let double = latest_double().expect("the nonce law's double");
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let provider = crate::testing::TestProvider::builder()
        .kind("nonce-laws")
        .complete(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            async { Ok(text_response("answered")) }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
        .expect("nonce-law core");
    (core, double, calls)
}

fn nonce_root(
    double: &lash_restate_test::RestateTestBackend,
    session: &str,
) -> lash_restate_test::InvocationView {
    let prefix = format!("LashTurn/{}:{session}", session.len());
    double
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.target.starts_with(&prefix) && invocation.target.ends_with("/run")
        })
        .expect("the root invocation")
}

/// L-S3/L-S8: the transaction committed, but the server lost its result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_atomic_admission_result_reuses_its_nonce_and_bound_rows() -> Result<()> {
    let (core, double, calls) = nonce_core().await;
    double.server().crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeRunResultStarting {
            prefix: lash_restate::JournalStepKind::RecordedEffect
                .journal_name("lash:shift-admission:"),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .handler("run"),
    );
    let session = core
        .session(crate::SessionId::parse("lost-root-result").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session
            .send(TurnInput::text("one admitted input"))
            .id(crate::TurnId::parse("lost-root-run").expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the result-loss retry completes")?;
    double.server().settle().await;
    let root = nonce_root(&double, "lost-root-result");
    assert_eq!(root.attempts, 2, "the loss cut the first attempt");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "adoption executes the retained input once"
    );
    let journal = double.server().journal(&root.id).expect("root journal");
    let start = journal
        .iter()
        .filter_map(|entry| entry.run_completion())
        .filter_map(|answer| answer.ok())
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).expect("recorded JSON"))
        .find_map(|record| {
            record["outcome"]["Ok"]["run_start"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("the recorded OS nonce");
    let session_id = lash_core::SessionId::from("lost-root-result");
    let run = lash_core::TurnId::from("lost-root-run");
    let lash_core::store::RunExecutor::Run { admission } = core
        .store_factory
        .run_executor(&session_id, &run)
        .await?
        .expect("recorded root executor")
    else {
        panic!("root executor");
    };
    let receipt = core
        .store_factory
        .read_shift_admission(&session_id, &admission)
        .await?
        .expect("the transaction's retained receipt");
    assert_eq!(receipt.run_start.as_str(), start);
    let Some(lash_core::store::RunAdmissionAnswer::Admitted { admission, .. }) =
        receipt.run_admission
    else {
        panic!("retained composition");
    };
    assert_eq!(
        admission
            .inputs
            .as_ref()
            .expect("input admission")
            .inputs
            .len(),
        1
    );
    assert_eq!(
        admission.cancel_intent,
        Some(lash_core::TurnCancelIntentSnapshot::Absent)
    );
    Ok(())
}

/// L-S8: purging the invocation destroys the only replayable nonce.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_purged_root_refuses_execution_lost_without_selecting_again() -> Result<()> {
    let (core, double, calls) = nonce_core().await;
    let session = core
        .session(crate::SessionId::parse("purged-root").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text("execute once"))
        .id(crate::TurnId::parse("purged-root-run").expect("nonblank host identity"))
        .output()
        .await?;
    double.server().settle().await;
    let root = nonce_root(&double, "purged-root");
    let journal = double
        .server()
        .journal(&root.id)
        .expect("the original journal");
    let input: serde_json::Value = serde_json::from_slice(
        &journal
            .iter()
            .find_map(|entry| entry.input())
            .expect("original input"),
    )
    .expect("root request");
    let session_id = lash_core::SessionId::from("purged-root");
    let run = lash_core::TurnId::from("purged-root-run");
    let lash_core::store::RunExecutor::Run { admission } = core
        .store_factory
        .run_executor(&session_id, &run)
        .await?
        .expect("original executor")
    else {
        panic!("root executor");
    };
    let original = core
        .store_factory
        .read_shift_admission(&session_id, &admission)
        .await?
        .expect("original root receipt");
    let epoch = core.store_factory.shift_epoch(&session_id).await?;
    assert_eq!(double.server().purge(&root.id), Some(true));
    let key = root
        .target
        .strip_prefix("LashTurn/")
        .and_then(|target| target.strip_suffix("/run"))
        .expect("root key");
    let connection = lash_restate::RestateConnection::with_transport(
        double.server().ingress_url(),
        double.server().transport(),
    );
    let ingress = lash_restate::RestateIngressClient::new(connection);
    #[derive(serde::Deserialize)]
    struct RootAnswer {
        outcome: lash_core::engine::RunOutcome,
    }
    let reply: lash_restate::Reply<RootAnswer> = ingress
        .call_workflow_json("LashTurn", key, "run", &input)
        .await
        .expect("purged invocation reply");
    assert!(matches!(
        reply.body.outcome,
        lash_core::engine::RunOutcome::Refused {
            refusal: lash_core::engine::SealRefusal::ExecutionLost,
            ..
        }
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a purge executes no model work"
    );
    assert_eq!(
        core.store_factory.shift_epoch(&session_id).await?,
        epoch,
        "the fresh nonce raises no epoch"
    );
    let retained = core
        .store_factory
        .read_shift_admission(&session_id, &admission)
        .await?
        .expect("retained original receipt");
    assert_eq!(
        retained.run_start, original.run_start,
        "the fresh invocation cannot replace the retained nonce"
    );
    Ok(())
}

/// A core over the double's engine, and the double it runs on.
struct Fixture {
    core: LashCore,
    double: lash_restate_test::RestateTestBackend,
    provider_calls: Arc<AtomicUsize>,
}

impl Fixture {
    /// The fixture whose provider, answering the first model call, commits
    /// the session head's next revision the way another writer would: the
    /// running turn's commit is then superseded.
    async fn head_moves_under_the_first_turn() -> Self {
        Self::head_moves_under_model_call(0).await
    }

    /// The fixture whose provider moves the head while it answers model
    /// call `moving` (0-based), as another writer would.
    async fn head_moves_under_model_call(moving: usize) -> Self {
        Self::head_moves_under_model_call_and(moving, |_| {}).await
    }

    /// The fixture whose provider moves the head while it answers model
    /// call `moving`, then `arm`s a one-shot fault on the session's store.
    async fn head_moves_under_model_call_and(
        moving: usize,
        arm: impl Fn(&lash_core::testing::runtime_helpers::RecordingStore) + Send + Sync + 'static,
    ) -> Self {
        let arm = Arc::new(arm);
        let catalog = Arc::new(std::sync::OnceLock::<Arc<RecordingDeploymentStore>>::new());
        let installed = Arc::clone(&catalog);
        let backend =
            double_backend_over(lash_restate_test::ServerConfig::default(), move |stores| {
                LayeredStores::over(stores)
                    .map_session_store_factory(|inner| {
                        let recording = Arc::new(RecordingDeploymentStore::over(inner));
                        let _ = installed.set(Arc::clone(&recording));
                        recording
                    })
                    .into_store_set()
            })
            .await;
        let double = latest_double().expect("the double the backend runs on");
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let provider = {
            let provider_calls = Arc::clone(&provider_calls);
            crate::testing::TestProvider::builder()
                .kind("commit-superseded")
                .complete(move |_request| {
                    let provider_calls = Arc::clone(&provider_calls);
                    let catalog = Arc::clone(&catalog);
                    let arm = Arc::clone(&arm);
                    async move {
                        if provider_calls.fetch_add(1, Ordering::SeqCst) == moving {
                            let store = catalog
                                .get()
                                .and_then(|catalog| catalog.store_for(&SessionId::from(SESSION)))
                                .expect("the engine opened the session's store");
                            lash_core::testing::runtime_helpers::advance_session_head(
                                store.as_ref(),
                                |_| {},
                            )
                            .await;
                            arm(&store);
                        }
                        Ok(LlmResponse {
                            usage: paid_usage(),
                            provider_usage: Some(serde_json::json!({ "billed": true })),
                            ..text_response("answered")
                        })
                    }
                })
                .build()
                .into_handle()
        };
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
            .serve_test_llm_profile(provider, mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())
            .expect("build the core");
        Self {
            core,
            double,
            provider_calls,
        }
    }

    /// The invocations still open once the engine has settled: a shift a
    /// late ask started (a relay's, or the one queued behind the running
    /// shift) is given until a deadline to finish, so only work that never
    /// finishes is left.
    async fn open_after_settling(&self) -> Vec<lash_restate_test::InvocationView> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            self.double.server().settle().await;
            let open = self
                .double
                .server()
                .invocations()
                .into_iter()
                .filter(|view| view.status != "completed")
                .collect::<Vec<_>>();
            if open.is_empty() || tokio::time::Instant::now() >= deadline {
                return open;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Every `LashTurn` run of `turn` the double saw.
    fn turn_runs(&self, turn: &str) -> Vec<lash_restate_test::InvocationView> {
        self.double.server().turn_invocations(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::fixture(turn),
        )
    }
}

/// What the provider reports for every paid call of this module's turns.
fn paid_usage() -> lash_core::llm::types::LlmUsage {
    lash_core::llm::types::LlmUsage {
        input_tokens: 11,
        output_tokens: 4,
        ..lash_core::llm::types::LlmUsage::default()
    }
}

/// The law: the superseded commit ends its run with the typed refusal in the
/// one attempt that met it. The run's execution completes on its first attempt,
/// the send answers `StoreCommitSuperseded`, and every invocation the shift
/// made completes: nothing is left retrying or paused on the engine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_superseded_turn_commit_ends_its_run_typed_and_never_pauses() -> Result<()> {
    let fixture = Fixture::head_moves_under_the_first_turn().await;
    let session = fixture
        .core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let superseded = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(crate::TurnId::parse(TURN).expect("nonblank host identity"))
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(TURN);
    let superseded = superseded
        .unwrap_or_else(|_| panic!("the superseded turn settles; its runs: {runs:#?}"))
        .expect_err("the superseded commit ends the turn with its refusal");
    let EmbedError::Runtime(refusal) = &superseded else {
        panic!("the refusal is the typed runtime error: {superseded:?}; runs: {runs:#?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::StoreCommitSuperseded,
        "the turn ends with the superseded commit: {refusal:?}; runs: {runs:#?}"
    );
    // The send answers from the store, which can show the refusal before the
    // run's execution has returned to the engine: read the run once it settled.
    fixture.double.server().settle().await;
    let runs = fixture.turn_runs(TURN);
    let [run] = runs.as_slice() else {
        panic!("the run ran in one invocation: {runs:#?}");
    };
    assert_eq!(
        run.status, "completed",
        "the run's execution completed: {run:?}"
    );
    assert_eq!(
        run.attempts, 1,
        "the superseded commit is never retried: {run:?}"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "the model is called once"
    );

    let open = fixture
        .double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.status != "completed")
        .collect::<Vec<_>>();
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}

/// FIG-4018: a run that ends with a typed refusal is not left the session's
/// unfinished run. Its end is written to the store, so the session's next
/// send is admitted under a new run and completes, rather than re-admitting
/// the refused run and answering its old refusal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_send_after_a_refused_run_executes_a_new_run() -> Result<()> {
    // The head moves under the second turn, so the first commits the head
    // the session's later turns run on.
    let fixture = Fixture::head_moves_under_model_call(1).await;
    let session = fixture
        .core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's first turn"))
            .id(crate::TurnId::parse(FIRST_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the first turn settles")?;

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(crate::TurnId::parse(TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the refused turn settles")
    .expect_err("the superseded commit ends the turn with its refusal");
    assert!(
        matches!(&refused, EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::StoreCommitSuperseded),
        "the first run ends with its typed refusal: {refused:?}"
    );

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's next send"))
            .id(crate::TurnId::parse(NEXT_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(NEXT_TURN);
    let next = next
        .unwrap_or_else(|_| panic!("the next send settles; its runs: {runs:#?}"))
        .unwrap_or_else(|error| {
            panic!("the next send completes under a new run: {error:?}; runs: {runs:#?}")
        });
    assert_eq!(
        next.assistant_message(),
        Some("answered"),
        "the next send is answered by its own turn"
    );
    let [_] = runs.as_slice() else {
        panic!("the next send ran as its own run: {runs:#?}");
    };

    let open = fixture.open_after_settling().await;
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}

/// FIG-4018's crash window: the refused run's execution dies after its end is
/// written to the store and before the engine records its outcome. The
/// replay retraces the run's journal to the same refusal, writes nothing
/// more and records the outcome, so the run has one terminal, the refusal,
/// and nothing is left paused. The refused send answers the refusal, and
/// the session's next send is admitted under a new run and completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_run_crashed_before_its_outcome_converges_on_one_terminal() -> Result<()> {
    let fixture = Fixture::head_moves_under_model_call(1).await;
    let session = fixture
        .core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's first turn"))
            .id(crate::TurnId::parse(FIRST_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the first turn settles")?;
    fixture.double.server().crash_on(
        lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeStateWrite {
            key: "outcome".to_owned(),
            value_contains: Some(format!("\"run\":\"{TURN}\"")),
        })
        .service(lash_restate_test::TURN_DRIVER_SERVICE)
        .within_attempts(1),
    );

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(crate::TurnId::parse(TURN).expect("nonblank host identity"))
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(TURN);
    let refused = refused
        .unwrap_or_else(|_| panic!("the refused turn settles; its runs: {runs:#?}"))
        .expect_err("the superseded commit ends the turn with its refusal");
    assert!(
        matches!(&refused, EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::StoreCommitSuperseded),
        "the refused run answers its typed refusal: {refused:?}; runs: {runs:#?}"
    );

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's next send"))
            .id(crate::TurnId::parse(NEXT_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the next send settles")
    .unwrap_or_else(|error| panic!("the next send completes under a new run: {error:?}"));
    assert_eq!(next.assistant_message(), Some("answered"));

    fixture.double.server().settle().await;
    let runs = fixture.turn_runs(TURN);
    let [run] = runs.as_slice() else {
        panic!("the refused run ran in one invocation: {runs:#?}");
    };
    assert_eq!(run.status, "completed", "the replay completed: {run:?}");
    assert_eq!(
        run.attempts, 2,
        "the crash cut the first attempt and the replay finished: {run:?}"
    );
    let terminal = fixture
        .core
        .store_factory
        .run_terminal(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::from(TURN),
        )
        .await?
        .expect("the refused run has terminal evidence");
    assert!(
        matches!(
            &terminal.cause,
            lash_core::store::RunTerminalCause::Refused { code, .. }
                if *code == lash_core::RuntimeErrorCode::StoreCommitSuperseded
        ),
        "the run's one terminal is its refusal: {terminal:?}"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        3,
        "the replay called no model: the first turn, the refused turn and the next send did"
    );
    let open = fixture.open_after_settling().await;
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}

/// Shift FIG-4058's crash cells: the session's first turn commits, the head
/// moves under the second turn's model call and `fixture` arms its fault,
/// and the engine retries the second run's execution past its recorded
/// `shift-admit` head verdict. The replay honours the recorded `Ready`, retraces
/// the journal to the superseded commit and ends the run with that
/// refusal: the send answers it, the run's one terminal is the refusal, it
/// is never parked, and the session's next send completes under a new run.
async fn a_redriven_run_past_shift_head_ends_with_its_refusal(fixture: Fixture) -> Result<()> {
    let session = fixture
        .core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's first turn"))
            .id(crate::TurnId::parse(FIRST_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the first turn settles")?;

    let refused = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the head moves under this turn's commit"))
            .id(crate::TurnId::parse(TURN).expect("nonblank host identity"))
            .output(),
    )
    .await;
    let runs = fixture.turn_runs(TURN);
    let refused = refused
        .unwrap_or_else(|_| panic!("the redriven turn settles; its runs: {runs:#?}"))
        .expect_err("the superseded commit ends the turn with its refusal");
    assert!(
        matches!(&refused, EmbedError::Runtime(error)
            if error.code == lash_core::RuntimeErrorCode::StoreCommitSuperseded),
        "the redriven run answers its typed refusal, never a park: {refused:?}; runs: {runs:#?}"
    );

    let session_id = lash_core::SessionId::from(SESSION);
    let run = lash_core::TurnId::from(TURN);
    let terminal = fixture
        .core
        .store_factory
        .run_terminal(&session_id, &run)
        .await?
        .expect("the redriven run has terminal evidence");
    assert!(
        matches!(
            &terminal.cause,
            lash_core::store::RunTerminalCause::Refused { code, .. }
                if *code == lash_core::RuntimeErrorCode::StoreCommitSuperseded
        ),
        "the run's one terminal is its refusal: {terminal:?}"
    );
    let parks = fixture
        .core
        .store_factory
        .list_turn_parks(&lash_core::store::TurnParkQuery {
            reasons: None,
            session: Some(session_id.clone()),
            parked_at_or_before_ms: None,
            after: None,
            limit: std::num::NonZeroUsize::new(16).expect("a nonzero page"),
        })
        .await?;
    assert!(parks.is_empty(), "the run is never parked: {parks:#?}");

    let next = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        session
            .send(TurnInput::text("the session's next send"))
            .id(crate::TurnId::parse(NEXT_TURN).expect("nonblank host identity"))
            .output(),
    )
    .await
    .expect("the next send settles")
    .unwrap_or_else(|error| panic!("the next send completes under a new run: {error:?}"));
    assert_eq!(next.assistant_message(), Some("answered"));

    fixture.double.server().settle().await;
    let runs = fixture.turn_runs(TURN);
    let [run] = runs.as_slice() else {
        panic!("the redriven run ran in one invocation: {runs:#?}");
    };
    assert_eq!(run.status, "completed", "the replay completed: {run:?}");
    assert_eq!(
        run.attempts, 2,
        "the fault cut the first attempt and the replay finished: {run:?}"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        3,
        "the replay called no model: the first turn, the redriven turn and the next send did"
    );
    let open = fixture.open_after_settling().await;
    assert!(
        open.is_empty(),
        "the engine drains: no invocation is left running, retrying or paused: {open:#?}"
    );
    Ok(())
}

/// FIG-4058: a live fault the engine retries after the run's journal has
/// run past `shift-admit`. The head moved under the turn and its commit
/// meets a live store fault, so the retry replays a journal that already
/// holds the turn's model call. Its recorded `Ready` is honoured rather
/// than turned into `Diverged` by the moved head, which parked the run at
/// a recorded position.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_fault_retried_past_shift_head_keeps_the_recorded_ready() -> Result<()> {
    let fixture = Fixture::head_moves_under_model_call_and(1, |store| {
        store.fail_next_runtime_commit(lash_core::StoreError::Backend(
            "injected live fault on the turn's commit".to_string(),
        ));
    })
    .await;
    Box::pin(a_redriven_run_past_shift_head_ends_with_its_refusal(
        fixture,
    ))
    .await
}

/// FIG-4058: the refused run fails between meeting its refusal and writing
/// the run's end (FIG-4018). The retry replays to the same refusal and
/// ends the run, rather than meeting the moved head with no terminal
/// written and parking it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_run_failed_before_its_end_write_replays_to_an_ended_run() -> Result<()> {
    let fixture = Fixture::head_moves_under_model_call_and(1, |store| {
        store.fail_next_end_refused_run(lash_core::StoreError::Backend(
            "injected live fault on the refused run's end".to_string(),
        ));
    })
    .await;
    Box::pin(a_redriven_run_past_shift_head_ends_with_its_refusal(
        fixture,
    ))
    .await
}
