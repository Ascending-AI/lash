//! A protocol's code effect on the durable turn path: a protocol that
//! starts code in a session with no code executor stops the turn typed,
//! its driver handed the cause.

use super::*;
use lash::TurnInput;

use lash_core::facade_support::{TurnOutcome, TurnStop};
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};
use lash_protocol_types::ReportedFailure;

const PROTOCOL: &str = "test_protocol";

/// Every code failure the test protocol's driver was handed, encoded.
type Received = Arc<StdMutex<Vec<serde_json::Value>>>;

/// A protocol whose every turn starts one code cell, with no code
/// executor.
struct TestProtocolFactory(Received, bool, bool);

impl lash_core::facade_support::PluginFactory for TestProtocolFactory {
    fn id(&self) -> &'static str {
        PROTOCOL
    }

    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut reg = lash::plugins::PluginRegistrar::<TestProtocolPlugin>::new(
            lash::plugins::PluginFactory::id(self.as_ref()),
        );
        TestProtocolPlugin::declare(&mut reg, self.1, self.2)?;
        let id = self.id();
        Ok(
            lash::plugins::PluginRecipe::new(id, reg, move |_ctx, _state| {
                Ok(Arc::new(TestProtocolPlugin(Arc::clone(&self.0))))
            })
            .with_transcript_decoder(Arc::new(TestCellDecoder)),
        )
    }
}

impl TestProtocolFactory {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(PROTOCOL)
    }
}
impl lash_core::plugin::PluginMetadata for TestProtocolFactory {
    fn plugin_declaration(&self) -> lash_core::plugin::PluginDeclaration {
        Self::declaration()
    }
}

struct TestProtocolPlugin(Received);

impl lash_core::facade_support::SessionPlugin for TestProtocolPlugin {
    fn id(&self) -> &'static str {
        PROTOCOL
    }
}
impl TestProtocolPlugin {
    fn declare(
        reg: &mut lash::plugins::PluginRegistrar<Self>,
        fail_invocation: bool,
        stopped: bool,
    ) -> std::result::Result<(), lash::plugins::PluginError> {
        if stopped {
            reg.execution()
                .code_executor(|_, _| Ok(Arc::new(StoppedExecutor)))?;
        } else if fail_invocation {
            reg.execution()
                .code_executor(|_, _| Ok(Arc::new(FailedInvocation)))?;
        }
        reg.protocol()
            .session(move |_this, _state| Ok(Arc::new(TestProtocolSession)))?;

        reg.protocol().protocol_driver(move |this, _state| {
            Ok(Arc::new(TestProtocolDriver(Arc::clone(&this.0))))
        })?;
        Ok(())
    }
}

struct TestProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for TestProtocolSession {}

struct TestProtocolDriver(Received);

impl ProtocolDriverPlugin for TestProtocolDriver {
    fn attachment_omission_text(
        &self,
        notice: &lash_protocol_types::AttachmentMaterializationNotice,
    ) -> String {
        format!("({} omitted)", notice.media_type)
    }

    fn build_preamble(
        &self,
        input: lash_core::ProtocolBuildInput,
    ) -> lash_core::TurnDriverPreamble {
        lash_core::TurnDriverPreamble {
            config: lash_core::TurnDriverConfig::chat(Arc::new(TestDriver(Arc::clone(&self.0)))),
            tool_specs: input.tool_catalog.model_tool_specs(),
            tool_names: input.tool_catalog.tool_names(),
            writer_formats: input.writer_formats,
        }
    }
}

