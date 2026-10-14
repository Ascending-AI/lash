//! FIG-6167: a before-model refusal keeps its typed cause after a crash.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_durable::{ActorKey, ActorState, CommitLabel, DurableStore};
use lash_durable_test::{Fault, Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};

use super::{served, sim};

const PROTOCOL: &str = "refusing-protocol";
const FOREIGN_TEXT: &str = "secret foreign refusal text";

struct RefusingProtocol(Arc<AtomicUsize>);

impl lash::plugins::PluginMetadata for RefusingProtocol {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial(PROTOCOL)
    }
}

impl lash::plugins::PluginFactory for RefusingProtocol {
    fn id(&self) -> &'static str {
        PROTOCOL
    }

    fn recipe(self: Arc<Self>) -> Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<Self>::new(PROTOCOL);
        reg.protocol()
            .session(|this, _state| Ok(Arc::new(RefusingProtocol(Arc::clone(&this.0)))))?;
        reg.protocol()
            .protocol_driver(|_this, _state| Ok(Arc::new(ModelDriver)))?;
        Ok(lash::plugins::PluginRecipe::new(
            PROTOCOL,
            reg,
            move |_ctx, _state| Ok(Arc::new(RefusingProtocol(Arc::clone(&self.0)))),
        ))
    }
}

impl lash::plugins::SessionPlugin for RefusingProtocol {
    fn id(&self) -> &'static str {
        PROTOCOL
    }
}

#[async_trait::async_trait]
impl ProtocolSessionPlugin for RefusingProtocol {
    async fn before_llm_call(
        &self,
        _ctx: lash_core::plugin::ProtocolBeforeLlmCallContext,
        _request: &lash_core::LlmRequest,
    ) -> Result<Option<lash_core::plugin::ProtocolLlmCallAction>, lash::plugins::PluginError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(lash::plugins::PluginError::Runtime(
            lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::ProtocolBeforeLlmCall,
                FOREIGN_TEXT,
            ),
        ))
    }
}

struct ModelDriver;

impl ProtocolDriverPlugin for ModelDriver {
    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        lash_core::TurnDriverPreamble {
            config: lash_core::TurnDriverConfig::chat(Arc::new(Self)),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            writer_formats: input.writer_formats,
        }
    }
}

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for ModelDriver {
    fn prepare_protocol_iteration(
        &self,
        ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Start(
            lash_core::sansio::PendingWork::Llm {
                request: ctx
                    .project_llm_request(false)
                    .expect("the request projects"),
                driver_state: None,
            },
        )]
    }

    fn handle_llm_success(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        _state: Option<lash_core::ProtocolDriverState>,
        _response: lash_core::LlmResponse,
        _calls: &lash_sansio::ResponseToolCalls,
        _streamed: bool,
    ) -> Vec<lash_core::DriverAction> {
        panic!("a refused call cannot succeed")
    }

    fn handle_tool_results(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<lash_core::DriverAction> {
        panic!("the protocol starts no tools")
    }

    fn handle_exec_result(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _state: lash_core::ProtocolDriverState,
        _result: Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<lash_core::DriverAction> {
        panic!("the protocol starts no code")
    }
}

fn core(backend: lash::Backend, hooks: Arc<AtomicUsize>, boot: &str) -> lash::LashCore {
    let model = lash_core::testing::TestProvider::builder()
        .kind("refused-call")
        .complete(|_request: lash_core::LlmRequest| async {
            panic!("a before-model refusal must send no provider request")
        })
        .build()
        .into_handle();
    lash::LashCore::builder(backend)
        .protocol_plugin(Arc::new(RefusingProtocol(hooks)))
        .serve_sessions(false)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .serve_test_llm_profile(model, served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("refusal-deployment"),
            lash::persistence::LeaseIncarnationId::new(boot),
        ))
        .expect("the core builds")
}

fn nodes(
    database: Arc<dyn DurableStore>,
    clock: Arc<SimClock>,
    script: Script,
    core: &lash::LashCore,
    backend: &lash::Backend,
) -> SimNodes {
    SimNodes::new(
        database,
        clock,
        script,
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(core),
            max_active: 4,
        },
        Arc::new(
            lash_core::runtime::durable::session::SessionActivation::new(
                backend.clone(),
                lash::testing::session_turn_services(core),
                Arc::new(Tripwire::default()),
            ),
        ),
    )
}

