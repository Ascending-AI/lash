//! A protocol's code effect on the durable turn path: a protocol that
//! starts code in a session with no code executor stops the turn typed,
//! its driver handed the cause.

use super::*;
use crate::TurnInput;
use lash_core::facade_support::{TurnOutcome, TurnStop};
use lash_core::plugin::{ProtocolDriverPlugin, ProtocolSessionPlugin};

const PROTOCOL: &str = "test_protocol";

/// Every code failure the test protocol's driver was handed, encoded.
type Received = Arc<StdMutex<Vec<serde_json::Value>>>;

/// A protocol whose every turn starts one code cell, with no code
/// executor.
struct TestProtocolFactory(Received);

impl lash_core::facade_support::PluginFactory for TestProtocolFactory {
    fn id(&self) -> &'static str {
        PROTOCOL
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(TestProtocolPlugin(Arc::clone(&self.0))))
    }
}

impl lash_core::plugin::PluginDefinition for TestProtocolFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(PROTOCOL)
    }
}

struct TestProtocolPlugin(Received);

impl lash_core::facade_support::SessionPlugin for TestProtocolPlugin {
    fn id(&self) -> &'static str {
        PROTOCOL
    }

    fn register(
        &self,
        registrar: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        registrar
            .protocol()
            .session(Arc::new(TestProtocolSession))?;
        registrar
            .protocol()
            .protocol_driver(Arc::new(TestProtocolDriver(Arc::clone(&self.0))))?;
        Ok(())
    }
}

struct TestProtocolSession;

#[async_trait]
impl ProtocolSessionPlugin for TestProtocolSession {}

struct TestProtocolDriver(Received);

impl ProtocolDriverPlugin for TestProtocolDriver {
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

/// Starts one code cell, and records the failure it was handed and stops.
struct TestDriver(Received);

impl lash_sansio::ProtocolDriverHandle<lash_core::HostTurnProtocol> for TestDriver {
    fn prepare_protocol_iteration(
        &self,
        _ctx: lash_core::DriverContextView<'_>,
    ) -> Vec<lash_core::DriverAction> {
        vec![lash_core::DriverAction::Start(
            lash_core::sansio::PendingWork::Exec {
                language: "code".to_string(),
                code: "print('effect seam')".to_string(),
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
        if let Err(error) = result {
            self.0
                .lock_recover()
                .push(serde_json::to_value(&error).expect("an exec failure encodes"));
        }
        vec![lash_core::DriverAction::Finish(TurnOutcome::Stopped(
            TurnStop::RuntimeError,
        ))]
    }
}

/// A protocol that starts code in a session with no code executor stops
/// the turn `RuntimeError`, its driver handed the typed
/// `executor_unavailable` failure once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_exec_without_code_executor_stops_as_runtime_error() -> Result<()> {
    let received = Received::default();
    let core = explicit_ephemeral_facets(
        LashCore::builder(sqlite_memory_store_backend().await)
            .protocol_plugin(Arc::new(TestProtocolFactory(Arc::clone(&received)))),
    )
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse("protocol-effects").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let output = session.send(TurnInput::text("run code")).output().await?;
    assert!(
        matches!(
            output.result.outcome,
            TurnOutcome::Stopped(TurnStop::RuntimeError)
        ),
        "{:?}",
        output.result.outcome
    );
    assert_eq!(
        *received.lock_recover(),
        vec![serde_json::json!({
            "reason": "executor_unavailable",
            "message": "code execution is not available in this session",
        })],
        "the driver is handed the typed failure once"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}
