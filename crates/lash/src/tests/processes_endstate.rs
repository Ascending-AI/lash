use super::*;

use lashlang::testing::ast_builders as b;

use lash_core::{
    ProcessEngine as _, ProcessEventLogTestSupport as _, ProcessQuery as _, ProcessRetention as _,
};
use lash_sansio::ProcessId;
use lash_sansio::sync::MutexExt;
use programs::{child_join_process, wait_signal_process};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use event_pages::full_events;

struct FailOnceReleaseEnvStore {
    inner: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    release_failures: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for FailOnceReleaseEnvStore {
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
        if self
            .release_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(lash_core::PluginError::Session(
                "injected process environment release failure".to_string(),
            ));
        }
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
        self.inner.get_process_execution_env(env_ref).await
    }
}

#[derive(Default)]
struct PruneEngineState {
    owners: HashSet<lash_core::ArtifactOwner>,
    bytes_present: bool,
}

struct FailOnceReleaseEngine {
    state: std::sync::Mutex<PruneEngineState>,
    release_failures: std::sync::atomic::AtomicUsize,
}

impl FailOnceReleaseEngine {
    fn retain(&self, owner: lash_core::ArtifactOwner) {
        let mut state = self.state.lock_recover();
        state.bytes_present = true;
        state.owners.insert(owner);
    }

    fn snapshot(&self) -> (bool, HashSet<lash_core::ArtifactOwner>) {
        let state = self.state.lock_recover();
        (state.bytes_present, state.owners.clone())
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessEngine for FailOnceReleaseEngine {
    fn kind(&self) -> &'static str {
        "fig677-prune-engine"
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> std::result::Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        unreachable!("the prune recovery fixture never runs a process")
    }

    async fn release_artifacts(
        &self,
        owner: &lash_core::ArtifactOwner,
        _payload: &serde_json::Value,
    ) -> std::result::Result<(), lash_core::PluginError> {
        if self
            .release_failures
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(lash_core::PluginError::Session(
                "injected process engine release failure".to_string(),
            ));
        }
        let mut state = self.state.lock_recover();
        state.owners.remove(owner);
        if state.owners.is_empty() {
            state.bytes_present = false;
        }
        Ok(())
    }

    async fn retire_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let mut state = self.state.lock_recover();
        state.owners.remove(owner);
        if state.owners.is_empty() {
            state.bytes_present = false;
        }
        Ok(())
    }
}

struct LinkedTestProcess {
    module_ref: lashlang::ModuleRef,
    host_requirements_ref: lashlang::HostRequirementsRef,
    process_ref: lashlang::ProcessRef,
    process_name: String,
    signal_event_types: Vec<lash_core::ProcessEventType>,
}

impl LinkedTestProcess {
    async fn new(
        artifact_store: &lash_lashlang_runtime::LashlangArtifacts,
        program: lashlang::Program,
        process_name: &str,
    ) -> Self {
        Self::new_with_catalog(
            artifact_store,
            program,
            process_name,
            programs::process_control_catalog(),
        )
        .await
    }

    fn process_input(&self) -> lash_core::ProcessInput {
        lash_lashlang_runtime::LashlangProcessInput {
            module_ref: self.module_ref.clone(),
            process_ref: self.process_ref.clone(),
            host_requirements_ref: self.host_requirements_ref.clone(),
            process_name: self.process_name.clone(),
            args: serde_json::Map::new(),
        }
        .into_process_input()
        .expect("lashlang process input serializes")
    }

    fn process_identity(&self) -> lash_core::ProcessIdentity {
        lash_lashlang_runtime::LashlangProcessInput {
            module_ref: self.module_ref.clone(),
            process_ref: self.process_ref.clone(),
            host_requirements_ref: self.host_requirements_ref.clone(),
            process_name: self.process_name.clone(),
            args: serde_json::Map::new(),
        }
        .process_identity()
    }

    /// A host start of this process, keyed by `start_key` so a replay of the
    /// same request answers the process the key minted (ADR 0107).
    fn start_request(&self, start_key: &str) -> lash_core::ProcessStartRequest {
        lash_core::ProcessStartRequest::new(
            self.process_input(),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_start_key(Some(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            start_key,
        )))
        .with_env_spec(process_env_spec())
        .with_extra_event_types(
            lash_lashlang_runtime::lashlang_process_event_types()
                .into_iter()
                .chain(self.signal_event_types.clone()),
        )
    }

    fn trigger_draft(
        &self,
        source_type: &str,
        source_key: String,
        env_ref: lash_core::ProcessExecutionEnvRef,
    ) -> lash_core::TriggerSubscriptionDraft {
        lash_core::TriggerSubscriptionDraft {
            source_capture: lash_core::TriggerSourceCapture::provider(
                ["ui", "button"],
                lash_core::LashSchema::any(),
                "ui-provider",
                serde_json::json!({"account": "a"}),
            ),
            subscription_key: "host-owned-test-trigger".to_string(),
            env_ref,
            wake_target: None,
            name: Some("host-owned-test-trigger".to_string()),
            source_type: source_type.to_string(),
            source_key,
            source: serde_json::json!({}),
            payload_schema: lash_core::LashSchema::any(),
            target: self.process_input(),
            target_identity: self.process_identity(),
            event_types: lash_lashlang_runtime::lashlang_process_event_types()
                .into_iter()
                .chain(self.signal_event_types.clone())
                .collect(),
            input_template: BTreeMap::new(),
            target_label: Some(self.process_name.clone()),
        }
    }
}

fn process_env_spec() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: mock_model_spec(),
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    )
}

async fn persist_process_env_ref(
    process_env_store: &dyn lash_core::ProcessExecutionEnvStore,
) -> lash_core::ProcessExecutionEnvRef {
    let spec = process_env_spec();
    let env_ref = spec.stable_ref().expect("stable process env ref");
    let bytes = spec.to_store_bytes().expect("encode process env spec");
    process_env_store
        .publish_process_execution_env(
            &lash_core::ArtifactOwner::host("process-env-test"),
            &env_ref,
            &bytes,
        )
        .await
        .expect("store process execution env");
    env_ref
}