/// Starts one code cell; records and reports the failure it was handed,
/// and stops.
struct TestDriver(Received);

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for TestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Start(
            lash_core::sansio::PendingWork::Exec {
                language: "code".to_string(),
                code: "console.log('effect seam')".to_string(),
                driver_state: lash_core::ProtocolDriverState::new(
                    PROTOCOL,
                    serde_json::Value::Null,
                ),
            },
        )]
    }

    fn handle_llm_success(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _request: Arc<lash_core::LlmRequest>,
        _driver_state: Option<lash_core::ProtocolDriverState>,
        _llm_response: LlmResponse,
        _calls: &lash_core::sansio::ResponseToolCalls,
        _text_streamed: bool,
    ) -> Vec<lash_core::DriverAction> {
        Vec::new()
    }

    fn handle_tool_results(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _completed: Vec<lash_core::sansio::CompletedToolCall>,
    ) -> Vec<lash_core::DriverAction> {
        Vec::new()
    }

    fn handle_exec_result(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
        _driver_state: lash_core::ProtocolDriverState,
        result: std::result::Result<lash_core::ExecResponse, lash_core::ExecCodeFailure>,
    ) -> Vec<lash_core::DriverAction> {
        let mut actions = Vec::new();
        if let Err(error) = result {
            let record = lash_core::CellRecord {
                id: "failed-cell".into(),
                protocol_iteration: 0,
                language: "code".into(),
                code: "console.log('effect seam')".into(),
                prints: Vec::new(),
                prints_retained: None,
                images: Vec::new(),
                calls: Vec::new(),
                read_facts: Vec::new(),
                bindings: Default::default(),
                consumption: Vec::new(),
                result: lash_core::CellOutcome::Failed(error.clone().into()),
            };
            actions.push(lash_core::DriverAction::AppendEvents(vec![
                lash_core::SessionHistoryRecord::Protocol(
                    lash_core::ProtocolEvent::typed(PROTOCOL, record).expect("a cell encodes"),
                ),
            ]));
            let encoded = serde_json::to_value(&error).expect("an exec failure encodes");
            // The failure as the driver received it, reported as the turn's
            // error, so a host can read every field the driver was handed.
            actions.push(lash_core::DriverAction::Emit(
                lash_core::facade_support::SessionStreamEvent::Error(ReportedFailure {
                    message: encoded.to_string(),
                    envelope: None,
                }),
            ));
            self.0.lock_recover().push(encoded);
        }
        actions.push(lash_core::DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::RuntimeError {
                cause: lash_protocol_types::TurnErrorCause::new(
                    lash_protocol_types::TurnErrorCode::ExecutorUnavailable,
                ),
            },
        )));
        actions
    }
}

/// The output of one turn of a session whose protocol starts code with no
/// code executor, and the failures its driver was handed.
async fn a_turn_without_a_code_executor() -> Result<(lash::TurnOutput, Vec<serde_json::Value>)> {
    a_failed_cell(false).await
}

