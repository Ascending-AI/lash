//! A turn that aborts leaves its effect groups live for its redrive.
//!
//! The law runs a real standard-protocol turn over a SQLite memory backend.
//! Its tool call runs as an effect-group child, and the child resolves its
//! recorded execution environment through a store that is timing out. That is
//! a live fault, so the turn aborts and records nothing. The turn's end runs
//! anyway, as every exit of the effect loop does. It must not close the
//! turn's groups: a close under `Cancel` would cancel-decide the child, and
//! the redrive would then serve that cancellation as the child's recorded
//! outcome. The group stays live, read through the effect interface, for the
//! redrive to run on. Whether an engine then re-runs the child is that
//! engine's law (`tool_child_live_fault_tests!`).

use super::*;

const SESSION: &str = "aborted-turn-groups";
const TURN: &str = "aborted-turn-groups-turn";

/// `probe`, counting how many times its body ran.
struct ProbeTool {
    executions: Arc<AtomicUsize>,
}

fn probe_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:probe",
        "probe",
        "Aborted-turn probe tool.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
}

#[async_trait]
impl ToolProvider for ProbeTool {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "probe").then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "probed": true })) })
            .await
            .into()
    }
}

/// The backend's process-execution-env store, whose reads time out while
/// armed the way a pool that cannot acquire a connection does.
struct TimingOutEnvStore {
    inner: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    timing_out: std::sync::atomic::AtomicBool,
    failed_reads: AtomicUsize,
}

#[async_trait]
impl lash_core::ProcessExecutionEnvStore for TimingOutEnvStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner
            .publish_process_execution_env(owner, env_ref, bytes)
            .await
    }

    async fn transfer_process_execution_env(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner
            .transfer_process_execution_env(from, to, env_ref)
            .await
    }

    async fn release_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner
            .release_process_execution_env(owner, env_ref)
            .await
    }

    async fn retire_process_execution_env_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.inner.retire_process_execution_env_owner(owner).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<Option<Vec<u8>>, lash_core::PluginError> {
        if self.timing_out.load(Ordering::SeqCst) {
            self.failed_reads.fetch_add(1, Ordering::SeqCst);
            return Err(lash_core::PluginError::Session(
                "pool timed out while waiting for an open connection".to_string(),
            ));
        }
        self.inner.get_process_execution_env(env_ref).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_aborted_turn_leaves_its_groups_live_for_the_redrive() -> Result<()> {
    let backend = memory_backend().await;
    let backend_handle = Arc::clone(&backend);
    let env_store = Arc::new(TimingOutEnvStore {
        inner: lash_core::Backend::process_env_store(backend.as_ref()),
        timing_out: std::sync::atomic::AtomicBool::new(true),
        failed_reads: AtomicUsize::new(0),
    });
    let decorated = DecoratedBackend::over(backend).process_env_store({
        let env_store = Arc::clone(&env_store);
        move |_| env_store as Arc<dyn lash_core::ProcessExecutionEnvStore>
    });
    let provider_calls = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let provider = crate::testing::TestProvider::builder()
        .kind("aborted-turn-groups")
        .complete({
            let provider_calls = Arc::clone(&provider_calls);
            let requests = Arc::clone(&requests);
            move |request| {
                let call = provider_calls.fetch_add(1, Ordering::SeqCst);
                requests
                    .lock_recover()
                    .push(serde_json::to_string(&request.messages).unwrap_or_default());
                async move {
                    Ok(if call == 0 {
                        LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "probe-call".into(),
                                tool_name: "probe".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        }
                    } else {
                        LlmResponse {
                            parts: vec![LlmOutputPart::Text {
                                text: "done".to_string(),
                                response_meta: None,
                            }],
                            ..LlmResponse::default()
                        }
                    })
                }
            }
        })
        .build()
        .into_handle();
    let executions = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        Arc::new(decorated),
        crate::TurnBudget::Unbounded,
    ))
    .provider(provider)
    .model(mock_model_spec())
    .tools(Arc::new(ProbeTool {
        executions: Arc::clone(&executions),
    }))
    .build(crate::testing::runtime_lease_owner())?;

    let session = core.session(SESSION).open().await?;
    let aborted = session
        .turn(TurnInput::text("call the probe"))
        .turn_id(TURN)
        .run()
        .await
        .expect_err("a store fault in the tool child aborts the turn");
    let EmbedError::Runtime(runtime_error) = &aborted else {
        panic!("the abort is the typed runtime error: {aborted:?}");
    };
    assert_eq!(
        runtime_error.turn_failure_cause(),
        lash_core::TurnFailureCause::LiveFault,
        "the turn aborts on the live fault itself: {runtime_error}"
    );
    assert!(
        env_store.failed_reads.load(Ordering::SeqCst) > 0,
        "the precondition: the child read its environment through the timing-out store"
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "no attempt ran under an environment the child could not resolve"
    );

    // The turn's end closed nothing: the group its tool call formed is still
    // live under the turn's scope, not closing and not settled.
    let closing = lash_core::Backend::effect_host(backend_handle.as_ref())
        .effect_group_closing()
        .expect("a journaling host exposes its closing seam");
    let groups = closing
        .read_unsettled_groups(&lash_core::ExecutionScope::turn(SESSION, TURN))
        .await
        .expect("read the turn's unsettled groups");
    assert!(
        !groups.is_empty() && groups.iter().all(|group| !group.closing),
        "the aborted turn's groups stay live for its redrive: {groups:?}"
    );
    Ok(())
}