fn signal_request(
    process_id: &ProcessId,
    signal_name: &str,
    signal_id: &str,
    payload: serde_json::Value,
) -> lash_core::ProcessEventAppendRequest {
    let event_type = lash_core::facade_support::process_signal_event_type(signal_name)
        .expect("signal event type");
    lash_core::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(
        lash_core::facade_support::process_signal_wait_key(process_id, signal_name, signal_id),
    )
}

async fn wait_for_process(
    core: &LashCore,
    process_id: &ProcessId,
    label: &str,
    matches: impl Fn(&lash_core::facade_support::ObservedProcess) -> bool,
) -> lash_core::facade_support::ObservedProcess {
    let process = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(process) = core.processes().get(process_id).await.expect("get process")
                && matches(&process)
            {
                return process;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
    assert!(matches(&process), "returned process did not match {label}");
    process
}

async fn wait_for_waiting_signal(
    core: &LashCore,
    process_id: &ProcessId,
    signal_name: &str,
) -> lash_core::facade_support::ObservedProcess {
    wait_for_process(core, process_id, "process signal wait", |process| {
        matches!(
            process.wait.as_ref().map(|wait| &wait.kind),
            Some(lash_core::WaitKind::Signal { name, .. }) if name == signal_name
        )
    })
    .await
}

async fn wait_for_terminal(
    core: &LashCore,
    process_id: &ProcessId,
    status: lash_core::ProcessStatus,
) -> lash_core::facade_support::ObservedProcess {
    wait_for_process(core, process_id, "terminal process", |process| {
        process.lifecycle == status
    })
    .await
}

/// Contributes one process engine to a facade host, the way a plugin does.
struct EnginePlugin(Arc<dyn lash_core::ProcessEngine>);

impl crate::plugins::PluginFactory for EnginePlugin {
    fn id(&self) -> &'static str {
        "test-process-engine"
    }

    fn process_engine_contributions(
        &self,
        _context: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError>
    {
        Ok(vec![lash_core::ProcessEngineRegistration::accepting(
            Arc::clone(&self.0),
        )])
    }

    fn build(
        &self,
        _context: &crate::plugins::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn crate::plugins::SessionPlugin>, lash_core::PluginError> {
        Ok(Arc::new(EngineSessionPlugin))
    }
}

struct EngineSessionPlugin;

impl crate::plugins::SessionPlugin for EngineSessionPlugin {
    fn id(&self) -> &'static str {
        "test-process-engine"
    }

    fn register(
        &self,
        _registrar: &mut crate::plugins::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

fn process_test_core(backend: lash_core::Backend) -> Result<LashCore> {
    let core = process_test_builder(backend).build(crate::testing::runtime_lease_owner())?;
    serve_processes(&core);
    Ok(core)
}

fn process_test_builder(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    LashCore::rlm_builder(
        backend,
        crate::TurnBudget::Unbounded,
        factory,
    )
    .session_spec(
        crate::SessionSpec::new()
            .provider_id(provider_id)
            .turn_budget(crate::TurnBudget::Unbounded),
    )
    .provider(provider)
    .model(mock_model_spec())
    .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
    // ADR 0095: `processes` is catalogue presence, so the fixtures need this.
    .plugin(Arc::new(
        lash_plugin_process_controls::SessionProcessAdminPluginFactory::new(lash_core::lifetime::session_or_starter),
    ))
}

/// A core over `backend` whose process-env store is `env_store` (a
/// decoration of the backend's own) and which runs `engine`.
fn prune_recovery_core(
    backend: lash_core::Backend,
    env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    engine: Arc<FailOnceReleaseEngine>,
) -> Result<LashCore> {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    let backend = DecoratedBackend::over(backend).process_env_store(move |_| env_store);
    LashCore::standard_builder(backend.into(), crate::TurnBudget::Unbounded)
        .session_spec(
            crate::SessionSpec::new()
                .provider_id(provider_id)
                .turn_budget(crate::TurnBudget::Unbounded),
        )
        .provider(provider)
        .model(mock_model_spec())
        .commit_budget(lash_core::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash_core::QueuedWorkBatchingConfig::new(1))
        .plugin(Arc::new(EnginePlugin(
            engine as Arc<dyn lash_core::ProcessEngine>,
        )))
        .build(crate::testing::runtime_lease_owner())
}

async fn process_prune_recovery_case(failing_store: &str) -> Result<()> {
    let dir = tempfile::tempdir().expect("process prune recovery tempdir");
    let backend = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(dir.path())
            .await
            .expect("open the process prune backend"),
    );
    let registry = backend.process_registry();
    let env_store = Arc::new(FailOnceReleaseEnvStore {
        inner: backend.process_env_store(),
        release_failures: std::sync::atomic::AtomicUsize::new(usize::from(
            failing_store == "environment",
        )),
    });
    let engine = Arc::new(FailOnceReleaseEngine {
        state: std::sync::Mutex::new(PruneEngineState::default()),
        release_failures: std::sync::atomic::AtomicUsize::new(usize::from(
            failing_store == "engine",
        )),
    });
    let shared_owner = lash_core::ArtifactOwner::host(format!("shared-{failing_store}"));
    let env_spec = process_env_spec();
    let env_ref = env_spec.stable_ref().expect("stable environment ref");
    let env_bytes = env_spec.to_store_bytes().expect("environment bytes");
    env_store
        .publish_process_execution_env(&shared_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(shared_owner.clone());
    let registered = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "shared-bytes"}),
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    let process_owner = lash_core::ArtifactOwner::process(registered.id.clone());
    env_store
        .publish_process_execution_env(&process_owner, &env_ref, &env_bytes)
        .await?;
    engine.retain(process_owner.clone());
    registry
        .complete_process(
            &registered.id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key(registered.id.to_string()),
        )
        .await?;

    let double =
        lash_restate_test::backend_with(0x3861_0101, lash_restate_test::ServerConfig::default(), {
            let backend = Arc::clone(&backend);
            move |_| backend
        })
        .await
        .expect("serve the process store with Restate");
    let core = prune_recovery_core(
        double.lash_backend(),
        env_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    assert!(Arc::ptr_eq(
        &(env_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>),
        &core.env.core.durability.process_env_store,
    ));
    let first_prune = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await;
    let pending_after_first = registry.pending_process_artifact_cleanup().await?;
    assert!(
        first_prune.is_err(),
        "selected artifact release fails after durable row prune; result={first_prune:?}, pending={pending_after_first:?}, env_failures={}, engine_failures={}",
        env_store
            .release_failures
            .load(std::sync::atomic::Ordering::SeqCst),
        engine
            .release_failures
            .load(std::sync::atomic::Ordering::SeqCst),
    );
    assert!(matches!(
        registry.get_process(&registered.id).await,
        Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert_eq!(pending_after_first.len(), 1);
    drop(core);
    drop(registry);
    drop(double);

    let reopened_backend = Arc::new(
        backend
            .reopen()
            .await
            .expect("reopen the backend after release failure"),
    );
    let reopened = reopened_backend.process_registry();
    let reopened_double = lash_restate_test::backend_with(
        0x3861_0102,
        lash_restate_test::ServerConfig::default(),
        move |_| reopened_backend,
    )
    .await
    .expect("serve the reopened process store with Restate");
    let recovered_core = prune_recovery_core(
        reopened_double.lash_backend(),
        env_store.clone() as Arc<dyn lash_core::ProcessExecutionEnvStore>,
        Arc::clone(&engine),
    )?;
    recovered_core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert!(
        reopened
            .pending_process_artifact_cleanup()
            .await?
            .is_empty()
    );
    assert!(
        env_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_some()
    );
    let (engine_bytes, engine_owners) = engine.snapshot();
    assert!(engine_bytes);
    assert_eq!(engine_owners, HashSet::from([shared_owner.clone()]));
    env_store
        .release_process_execution_env(&shared_owner, &env_ref)
        .await?;
    engine
        .release_artifacts(&shared_owner, &serde_json::Value::Null)
        .await?;
    assert!(
        env_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_none()
    );
    assert_eq!(engine.snapshot(), (false, HashSet::new()));
    Ok(())
}

#[tokio::test]
async fn process_prune_retries_each_artifact_release_after_registry_reopen() -> Result<()> {
    process_prune_recovery_case("environment").await?;
    process_prune_recovery_case("engine").await
}

#[tokio::test]
async fn process_prune_waits_for_process_scoped_turn_cancel_closure() -> Result<()> {
    let backend = double_backend().await;
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let core = process_test_core(backend.clone())?;
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                lash_core::ProcessIdentity::new("test"),
            )),
        )
        .await?
        .id;
    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await?;

    let session_id = lash_core::SessionId::from("process-prune-closure-session");
    let factory = &core.store_factory;
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await?;
    let lease = store
        .try_claim_session_execution_lease(
            &session_id,
            &lash_core::LeaseOwnerIdentity::opaque(
                "process-prune-closure-owner",
                "process-prune-closure-owner:incarnation",
            ),
            "process-prune-closure-executor",
            60_000,
        )
        .await?
        .acquired()
        .expect("fresh session lane is available");
    // The backend's effect host owns the turn-control promises.
    let effect_host = backend.effect_host();
    let authority = lash_core::TurnCancellationAuthority::new(
        effect_host.turn_control_binding_id(),
        effect_host,
    );
    let physical_scope = lash_core::ExecutionScope::process(process_id.clone());
    let binding_id = lash_core::facade_support::turn_control_binding_id_for_scope(
        authority.binding_id(),
        &physical_scope,
    )?;
    store
        .validate_turn_cancellation_binding(
            &session_id,
            &lease.fence(),
            &binding_id,
            &physical_scope,
        )
        .await?;
    let turn_id = lash_core::TurnId::from("process-prune-closure-turn");
    let address = lash_core::facade_support::TurnAddress::new(&session_id, &turn_id);
    let resolver = authority.resolver();
    let cancel_key = resolver
        .await_event_key(
            &address.execution_scope(),
            lash_core::AwaitEventWaitIdentity::TurnCancelGate,
        )
        .await?;
    let escalation_key = resolver
        .await_event_key(
            &address.execution_scope(),
            lash_core::AwaitEventWaitIdentity::TurnCancelEscalation,
        )
        .await?;
    let terminal_key = resolver
        .await_event_key(
            &address.execution_scope(),
            lash_core::AwaitEventWaitIdentity::TurnTerminal,
        )
        .await?;
    let authorization = lash_core::TurnCancelClosureAuthorization::new(
        address,
        binding_id,
        physical_scope,
        cancel_key,
        escalation_key,
        terminal_key,
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &lease.fence(),
    )?;
    store
        .authorize_turn_cancel_closure(&lease.fence(), &authorization)
        .await?;

    let refusal = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await
        .expect_err("a Process-scoped closure pins its process journal");
    assert!(matches!(
        refusal,
        crate::EmbedError::Store(lash_core::StoreError::TurnCancelClosureLifecyclePinned {
            ref session_id,
            pending_count: 1,
        }) if session_id == "process-prune-closure-session"
    ));
    assert!(
        registry.get_process(&process_id).await?.is_some(),
        "the refused prune retains the terminal process and its journal"
    );

    let settlement = authority.settle_authorized_closure(&authorization).await?;
    store
        .repair_orphaned_active_turn_inputs(
            &session_id,
            &lease.fence(),
            &turn_id,
            &lash_core::TurnCancelIntentSnapshot::Absent,
            Some(&settlement),
        )
        .await?
        .into_applied()
        .expect("the exact current owner consumes the closure authorization");
    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(report.pruned_processes, 1);

    let late_session_id = lash_core::SessionId::from("process-prune-closure-late-session");
    let late_store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: late_session_id.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await?;
    let late_lease = late_store
        .try_claim_session_execution_lease(
            &late_session_id,
            &lash_core::LeaseOwnerIdentity::opaque(
                "process-prune-closure-late-owner",
                "process-prune-closure-late-owner:incarnation",
            ),
            "process-prune-closure-late-executor",
            60_000,
        )
        .await?
        .acquired()
        .expect("fresh late session lane is available");
    let late_authority = authority.clone();
    let late_scope = lash_core::ExecutionScope::process(process_id.clone());
    let late_binding_id = lash_core::facade_support::turn_control_binding_id_for_scope(
        late_authority.binding_id(),
        &late_scope,
    )?;
    late_store
        .validate_turn_cancellation_binding(
            &late_session_id,
            &late_lease.fence(),
            &late_binding_id,
            &late_scope,
        )
        .await?;
    let late_address = lash_core::facade_support::TurnAddress::new(
        &late_session_id,
        lash_core::TurnId::from("process-prune-closure-late-turn"),
    );
    let late_resolver = late_authority.resolver();
    let late_authorization = lash_core::TurnCancelClosureAuthorization::new(
        late_address.clone(),
        late_binding_id,
        lash_core::ExecutionScope::process(process_id),
        late_resolver
            .await_event_key(
                &late_address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelGate,
            )
            .await?,
        late_resolver
            .await_event_key(
                &late_address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnCancelEscalation,
            )
            .await?,
        late_resolver
            .await_event_key(
                &late_address.execution_scope(),
                lash_core::AwaitEventWaitIdentity::TurnTerminal,
            )
            .await?,
        lash_core::TurnCancelClosureProposal::CompletionSealed,
        lash_core::TurnCancelIntentSnapshot::Absent,
        &late_lease.fence(),
    )?;
    let late_result = late_store
        .authorize_turn_cancel_closure(&late_lease.fence(), &late_authorization)
        .await;
    assert!(
        matches!(
            late_result,
            Err(lash_core::StoreError::TurnCancelClosureScopeRetired { .. })
        ),
        "a closure authorized under a pruned process scope is refused: {late_result:?}"
    );
    assert!(
        late_store
            .pending_turn_cancel_closure_pins()
            .await?
            .is_empty()
    );

    Ok(())
}

#[tokio::test]
async fn sqlite_facade_prune_removes_tombstoned_process_delivery() -> Result<()> {
    let dir = tempfile::tempdir().expect("sqlite facade prune tempdir");
    let backend = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(dir.path())
            .await
            .expect("open the SQLite facade prune backend"),
    );
    let trigger_store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let double =
        lash_restate_test::backend_with(0x3861_0103, lash_restate_test::ServerConfig::default(), {
            let backend = Arc::clone(&backend);
            move |_| backend
        })
        .await
        .expect("serve the SQLite facade store with Restate");
    let core = process_test_core(double.lash_backend())?;

    let session_id = "sqlite-facade-prune-session";
    let source_key = "sqlite-facade-prune-source";
    let mut input_template = BTreeMap::new();
    input_template.insert("event".to_string(), lash_core::TriggerInputBinding::Event);
    trigger_store
        .execute_command(
            "sqlite-facade-prune-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::session(session_id),
                actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                    session_id,
                )),
                draft: lash_core::TriggerSubscriptionDraft {
                    source_capture: lash_core::TriggerSourceCapture::provider(
                        ["ui", "button"],
                        lash_core::LashSchema::any(),
                        "ui-provider",
                        serde_json::json!({"account": "a"}),
                    ),
                    subscription_key: "sqlite-facade-prune-key".to_string(),
                    env_ref: lash_core::ProcessExecutionEnvRef::new(
                        "process-env:sqlite-facade-prune",
                    ),
                    wake_target: Some(lash_core::SessionScope::new(session_id)),
                    name: Some("worker".to_string()),
                    source_type: "ui.button.pressed".to_string(),
                    source_key: source_key.to_string(),
                    source: serde_json::json!({ "button": "Blue" }),
                    payload_schema: lash_core::LashSchema::any(),
                    target: lash_core::ProcessInput::Engine {
                        kind: "test".to_string(),
                        payload: serde_json::json!({ "process": "worker" }),
                    },
                    target_identity: lash_core::ProcessIdentity::new("test"),
                    event_types: Vec::new(),
                    input_template,
                    target_label: Some("worker".to_string()),
                },
            },
        )
        .await?
        .expect("register trigger");
    let ingress = trigger_store
        .ingest_occurrence(lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({ "button": "Blue" }),
            "sqlite-facade-prune-occurrence",
        ))
        .await?;
    assert_eq!(ingress.reservations.len(), 1);
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                lash_core::ProcessIdentity::new("test"),
            ))
            .with_start_key(Some(
                lash_core::facade_support::trigger_delivery_start_key(&ingress.reservations[0]),
            )),
        )
        .await?
        .id;
    // The router binds the delivery to the process its start minted.
    trigger_store
        .bind_delivery_process(
            &ingress.reservations[0].occurrence.occurrence_id,
            &ingress.reservations[0].subscription.subscription_id,
            &process_id,
        )
        .await?;
    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await?;

    // A retention filter carrying the `ProcessListFilter` default status selects
    // `running`, which no prunable row can hold. Refusing it is what keeps a
    // scoped retention call from reporting a silent zero (ADR 0023). The
    // refusal is scoped to the two live statuses, not to "non-terminal": a
    // caller-departed row is non-terminal and prunable, so retention policy can
    // name it (see `caller_departed_rows_are_selectable_retention_policy`).
    let refused = core
        .processes()
        .prune(
            u64::MAX,
            Some(&lash_core::ProcessListFilter {
                originator: Some(lash_core::ProcessOriginatorFilter::session("some-session")),
                ..lash_core::ProcessListFilter::default()
            }),
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect_err("a live retention filter must be refused");
    assert!(
        refused
            .to_string()
            .contains("live status set `In({Running})`"),
        "unexpected refusal: {refused}"
    );
    assert!(
        registry.get_process(&process_id).await?.is_some(),
        "a refused retention filter must not have deleted anything"
    );

    let report = core
        .processes()
        .prune(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(report.pruned_processes, 1);
    assert_eq!(report.pruned_trigger_deliveries, 1);
    assert!(
        trigger_store
            .list_deliveries_by_process_id(&process_id)
            .await?
            .is_empty(),
        "the facade coordinates delivery cleanup through the trigger store"
    );
    assert!(
        registry
            .filter_unregistered_process_ids(std::slice::from_ref(&process_id))
            .await?
            .is_empty(),
        "the tombstoned process is not a recovery candidate"
    );

    let orphaned = trigger_store
        .ingest_occurrence(lash_core::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            source_key,
            serde_json::json!({ "button": "Blue" }),
            "sqlite-facade-compact-occurrence",
        ))
        .await?;
    assert_eq!(orphaned.reservations.len(), 1);
    let orphaned_process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_admitted_identity(lash_core::AdmittedProcessIdentity::for_testing(
                lash_core::ProcessIdentity::new("test"),
            ))
            .with_start_key(Some(
                lash_core::facade_support::trigger_delivery_start_key(&orphaned.reservations[0]),
            )),
        )
        .await?
        .id;
    trigger_store
        .bind_delivery_process(
            &orphaned.reservations[0].occurrence.occurrence_id,
            &orphaned.reservations[0].subscription.subscription_id,
            &orphaned_process_id,
        )
        .await?;
    registry
        .complete_process(
            &orphaned_process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!("done"),
            )),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await?;
    registry
        .prune_terminal_processes(u64::MAX, None, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert_eq!(
        trigger_store
            .list_deliveries_by_process_id(&orphaned_process_id)
            .await?
            .len(),
        1,
        "the raw process prune leaves a crash-window delivery for reconciliation"
    );

    let compacted = core
        .processes()
        .compact_tombstones(u64::MAX, lash_core::ProjectionWatermark::NoProjector)
        .await?;
    assert!(
        compacted >= 1,
        "the facade may compact unrelated, already-reconciled tombstones"
    );
    assert!(
        trigger_store
            .list_deliveries_by_process_id(&orphaned_process_id)
            .await?
            .is_empty(),
        "facade compaction reconciles the delivery before removing its tombstone"
    );
    assert!(
        matches!(
            registry.get_process(&orphaned_process_id).await,
            Ok(None) | Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the reconciled tombstone is compacted and cannot become recoverable"
    );
    Ok(())
}

#[tokio::test]
async fn host_owned_processes_run_without_application_session() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let trigger_store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let process_env_store = backend.process_env_store();
    let core = process_test_core(backend.clone())?;
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: any } {
        //   value = wait_signal("ready")
        //   finish value
        // }
        wait_signal_process(lashlang::TypeExpr::Any, b::var("value")),
        "main",
    )
    .await;

    let start_request = process.start_request("sessionless-direct");
    let started = core
        .processes()
        .start(
            start_request.clone(),
            runtime_operation_scope(&core, "sessionless-direct-start").await,
        )
        .await?;
    assert_eq!(
        started.disposition,
        lash_core::ProcessRegistrationDisposition::Created,
        "the first start under its key creates the process"
    );
    assert!(
        started.start_key.is_some(),
        "the receipt names the start's key"
    );
    let sessionless_direct_id = started.process_id.clone();
    let retried = core
        .processes()
        .start(
            start_request,
            runtime_operation_scope(&core, "sessionless-direct-start-replay").await,
        )
        .await
        .expect("public start replay discovers process ownership after staging retirement");
    assert_eq!(
        retried,
        lash_core::ProcessStartReceipt {
            disposition: lash_core::ProcessRegistrationDisposition::Existing,
            ..started
        },
        "a retry under the same key answers the same process, found not created"
    );
    let waiting = wait_for_waiting_signal(&core, &sessionless_direct_id, "ready").await;
    assert!(matches!(
        waiting.originator,
        lash_core::ProcessOriginator::Host { .. }
    ));
    let waiting_events = full_events(&core, &sessionless_direct_id).await?;
    assert!(
        waiting_events
            .iter()
            .any(|event| event.event_type == "process.waiting")
    );

    let cancelled = core
        .processes()
        .cancel(
            &sessionless_direct_id,
            runtime_operation_scope(&core, "sessionless-direct-cancel").await,
        )
        .await?;
    assert_eq!(cancelled.status, lash_core::ProcessStatus::Waiting);
    wait_for_terminal(
        &core,
        &sessionless_direct_id,
        lash_core::ProcessStatus::Cancelled,
    )
    .await;

    let source_type = "timer.tick";
    let source_key =
        lash_core::facade_support::default_trigger_source_key(source_type, &serde_json::json!({}));
    let env_ref = persist_process_env_ref(process_env_store.as_ref()).await;
    trigger_store
        .execute_command(
            "sessionless-trigger-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("processes-endstate")?,
                actor: lash_core::ProcessOriginator::host_scoped("processes-endstate"),
                draft: process.trigger_draft(source_type, source_key.clone(), env_ref),
            },
        )
        .await?
        .map_err(|err| lash_core::PluginError::Session(err.to_string()))?;
    let report = core
        .triggers()
        .emit(
            lash_core::TriggerOccurrenceRequest::new(
                source_type,
                source_key,
                serde_json::json!({ "at": "2026-06-10T12:00:00Z" }),
                "sessionless-trigger-1",
            )
            .with_source(serde_json::json!({})),
            runtime_operation_scope(&core, "sessionless-trigger").await,
        )
        .await?;
    let started_process_ids = report.started_process_ids();
    assert_eq!(started_process_ids.len(), 1);
    let triggered_process_id = &started_process_ids[0];
    let triggered = wait_for_waiting_signal(&core, triggered_process_id, "ready").await;
    assert!(matches!(
        triggered.originator,
        lash_core::ProcessOriginator::Host { .. }
    ));
    let event = core
        .processes()
        .signal(
            triggered_process_id,
            "ready",
            "host-signal-1",
            signal_request(
                triggered_process_id,
                "ready",
                "host-signal-1",
                serde_json::json!({ "ok": true }),
            ),
            runtime_operation_scope(&core, "sessionless-host-signal").await,
        )
        .await?;
    assert_eq!(event.event_type, "signal.ready");
    let output = core.processes().await_output(triggered_process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("triggered process did not succeed: {output:#?}");
    };
    assert_eq!(value.to_json_value(), serde_json::json!({ "ok": true }));
    let signal_events = registry.full_event_window(triggered_process_id, 0).await?;
    assert!(
        signal_events
            .iter()
            .any(|event| event.event_type == "signal.ready")
    );
    Ok(())
}