async fn a_failed_cell(
    fail_invocation: bool,
) -> Result<(lash::TurnOutput, Vec<serde_json::Value>)> {
    let received = Received::default();
    let sink = Arc::new(lash::testing::RecordingTraceSink::new());
    let core = explicit_ephemeral_facets(
        LashCore::builder(sqlite_memory_store_backend().await).protocol_plugin(Arc::new(
            TestProtocolFactory(Arc::clone(&received), fail_invocation, false),
        )),
    )
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .trace_sink(sink.clone())
    .build(lash_facade_tests::runtime_lease_owner())?;
    let session = core
        .session(
            lash_kernel_value::identity::SessionId::parse("protocol-effects")
                .expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let output = session.send(TurnInput::text("run code")).output().await?;
    drop(session);
    core.shutdown().await?;
    if fail_invocation {
        let expected = serde_json::json!({
            "effect_kind": "process",
            "controller_code": "lash:runtime_effect_local_executor_unavailable",
            "detail": "the nested cell invocation failed",
        });
        let records = sink.json_records().expect("trace serializes");
        let terminal = records
            .iter()
            .find(|record| record["type"] == "turn_completed")
            .expect("turn terminal trace");
        assert_eq!(
            terminal.pointer("/outcome/effect_refusal"),
            Some(&expected),
            "FIG-7426: the trace identifies the same refusal as the settlement"
        );
        let failure = output
            .activities
            .iter()
            .find_map(|activity| {
                let value = serde_json::to_value(&activity.event).expect("activity serializes");
                (value["type"] == "error").then_some(value)
            })
            .expect("effect refusal activity");
        assert_eq!(failure.pointer("/envelope/effect_refusal"), Some(&expected));
    }
    let received = received.lock_recover().clone();
    Ok((output, received))
}

fn executor_unavailable() -> serde_json::Value {
    serde_json::json!({
        "reason": "executor_unavailable",
        "message": "code execution is not available in this session",
    })
}

/// A protocol that starts code in a session with no code executor stops
/// the turn `RuntimeError`, its driver handed the typed
/// `executor_unavailable` failure once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_exec_without_code_executor_stops_as_runtime_error() -> Result<()> {
    let (output, received) = a_turn_without_a_code_executor().await?;
    assert!(
        matches!(
            output.outcome,
            TurnOutcome::Stopped(TurnStop::RuntimeError { .. })
        ),
        "{:?}",
        output.outcome
    );
    assert_eq!(
        received,
        vec![executor_unavailable()],
        "the driver is handed the typed failure once"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_stop_cause_survives_owner_replacement() -> Result<()> {
    fn core(backend: lash_execution_contracts::backend::Backend) -> Result<LashCore> {
        explicit_ephemeral_facets(LashCore::builder(backend).protocol_plugin(Arc::new(
            TestProtocolFactory(Arc::new(std::sync::Mutex::new(Vec::new())), false, false),
        )))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())
    }
    let backend = sqlite_memory_store_backend().await;
    let running = core(backend.clone())?;
    let id = lash_kernel_value::identity::SessionId::from("runtime-stop-cause");
    let run = lash_vocabulary::identity::TurnId::from("runtime-stop-cause-run");
    let session = running
        .session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let live = session
        .send(lash::TurnInput::text("run code"))
        .id(run.clone())
        .output()
        .await?
        .outcome;
    drop(session);
    running.shutdown().await?;
    let reopened = core(backend)?;
    let report = reopened
        .session(id)
        .open()
        .await?
        .attach_id(run)
        .output()
        .await?
        .outcome;
    assert_eq!(report, live);
    assert_eq!(report.error_cause(), live.error_cause());
    assert_eq!(
        lash::TurnStatus::of_outcome(&report),
        lash::TurnStatus::Failed {
            cause: live.error_cause().cloned(),
        }
    );
    let encoded = serde_json::to_value(&report)?;
    assert_eq!(
        encoded.pointer("/stopped/runtime_error/cause/code"),
        Some(&serde_json::json!("executor_unavailable"))
    );
    assert_eq!(
        encoded.pointer("/stopped/runtime_error/cause/detail"),
        Some(&serde_json::json!(
            "code execution is not available in this session"
        ))
    );
    reopened.shutdown().await?;
    Ok(())
}

/// A nested effect refusal leaves the invocation without an executor response.
struct FailedInvocation;

#[async_trait]
impl lash_core::plugin::CodeExecutorPlugin for FailedInvocation {
    async fn frame_switch_carries(
        &self,
        _: lash_core::plugin::ProtocolSessionContext<'_>,
        _: &lash_core::FrameNodeId,
        _: &[lash_core::SessionAppendNode],
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, lash_core::SessionError> {
        Ok(Vec::new())
    }

    async fn execute_code(
        &self,
        ctx: lash_core::RuntimeExecutionContext<'_>,
        _: lash_core::ExecRequest,
    ) -> std::result::Result<lash_core::ExecResponse, lash_core::SessionError> {
        ctx.record_nested_runtime_effect_error(
            lash_core::RuntimeEffectControllerError::new(
                lash_core::RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                "the nested cell invocation failed",
            )
            .with_effect_kind(lash_core::runtime::RuntimeEffectKind::Process),
        );
        Err(lash_core::SessionError::CodeExecutionUnavailable)
    }
}

/// S1: an invocation failure has no observed duration; an answered failure does.
/// FIG-7426: a nested refusal retains its own operation, code and detail on
/// the settlement, error activity and terminal trace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cell_failure_activity_distinguishes_unmeasured_from_measured_duration() -> Result<()> {
    for invocation_failed in [true, false] {
        let (output, _) = a_failed_cell(invocation_failed).await?;
        let completion = output
            .activities
            .iter()
            .find_map(|activity| {
                matches!(&activity.event, lash::TurnEvent::CodeBlockCompleted { .. })
                    .then(|| serde_json::to_value(&activity.event).expect("activity serializes"))
            })
            .expect("the failed cell publishes its completion");
        if invocation_failed {
            let terminal = serde_json::to_value(&output.outcome)?;
            assert_eq!(
                terminal.pointer("/stopped/runtime_error/cause/effect_refusal"),
                Some(&serde_json::json!({
                    "effect_kind": "process",
                    "controller_code": "lash:runtime_effect_local_executor_unavailable",
                    "detail": "the nested cell invocation failed",
                })),
                "FIG-7426: a host can identify the refused effect and controller's answer"
            );
            assert!(completion.get("duration_ms").is_none(), "{completion}");
            assert!(completion.get("prints").is_none(), "{completion}");
            assert!(completion.get("tool_call_ids").is_none(), "{completion}");
        } else {
            assert!(completion["duration_ms"].is_u64(), "{completion}");
        }
    }
    Ok(())
}

struct TestCellDecoder;
impl lash_core::transcript::TranscriptDecoderPlugin for TestCellDecoder {
    fn decode_event(
        &self,
        event: &lash_core::ProtocolEvent,
    ) -> std::result::Result<
        Option<lash_core::transcript::TranscriptItem>,
        lash_core::StoredDataCorruption,
    > {
        Ok(event
            .decode::<lash_core::CellRecord>(PROTOCOL)
            .expect("the test protocol retains a cell")
            .map(|cell| lash_core::transcript::TranscriptItem::Cell(Arc::new(cell))))
    }
}

struct StoppedExecutor;
#[async_trait]
impl lash_core::plugin::CodeExecutorPlugin for StoppedExecutor {
    async fn frame_switch_carries(
        &self,
        _: lash_core::plugin::ProtocolSessionContext<'_>,
        _: &lash_core::FrameNodeId,
        _: &[lash_core::SessionAppendNode],
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, lash_core::SessionError> {
        Ok(Vec::new())
    }
    async fn execute_code(
        &self,
        _: lash_core::RuntimeExecutionContext<'_>,
        _: lash_core::ExecRequest,
    ) -> std::result::Result<lash_core::ExecResponse, lash_core::SessionError> {
        Err(lash_core::SessionError::CodeExecutionRuntimeStopped)
    }
}

/// CELL-FAILURE-CATEGORY: live return, committed completion and reopened history retain the executor's typed reason.
#[tokio::test]
async fn runtime_stopped_cell_failure_agrees_live_committed_and_reopened() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let build = || {
        explicit_ephemeral_facets(LashCore::builder(backend.clone()).protocol_plugin(Arc::new(
            TestProtocolFactory(Received::default(), false, true),
        )))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())
    };
    let core = build()?;
    let id = lash_kernel_value::identity::SessionId::from("runtime-stopped-cell");
    let session = core
        .session(id.clone())
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let output = session.send(TurnInput::text("run code")).output().await?;
    let returned = output
        .activities
        .iter()
        .find_map(|activity| match &activity.event {
            lash::TurnEvent::CodeBlockReturned { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the cell returned");
    let completed = output
        .activities
        .iter()
        .find_map(|activity| match &activity.event {
            lash::TurnEvent::CodeBlockCompleted { result, .. } => Some(result.clone()),
            _ => None,
        })
        .expect("the cell completion committed");
    assert_eq!(returned, completed);
    assert!(
        matches!(&returned, lash_core::CellOutcome::Failed(failure) if failure.exec_failure == Some(lash_core::ExecCodeFailureReason::RuntimeStopped)),
        "{returned:?}"
    );
    drop(session);
    core.shutdown().await?;
    let reopened = build()?;
    let transcript = reopened.session(id).durable().await?.transcript().await?;
    let saved = transcript
        .entries()
        .iter()
        .find_map(|entry| match &entry.item {
            lash::transcript::TranscriptItem::Cell(cell) => Some(cell.result.clone()),
            _ => None,
        })
        .expect("the reopened cell is retained");
    assert_eq!(saved, completed);
    reopened.shutdown().await?;
    Ok(())
}