#[tokio::test]
async fn protocol_before_model_refusal_keeps_its_specific_cause_after_crash_and_resume() {
    let clock = SimClock::new();
    let stores = Arc::new(sim::memory(Arc::clone(&clock)).await);
    let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let hooks = Arc::new(AtomicUsize::new(0));
    let first = core(backend.clone(), Arc::clone(&hooks), "first");
    let session_id = lash::SessionId::from("refusal-cause");
    let run = lash::TurnId::from("refusal-run");
    let session = first
        .session(session_id.clone())
        .create(lash::SessionCreation::root(served::spec(1)))
        .await
        .expect("the session is created");
    session
        .send(lash::TurnInput::text("refuse before the model"))
        .id(run.clone())
        .await
        .expect("the input is sent");

    let script = Script::new();
    script.cut_on("a", CommitLabel::TURN_COMMIT, 1, Fault::CommitThenAbort);
    let before = nodes(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        &first,
        &backend,
    );
    before.start("a");
    before.quiesce().await;
    while before.script().cuts().is_empty() {
        assert!(clock.logical_ms() < 600_000, "the refusal did not commit");
        assert!(before.step().await.is_some(), "the refusal stalled");
    }
    before.kill("a");
    before.quiesce().await;
    drop(session);
    drop(before);
    drop(first);

    let cold = core(backend.clone(), Arc::clone(&hooks), "cold");
    let after = nodes(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        &cold,
        &backend,
    );
    after.start("b");
    after.quiesce().await;
    let actor = ActorKey::session(session_id.as_str()).expect("the session actor");
    let horizon = clock.logical_ms() + 600_000;
    while database
        .actor(&actor)
        .await
        .expect("the actor reads")
        .is_none_or(|snapshot| snapshot.state != ActorState::Idle)
    {
        assert!(
            clock.logical_ms() < horizon,
            "the cold owner did not finish"
        );
        assert!(after.step().await.is_some(), "the cold owner stalled");
    }
    let report = cold
        .session(session_id)
        .durable()
        .await
        .expect("the durable session opens")
        .attach_id(run)
        .output()
        .await
        .expect("the stopped turn reads")
        .result;
    assert_eq!(report.source, lash::ReportSource::Durable);
    assert!(
        report.errors.is_empty(),
        "the follower observed no failure activity"
    );
    assert_eq!(
        hooks.load(Ordering::SeqCst),
        1,
        "recovery reran the refused hook"
    );
    let cause = report
        .error_cause()
        .expect("the terminal carries its cause");
    assert_eq!(cause.code.code(), "protocol_refused_before_model_call");
    assert_eq!(
        cause.detail(),
        Some("protocol refused the turn before the model call")
    );
    assert!(!serde_json::to_string(cause).unwrap().contains(FOREIGN_TEXT));
}

/// FIG-6211: the terminal kind owns the failed plugin stage, independently of
/// activity observations, and reopening the store returns the identical cause.
#[tokio::test]
async fn before_llm_call_plugin_failure_keeps_its_stage_after_store_reopen() {
    let dir = tempfile::tempdir().expect("the store directory");
    let path = dir.path().join("plugin-stage.db");
    let clock = SimClock::new();
    let hooks = Arc::new(AtomicUsize::new(0));
    let session_id = lash::SessionId::from("plugin-stage");
    let run = lash::TurnId::from("plugin-stage-run");
    let committed = {
        let stores = Arc::new(sim::file(&path, Arc::clone(&clock)).await);
        let database: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        let backend = served::configured_backend(stores, sim::settings(), Vec::new());
        let first = core(backend.clone(), Arc::clone(&hooks), "first");
        let session = first
            .session(session_id.clone())
            .create(lash::SessionCreation::root(served::spec(1)))
            .await
            .expect("the session is created");
        session
            .send(lash::TurnInput::text("fail the before-call hook"))
            .id(run.clone())
            .await
            .expect("the input is sent");
        let owner = nodes(
            Arc::clone(&database),
            Arc::clone(&clock),
            Script::new(),
            &first,
            &backend,
        );
        owner.start("a");
        owner.quiesce().await;
        let actor = ActorKey::session(session_id.as_str()).expect("the session actor");
        while database
            .actor(&actor)
            .await
            .expect("the actor reads")
            .is_none_or(|snapshot| snapshot.state != ActorState::Idle)
        {
            assert!(
                clock.logical_ms() < 600_000,
                "the failed turn did not commit"
            );
            assert!(owner.step().await.is_some(), "the failed turn stalled");
        }
        let report = first
            .session(session_id.clone())
            .durable()
            .await
            .expect("the durable session opens")
            .attach_id(run.clone())
            .output()
            .await
            .expect("the committed terminal reads")
            .result;
        let cause = report.error_cause().expect("the terminal cause").clone();
        owner.kill("a");
        owner.quiesce().await;
        cause
    };
    let stores = Arc::new(sim::file(&path, Arc::clone(&clock)).await);
    let backend = served::configured_backend(stores, sim::settings(), Vec::new());
    let cold = core(backend, Arc::clone(&hooks), "reopened");
    let report = cold
        .session(session_id)
        .durable()
        .await
        .expect("the reopened session")
        .attach_id(run)
        .output()
        .await
        .expect("the reopened terminal reads")
        .result;
    assert_eq!(report.source, lash::ReportSource::Durable);
    assert!(
        report.errors.is_empty(),
        "no activity contributes to the cause"
    );
    let reopened = report.error_cause().expect("the reopened terminal cause");
    assert_eq!(reopened, &committed);
    assert_eq!(hooks.load(Ordering::SeqCst), 1, "the hook never reruns");
    assert_eq!(
        reopened.code,
        lash::TurnErrorCode::Plugin {
            stage: lash::PluginStage::BeforeLlmCall
        }
    );
    assert_eq!(
        serde_json::to_value(reopened).unwrap()["code"],
        serde_json::json!({"plugin": {"stage": "before_llm_call"}}),
        "the terminal kind must retain the typed plugin stage"
    );
    assert!(
        !serde_json::to_string(reopened)
            .unwrap()
            .contains(FOREIGN_TEXT)
    );
}