#[tokio::test]
async fn session_trigger_process_visibility_conformance() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let trigger_store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let core = process_test_core(backend.clone())?;
    let env_ref =
        persist_process_env_ref(core.env.core.durability.process_env_store.as_ref()).await;
    let session_id = "session-trigger-visibility";
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: any } {
        //   value = wait_signal("ready")
        //   finish value
        // }
        wait_signal_process(lashlang::TypeExpr::Any, b::var("value")),
        "main",
    )
    .await;
    let source_type = "ui.button.pressed";
    let source_key = lash_core::facade_support::default_trigger_source_key(
        source_type,
        &serde_json::json!({ "button": "Blue" }),
    );
    let mut draft = process.trigger_draft(source_type, source_key.clone(), env_ref);
    draft.subscription_key = "session-trigger-visibility".to_string();
    draft.name = Some("session trigger visibility".to_string());
    trigger_store
        .execute_command(
            "session-trigger-visibility-register",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::session(session_id),
                actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                    session_id,
                )),
                draft,
            },
        )
        .await?
        .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;

    let session = core.session(session_id).open().await?;
    let lifecycle_cursor = session.observe().current_observation().cursor;

    let report = core
        .triggers()
        .emit(
            lash_core::TriggerOccurrenceRequest::new(
                source_type,
                source_key,
                serde_json::json!({ "button": "Blue" }),
                "session-trigger-visibility-occurrence",
            )
            .with_source(serde_json::json!({ "button": "Blue" })),
            runtime_operation_scope(&core, "session-trigger-visibility-emit").await,
        )
        .await?;
    let started_process_ids = report.started_process_ids();
    assert_eq!(started_process_ids.len(), 1);
    let process_id = &started_process_ids[0];
    wait_for_waiting_signal(&core, process_id, "ready").await;
    core.processes()
        .signal(
            process_id,
            "ready",
            "session-trigger-visibility-signal",
            signal_request(
                process_id,
                "ready",
                "session-trigger-visibility-signal",
                serde_json::json!({ "delivered": true }),
            ),
            runtime_operation_scope(&core, "session-trigger-visibility-signal").await,
        )
        .await?;
    let output = core.processes().await_output(process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("session trigger process did not succeed: {output:#?}");
    };
    assert_eq!(
        value.to_json_value(),
        serde_json::json!({ "delivered": true })
    );
    let events = registry.full_event_window(process_id, 0).await?;

    let observed = session.admin().processes().list_all().await?;
    let process = observed
        .iter()
        .find(|process| process.process_id == *process_id)
        .unwrap_or_else(|| {
            panic!(
                "the registering session must observe trigger delivery {process_id}; observed={observed:?}"
            )
        });
    assert_eq!(process.lifecycle, lash_core::ProcessStatus::Completed);
    let observers = registry.observers_for_process(process_id).await?;
    assert!(
        observers.iter().any(|observer| observer == session_id),
        "completed trigger delivery must retain the registering session edge; observers={observers:?}"
    );
    let SessionResume::Replayed {
        events: session_events,
    } = session.observe().resume_from_cursor(&lifecycle_cursor)?
    else {
        panic!("lifecycle should replay on the session stream")
    };
    lifecycle_observation::assert_working_process_lifecycle(&events, process_id, &session_events);
    Ok(())
}

#[tokio::test]
async fn signal_validation_rejects_undeclared_names_and_mistyped_payloads() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let core = process_test_core(backend.clone())?;
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: string } {
        //   value = wait_signal("ready")
        //   finish value
        // }
        wait_signal_process(lashlang::TypeExpr::Str, b::var("value")),
        "main",
    )
    .await;
    let process_id = "signal-validation";

    let process_id = core
        .processes()
        .start(
            process.start_request(process_id),
            runtime_operation_scope(&core, "signal-validation-start").await,
        )
        .await?
        .process_id;
    wait_for_waiting_signal(&core, &process_id, "ready").await;

    let undeclared = core
        .processes()
        .signal(
            &process_id,
            "nope",
            "undeclared-1",
            signal_request(&process_id, "nope", "undeclared-1", serde_json::json!("x")),
            runtime_operation_scope(&core, "signal-validation-undeclared").await,
        )
        .await;
    let undeclared_err = undeclared.expect_err("undeclared signal name must be rejected");
    assert!(
        undeclared_err.to_string().contains("undeclared"),
        "unexpected error: {undeclared_err}"
    );

    let mistyped = core
        .processes()
        .signal(
            &process_id,
            "ready",
            "mistyped-1",
            signal_request(
                &process_id,
                "ready",
                "mistyped-1",
                serde_json::json!({ "not": "a string" }),
            ),
            runtime_operation_scope(&core, "signal-validation-mistyped").await,
        )
        .await;
    assert!(
        mistyped.is_err(),
        "schema-invalid signal payload must be rejected"
    );

    // Both rejected sends left the process parked with nothing consumed.
    let still_waiting = wait_for_waiting_signal(&core, &process_id, "ready").await;
    assert_eq!(still_waiting.lifecycle, lash_core::ProcessStatus::Waiting);
    assert!(
        full_events(&core, &process_id)
            .await?
            .iter()
            .all(|event| event.event_type != "signal.ready" && event.event_type != "signal.nope")
    );

    core.processes()
        .signal(
            &process_id,
            "ready",
            "valid-1",
            signal_request(&process_id, "ready", "valid-1", serde_json::json!("done")),
            runtime_operation_scope(&core, "signal-validation-valid").await,
        )
        .await?;
    let output = core.processes().await_output(&process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed after valid signal: {output:#?}");
    };
    assert_eq!(value.to_json_value(), serde_json::json!("done"));
    Ok(())
}

#[tokio::test]
async fn repeated_waits_on_one_signal_consume_in_order() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let core = process_test_core(backend.clone())?;
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: any } {
        //   first = wait_signal("ready")
        //   second = wait_signal("ready")
        //   finish { first: first, second: second }
        // }
        b::module(
            vec![b::process_with_signals(
                "main",
                Vec::new(),
                vec![b::signal("ready", lashlang::TypeExpr::Any)],
                b::block(vec![
                    b::assign("first", b::wait_signal("ready")),
                    b::assign("second", b::wait_signal("ready")),
                    b::finish(b::record(vec![
                        ("first", b::var("first")),
                        ("second", b::var("second")),
                    ])),
                ]),
            )],
            Vec::new(),
        ),
        "main",
    )
    .await;
    let process_id = "repeated-waits";

    let process_id = core
        .processes()
        .start(
            process.start_request(process_id),
            runtime_operation_scope(&core, "repeated-waits-start").await,
        )
        .await?
        .process_id;

    let first_wait = wait_for_waiting_signal(&core, &process_id, "ready").await;
    let lash_core::WaitKind::Signal { ordinal, .. } =
        first_wait.wait.expect("first wait facet").kind;
    assert_eq!(ordinal, 1, "first wait must use ordinal 1");
    core.processes()
        .signal(
            &process_id,
            "ready",
            "order-1",
            signal_request(&process_id, "ready", "order-1", serde_json::json!(1)),
            runtime_operation_scope(&core, "repeated-waits-signal-1").await,
        )
        .await?;

    let second_wait = wait_for_process(&core, &process_id, "second signal wait", |process| {
        matches!(
            process.wait.as_ref().map(|wait| &wait.kind),
            Some(lash_core::WaitKind::Signal { ordinal, .. }) if *ordinal == 2
        )
    })
    .await;
    let lash_core::WaitKind::Signal {
        key: second_key, ..
    } = second_wait.wait.expect("second wait facet").kind;
    assert!(
        second_key.ends_with(":2"),
        "second wait key must carry ordinal 2: {second_key}"
    );
    core.processes()
        .signal(
            &process_id,
            "ready",
            "order-2",
            signal_request(&process_id, "ready", "order-2", serde_json::json!(2)),
            runtime_operation_scope(&core, "repeated-waits-signal-2").await,
        )
        .await?;

    let output = core.processes().await_output(&process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed: {output:#?}");
    };
    assert_eq!(
        value.to_json_value(),
        serde_json::json!({ "first": 1, "second": 2 })
    );

    // The suspension history is on the event log: two waits, two resumes.
    let events = full_events(&core, &process_id).await?;
    let waiting = events
        .iter()
        .filter(|event| event.event_type == "process.waiting")
        .count();
    let resumed = events
        .iter()
        .filter(|event| event.event_type == "process.resumed")
        .count();
    assert_eq!((waiting, resumed), (2, 2));
    Ok(())
}

#[tokio::test]
async fn process_starts_and_awaits_child_process() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let core = process_test_core(backend.clone())?;
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process child() { finish { from: "child" } }
        // process main() {
        //   handle = start child()
        //   value = await handle
        //   finish { joined: value }
        // }
        child_join_process(b::record(vec![("joined", b::var("value"))])),
        "main",
    )
    .await;
    let process_id = "parent-joins-child";

    let process_id = core
        .processes()
        .start(
            process.start_request(process_id),
            runtime_operation_scope(&core, "parent-joins-child-start").await,
        )
        .await?
        .process_id;
    let output = core.processes().await_output(&process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("parent process did not succeed: {output:#?}");
    };
    let value = value.to_json_value();
    // `await handle` yields the await envelope: success flag plus the child's
    // finish value.
    assert_eq!(
        value,
        serde_json::json!({ "joined": { "ok": true, "value": { "from": "child" } } })
    );

    // Both parent and child are globally addressable, completed, and the
    // child INHERITS its parent's provenance chain: Host originator, no wake
    // target, no grants — the ephemeral execution scope appears nowhere.
    let all = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::any_of([lash_core::ProcessStatus::Completed]),
            ..lash_core::ProcessListFilter::default()
        })
        .await?;
    assert_eq!(all.len(), 2, "parent and child should both be completed");
    assert!(
        all.iter().all(|process| matches!(
            process.originator,
            lash_core::ProcessOriginator::Host { .. }
        )),
        "children of a host chain stay host-originated"
    );
    assert!(
        all.iter().any(|process| process.process_id != process_id),
        "child process record"
    );
    let parent = registry
        .get_process(&process_id)
        .await?
        .expect("parent record");
    let child_id = &all
        .iter()
        .find(|record| record.process_id != process_id)
        .expect("child exists")
        .process_id;
    let child = registry.get_process(child_id).await?.expect("child record");
    assert!(
        !child.input.is_externally_owned(),
        "a body-started child is a process lash executes"
    );
    // Started by the parent's body; with no session above it, the host's
    // `session_or_starter` policy keeps it only until its starter.
    let parent_scope = lash_core::ScopeId::process(parent.id);
    assert_eq!(child.ancestry.starter(), Some(&parent_scope));
    assert_eq!(child.lifetime.scope(), Some(&parent_scope));
    Ok(())
}

#[tokio::test]
async fn process_children_inherit_session_chain_provenance() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let core = process_test_core(backend.clone())?;
    let session_id = "chain-session";
    let process_id = "chain-parent";
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process child() { finish { from: "child" } }
        // process main() {
        //   handle = start child()
        //   value = await handle
        //   finish value
        // }
        child_join_process(b::var("value")),
        "main",
    )
    .await;
    let session = core.session(session_id).open().await?;
    let process_id = session
        .admin()
        .processes()
        .start(
            {
                let mut request = process.start_request(process_id);
                request.originator =
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id));
                request
            }
            .with_wake_session_id(Some(SessionId::from(session_id.to_string())))
            .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "chain-parent-start").await,
        )
        .await?
        .process_id;
    wait_for_terminal(&core, &process_id, lash_core::ProcessStatus::Completed).await;

    // The child inherited the session originator and indexed wake target. Under
    // FIG-2346, session-originated descendants propagate the root-session
    // observer edge. Host-originated chains still mint no observer.
    let completed = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::any_of([lash_core::ProcessStatus::Completed]),
            ..lash_core::ProcessListFilter::default()
        })
        .await?;
    assert_eq!(completed.len(), 2);
    for observed in &completed {
        match &observed.originator {
            lash_core::ProcessOriginator::Session {
                session_id: originator_session_id,
                ..
            } => {
                assert_eq!(originator_session_id, session_id)
            }
            other => panic!("expected session originator, got {other:?}"),
        }
    }
    let snapshot = core.processes().session_snapshot(session_id).await?;
    assert_eq!(
        snapshot.items.len(),
        2,
        "the originating session observes both the parent and its descendant"
    );
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.process.process_id == process_id),
        "the explicitly observed parent remains visible"
    );
    assert!(
        snapshot
            .items
            .iter()
            .any(|item| item.process.process_id != process_id),
        "the session-originated child inherits the root-session observer edge"
    );
    Ok(())
}

#[tokio::test]
async fn process_outlives_deleted_session_and_resumes_from_host_signal() -> Result<()> {
    let backend = double_backend().await;
    let artifact_store = lash_lashlang_runtime::LashlangArtifacts::of_backend(&backend);
    let registry: Arc<dyn lash_core::ProcessRegistry> = backend.process_registry();
    let core = process_test_core(backend.clone())?;
    let session_id = "process-outlives-session";
    let process_id = "outliving-process";
    let process = LinkedTestProcess::new(
        &artifact_store,
        // process main() signals { ready: any } {
        //   value = wait_signal("ready")
        //   finish { resumed: value }
        // }
        wait_signal_process(
            lashlang::TypeExpr::Any,
            b::record(vec![("resumed", b::var("value"))]),
        ),
        "main",
    )
    .await;
    let session = core.session(session_id).open().await?;
    let process_id = session
        .admin()
        .processes()
        .start(
            process
                .start_request(process_id)
                .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "outliving-process-start").await,
        )
        .await?
        .process_id;
    wait_for_waiting_signal(&core, &process_id, "ready").await;
    drop(session);

    let report = delete_bound_session(&core, session_id).await?;
    let process_report = report.process.expect("process delete report");
    assert_eq!(process_report.removed_observer_count, 1);
    assert_eq!(process_report.discarded_wake_delivery_count, 0);
    assert!(
        core.processes()
            .session_snapshot(session_id)
            .await?
            .items
            .is_empty()
    );
    let still_waiting = wait_for_waiting_signal(&core, &process_id, "ready").await;
    assert!(still_waiting.env_ref.is_some());

    let wake_after_delete = registry
        .append_event(
            &process_id,
            lash_core::ProcessEventAppendRequest::new(
                "process.wake",
                serde_json::json!({ "text": "wake after deleted session" }),
            ),
        )
        .await?;
    assert_eq!(wake_after_delete.event.event_type, "process.wake");
    assert!(
        full_events(&core, &process_id)
            .await?
            .iter()
            .any(|event| event.payload["text"] == "wake after deleted session")
    );
    assert!(
        core.processes()
            .session_snapshot(session_id)
            .await?
            .items
            .is_empty()
    );

    core.processes()
        .signal(
            &process_id,
            "ready",
            "outliving-host-signal",
            signal_request(
                &process_id,
                "ready",
                "outliving-host-signal",
                serde_json::json!({ "after_delete": true }),
            ),
            runtime_operation_scope(&core, "outliving-process-signal").await,
        )
        .await?;
    let output = core.processes().await_output(&process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("outliving process did not succeed: {output:#?}");
    };
    let value = value.to_json_value();
    assert_eq!(
        value,
        serde_json::json!({ "resumed": { "after_delete": true } })
    );
    wait_for_terminal(&core, &process_id, lash_core::ProcessStatus::Completed).await;
    Ok(())
}

#[derive(Clone, Default)]
struct CollectingProcessEventSink {
    events: Arc<std::sync::Mutex<Vec<(String, u64)>>>,
}

impl CollectingProcessEventSink {
    fn collected(&self) -> Vec<(String, u64)> {
        self.events.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for CollectingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock_recover()
            .push((event.event_type.clone(), event.sequence));
    }
}

mod artifact_cleanup_round4;
mod caller_departure;
mod event_pages;
mod lifecycle_observation;
mod native_process_await;
mod programs;
