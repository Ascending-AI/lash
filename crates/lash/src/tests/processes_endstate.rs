use super::*;
use lash_core::{
    ProcessEngine as _, ProcessQuery as _, ProcessRetention as _, TestProcessRegistryWriteExt,
};
use lash_sansio::ProcessId;
use lash_sansio::sync::MutexExt;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

struct FailOnceReleaseEnvStore {
    inner: Arc<lash_core::InMemoryProcessExecutionEnvStore>,
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
        artifact_store: &dyn lash_lashlang_runtime::LashlangArtifactStore,
        source: &str,
        process_name: &str,
    ) -> Self {
        let linked = lashlang::LinkedModule::link(
            lashlang::parse(source).expect("parse lashlang process"),
            lashlang::LashlangHostEnvironment::new(
                lashlang::LashlangHostCatalog::new(),
                lashlang::LashlangAbilities::default()
                    .with_processes()
                    .with_sleep()
                    .with_process_signals(),
            ),
        )
        .expect("link lashlang process");
        artifact_store
            .publish_module_artifact(
                &lash_core::ArtifactOwner::host(format!("process-test:{process_name}")),
                &linked.artifact,
            )
            .await
            .expect("store lashlang process artifact");
        let process_ref = linked
            .artifact
            .process_ref(process_name)
            .unwrap_or_else(|| panic!("missing process ref `{process_name}`"))
            .clone();
        let signal_event_types = linked
            .artifact
            .canonical_ir
            .process(process_name)
            .map(lash_lashlang_runtime::lashlang_process_signal_event_types)
            .unwrap_or_default();
        Self {
            module_ref: linked.module_ref,
            host_requirements_ref: linked.host_requirements_ref,
            process_ref,
            process_name: process_name.to_string(),
            signal_event_types,
        }
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
        let input = lash_lashlang_runtime::LashlangProcessInput {
            module_ref: self.module_ref.clone(),
            process_ref: self.process_ref.clone(),
            host_requirements_ref: self.host_requirements_ref.clone(),
            process_name: self.process_name.clone(),
            args: serde_json::Map::new(),
        };
        lash_core::ProcessIdentity::new(lash_lashlang_runtime::LASHLANG_ENGINE_KIND)
            .with_label(Some(self.process_name.clone()))
            .with_definition(Some(input.definition()))
    }

    fn start_request(&self, process_id: &ProcessId) -> lash_core::ProcessStartRequest {
        lash_core::ProcessStartRequest::new(
            process_id,
            self.process_input(),
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessOriginator::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
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
    lash_core::ProcessEventAppendRequest::new(event_type, payload).with_replay_key(format!(
        "process:{process_id}:signal.{signal_name}:{signal_id}"
    ))
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

fn process_runtime_host_config(
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    provider: ProviderHandle,
) -> lash_core::facade_support::RuntimeHostConfig {
    let mut config = lash_core::facade_support::RuntimeHostConfig::new(
        Arc::new(
            lash_core::facade_support::NativeEffectHost::default()
                .allow_process_lifetime_completion_keys(),
        ),
        Arc::new(lash_core::facade_support::InMemoryAttachmentStore::new()),
        process_env_store,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    config.providers.provider_resolver = Arc::new(
        lash_core::facade_support::SingleProviderResolver::new(provider),
    );
    config
}

fn process_test_core(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
) -> Result<LashCore> {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            artifact_store,
        ),
    )
    .session_spec(
        crate::SessionSpec::new()
            .provider_id(provider_id)
            .turn_budget(crate::TurnBudget::Unbounded),
    )
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .trigger_store(trigger_store)
    .process_registry(registry)
    .without_queued_work()
    .advanced()
    .runtime_host_config(process_runtime_host_config(process_env_store, provider))
    .build(crate::testing::runtime_lease_owner())
}

fn in_memory_process_env_store() -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
    Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new())
}

fn prune_recovery_core(
    registry: Arc<dyn lash_core::ProcessRegistry>,
    env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    engine: Arc<FailOnceReleaseEngine>,
) -> Result<LashCore> {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    let config = process_runtime_host_config(env_store, provider.clone())
        .with_process_engine_registration(lash_core::ProcessEngineRegistration::accepting(
            engine as Arc<dyn lash_core::ProcessEngine>,
        ));
    LashCore::standard_builder(crate::TurnBudget::Unbounded)
        .session_spec(
            crate::SessionSpec::new()
                .provider_id(provider_id)
                .turn_budget(crate::TurnBudget::Unbounded),
        )
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(registry)
        .without_queued_work()
        .advanced()
        .runtime_host_config(config)
        .build(crate::testing::runtime_lease_owner())
}

async fn process_prune_recovery_case(failing_store: &str) -> Result<()> {
    let dir = tempfile::tempdir().expect("process prune recovery tempdir");
    let database = dir.path().join("processes.db");
    let sessions = dir.path().join("sessions");
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&database, &sessions)
            .await
            .expect("open process registry"),
    );
    let env_inner = Arc::new(lash_core::InMemoryProcessExecutionEnvStore::new());
    let env_store = Arc::new(FailOnceReleaseEnvStore {
        inner: Arc::clone(&env_inner),
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
    let process_id = ProcessId::from(format!("prune-recovery-{failing_store}"));
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
                process_id.clone(),
                lash_core::ProcessInput::Engine {
                    kind: engine.kind().to_string(),
                    payload: serde_json::json!({"artifact_ref": "shared-bytes"}),
                },
                lash_core::RecoveryContract::Rerunnable,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_execution_env_ref(Some(env_ref.clone())),
        )
        .await?;
    let process_owner =
        lash_core::ArtifactOwner::process(lash_core::ProcessRef::from_record(&registered));
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
            lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string()),
        )
        .await?;

    let core = prune_recovery_core(
        registry.clone() as Arc<dyn lash_core::ProcessRegistry>,
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
        registry.get_process(&process_id).await,
        Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
    ));
    assert_eq!(pending_after_first.len(), 1);
    drop(core);
    drop(registry);

    let reopened = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&database, &sessions)
            .await
            .expect("reopen process registry after release failure"),
    );
    let recovered_core = prune_recovery_core(
        reopened.clone() as Arc<dyn lash_core::ProcessRegistry>,
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
async fn sqlite_facade_prune_removes_tombstoned_process_delivery() -> Result<()> {
    let dir = tempfile::tempdir().expect("sqlite facade prune tempdir");
    let trigger_store: Arc<dyn lash_core::TriggerStore> = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&dir.path().join("triggers.db"))
            .await
            .expect("open trigger store"),
    );
    let registry: Arc<dyn lash_core::ProcessRegistry> = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(
            &dir.path().join("processes.db"),
            dir.path().join("sessions"),
        )
        .await
        .expect("open process registry"),
    );
    let core = process_test_core(
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new()),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;

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
    let process_id = ingress.reservations[0].process_id.clone();
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                process_id.clone(),
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_identity(lash_core::ProcessIdentity::new("test")),
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
                originator_id: Some("some-session".to_string()),
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
    let orphaned_process_id = orphaned.reservations[0].process_id.clone();
    registry
        .register_process(
            lash_core::ProcessRegistration::new(
                orphaned_process_id.clone(),
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            )
            .with_identity(lash_core::ProcessIdentity::new("test")),
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
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let process_env_store = in_memory_process_env_store();
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        Arc::clone(&process_env_store),
    )?;
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: any } {
          value = wait_signal("ready")
          finish value
        }
        "#,
        "main",
    )
    .await;

    let start_request = process.start_request(&ProcessId::from("sessionless-direct"));
    core.processes()
        .start(
            start_request.clone(),
            runtime_operation_scope(&core, "sessionless-direct-start"),
        )
        .await?;
    core.processes()
        .start(
            start_request,
            runtime_operation_scope(&core, "sessionless-direct-start-replay"),
        )
        .await
        .expect("public start replay discovers process ownership after staging retirement");
    let waiting =
        wait_for_waiting_signal(&core, &ProcessId::from("sessionless-direct"), "ready").await;
    assert!(matches!(
        waiting.originator,
        lash_core::ProcessOriginator::Host { .. }
    ));
    let waiting_events = core
        .processes()
        .events(&ProcessId::from("sessionless-direct"), 0)
        .await?;
    assert!(
        waiting_events
            .iter()
            .any(|event| event.event_type == "process.waiting")
    );

    let cancelled = core
        .processes()
        .cancel(
            &ProcessId::from("sessionless-direct"),
            runtime_operation_scope(&core, "sessionless-direct-cancel"),
        )
        .await?;
    assert_eq!(cancelled.status, lash_core::ProcessStatus::Waiting);
    wait_for_terminal(
        &core,
        &ProcessId::from("sessionless-direct"),
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
            runtime_operation_scope(&core, "sessionless-trigger"),
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
            runtime_operation_scope(&core, "sessionless-host-signal"),
        )
        .await?;
    assert_eq!(event.event_type, "signal.ready");
    let output = core.processes().await_output(triggered_process_id).await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("triggered process did not succeed: {output:#?}");
    };
    assert_eq!(value.to_json_value(), serde_json::json!({ "ok": true }));
    let signal_events = core.processes().events(triggered_process_id, 0).await?;
    assert!(
        signal_events
            .iter()
            .any(|event| event.event_type == "signal.ready")
    );
    Ok(())
}

#[tokio::test]
async fn session_trigger_process_visibility_conformance() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let process_env_store = in_memory_process_env_store();
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        Arc::clone(&process_env_store),
    )?;
    let env_ref =
        persist_process_env_ref(core.env.core.durability.process_env_store.as_ref()).await;
    let session_id = "session-trigger-visibility";
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: any } {
          value = wait_signal("ready")
          finish value
        }
        "#,
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
            runtime_operation_scope(&core, "session-trigger-visibility-emit"),
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
            runtime_operation_scope(&core, "session-trigger-visibility-signal"),
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
    let events = registry.events_after(process_id, 0).await?;

    let session = core.session(session_id).open().await?;
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
    let first_started = events
        .iter()
        .position(|event| event.event_type == "process.first_started")
        .unwrap_or_else(|| panic!("missing process.first_started: {events:?}"));
    let completed = events
        .iter()
        .position(|event| event.event_type == "process.completed")
        .unwrap_or_else(|| panic!("missing process.completed: {events:?}"));
    assert!(
        first_started < completed,
        "process.first_started must precede process.completed: {events:?}"
    );
    Ok(())
}

#[tokio::test]
async fn signal_validation_rejects_undeclared_names_and_mistyped_payloads() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: string } {
          value = wait_signal("ready")
          finish value
        }
        "#,
        "main",
    )
    .await;
    let process_id = "signal-validation";

    core.processes()
        .start(
            process.start_request(&ProcessId::from(process_id)),
            runtime_operation_scope(&core, "signal-validation-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;

    let undeclared = core
        .processes()
        .signal(
            &ProcessId::from(process_id),
            "nope",
            "undeclared-1",
            signal_request(
                &ProcessId::from(process_id),
                "nope",
                "undeclared-1",
                serde_json::json!("x"),
            ),
            runtime_operation_scope(&core, "signal-validation-undeclared"),
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
            &ProcessId::from(process_id),
            "ready",
            "mistyped-1",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "mistyped-1",
                serde_json::json!({ "not": "a string" }),
            ),
            runtime_operation_scope(&core, "signal-validation-mistyped"),
        )
        .await;
    assert!(
        mistyped.is_err(),
        "schema-invalid signal payload must be rejected"
    );

    // Both rejected sends left the process parked with nothing consumed.
    let still_waiting = wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;
    assert_eq!(still_waiting.lifecycle, lash_core::ProcessStatus::Waiting);
    assert!(
        core.processes()
            .events(&ProcessId::from(process_id), 0)
            .await?
            .iter()
            .all(|event| event.event_type != "signal.ready" && event.event_type != "signal.nope")
    );

    core.processes()
        .signal(
            &ProcessId::from(process_id),
            "ready",
            "valid-1",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "valid-1",
                serde_json::json!("done"),
            ),
            runtime_operation_scope(&core, "signal-validation-valid"),
        )
        .await?;
    let output = core
        .processes()
        .await_output(&ProcessId::from(process_id))
        .await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed after valid signal: {output:#?}");
    };
    assert_eq!(value.to_json_value(), serde_json::json!("done"));
    Ok(())
}

#[tokio::test]
async fn repeated_waits_on_one_signal_consume_in_order() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: any } {
          first = wait_signal("ready")
          second = wait_signal("ready")
          finish { first: first, second: second }
        }
        "#,
        "main",
    )
    .await;
    let process_id = "repeated-waits";

    core.processes()
        .start(
            process.start_request(&ProcessId::from(process_id)),
            runtime_operation_scope(&core, "repeated-waits-start"),
        )
        .await?;

    let first_wait = wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;
    let lash_core::WaitKind::Signal { ordinal, .. } =
        first_wait.wait.expect("first wait facet").kind;
    assert_eq!(ordinal, 1, "first wait must use ordinal 1");
    core.processes()
        .signal(
            &ProcessId::from(process_id),
            "ready",
            "order-1",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "order-1",
                serde_json::json!(1),
            ),
            runtime_operation_scope(&core, "repeated-waits-signal-1"),
        )
        .await?;

    let second_wait = wait_for_process(
        &core,
        &ProcessId::from(process_id),
        "second signal wait",
        |process| {
            matches!(
                process.wait.as_ref().map(|wait| &wait.kind),
                Some(lash_core::WaitKind::Signal { ordinal, .. }) if *ordinal == 2
            )
        },
    )
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
            &ProcessId::from(process_id),
            "ready",
            "order-2",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "order-2",
                serde_json::json!(2),
            ),
            runtime_operation_scope(&core, "repeated-waits-signal-2"),
        )
        .await?;

    let output = core
        .processes()
        .await_output(&ProcessId::from(process_id))
        .await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed: {output:#?}");
    };
    assert_eq!(
        value.to_json_value(),
        serde_json::json!({ "first": 1, "second": 2 })
    );

    // The suspension history is on the event log: two waits, two resumes.
    let events = core
        .processes()
        .events(&ProcessId::from(process_id), 0)
        .await?;
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
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process child() {
          finish { from: "child" }
        }

        process main() {
          handle = start child()
          value = await handle
          finish { joined: value }
        }
        "#,
        "main",
    )
    .await;
    let process_id = "parent-joins-child";

    core.processes()
        .start(
            process.start_request(&ProcessId::from(process_id)),
            runtime_operation_scope(&core, "parent-joins-child-start"),
        )
        .await?;
    let output = core
        .processes()
        .await_output(&ProcessId::from(process_id))
        .await?;
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
        .get_process(&ProcessId::from(process_id))
        .await?
        .expect("parent record");
    let child_id = &all
        .iter()
        .find(|record| record.process_id != process_id)
        .expect("child exists")
        .process_id;
    let child = registry.get_process(child_id).await?.expect("child record");
    assert_eq!(child.disposition, lash_core::RecoveryContract::Rerunnable);
    assert_eq!(
        child.lifecycle,
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Process {
                process_id: parent.id,
                incarnation: parent.incarnation,
            },
            lash_core::OnParentEnd::Abandon,
        )
    );
    Ok(())
}

#[tokio::test]
async fn process_children_inherit_session_chain_provenance() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;
    let session_id = "chain-session";
    let process_id = "chain-parent";
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process child() {
          finish { from: "child" }
        }

        process main() {
          handle = start child()
          value = await handle
          finish value
        }
        "#,
        "main",
    )
    .await;
    let session = core.session(session_id).open().await?;
    session
        .admin()
        .processes()
        .start(
            {
                let mut request = process.start_request(&ProcessId::from(process_id));
                request.originator =
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id));
                request
            }
            .with_wake_session_id(Some(SessionId::from(session_id.to_string())))
            .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "chain-parent-start"),
        )
        .await?;
    wait_for_terminal(
        &core,
        &ProcessId::from(process_id),
        lash_core::ProcessStatus::Completed,
    )
    .await;

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
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let core = process_test_core(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        in_memory_process_env_store(),
    )?;
    let session_id = "process-outlives-session";
    let process_id = "outliving-process";
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: any } {
          value = wait_signal("ready")
          finish { resumed: value }
        }
        "#,
        "main",
    )
    .await;
    let session = core.session(session_id).open().await?;
    session
        .admin()
        .processes()
        .start(
            process
                .start_request(&ProcessId::from(process_id))
                .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "outliving-process-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;
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
    let still_waiting = wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;
    assert!(still_waiting.env_ref.is_some());

    let wake_after_delete = registry
        .append_event(
            &ProcessId::from(process_id),
            lash_core::ProcessEventAppendRequest::new(
                "process.wake",
                serde_json::json!({ "text": "wake after deleted session" }),
            ),
        )
        .await?;
    assert_eq!(wake_after_delete.event.event_type, "process.wake");
    assert!(
        core.processes()
            .events(&ProcessId::from(process_id), 0)
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
            &ProcessId::from(process_id),
            "ready",
            "outliving-host-signal",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "outliving-host-signal",
                serde_json::json!({ "after_delete": true }),
            ),
            runtime_operation_scope(&core, "outliving-process-signal"),
        )
        .await?;
    let output = core
        .processes()
        .await_output(&ProcessId::from(process_id))
        .await?;
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("outliving process did not succeed: {output:#?}");
    };
    let value = value.to_json_value();
    assert_eq!(
        value,
        serde_json::json!({ "resumed": { "after_delete": true } })
    );
    wait_for_terminal(
        &core,
        &ProcessId::from(process_id),
        lash_core::ProcessStatus::Completed,
    )
    .await;
    Ok(())
}

/// Records `(event_type, sequence)` for each pushed event, in emit order, as a
/// host would project the freshness feed into its own store.
#[derive(Clone, Default)]
struct CollectingProcessEventSink {
    events: Arc<std::sync::Mutex<Vec<(String, u64)>>>,
    faults: Arc<std::sync::Mutex<Vec<lash_core::facade_support::ProcessWorkerFault>>>,
}

impl CollectingProcessEventSink {
    fn collected(&self) -> Vec<(String, u64)> {
        self.events.lock_recover().clone()
    }

    fn faults(&self) -> Vec<lash_core::facade_support::ProcessWorkerFault> {
        self.faults.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for CollectingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock_recover()
            .push((event.event_type.clone(), event.sequence));
    }

    async fn emit_worker_fault(&self, fault: &lash_core::facade_support::ProcessWorkerFault) {
        self.faults.lock_recover().push(fault.clone());
    }
}

#[derive(Clone)]
struct SwitchableArtifactStore {
    inner: Arc<lash_sqlite_store::Store>,
    unavailable: Arc<std::sync::atomic::AtomicBool>,
    failed_reads: Arc<std::sync::atomic::AtomicUsize>,
}

impl SwitchableArtifactStore {
    fn new(inner: Arc<lash_sqlite_store::Store>) -> Self {
        Self {
            inner,
            unavailable: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            failed_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn set_unavailable(&self, unavailable: bool) {
        self.unavailable
            .store(unavailable, std::sync::atomic::Ordering::SeqCst);
    }

    fn failed_reads(&self) -> usize {
        self.failed_reads.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl lash_lashlang_runtime::LashlangArtifactStore for SwitchableArtifactStore {
    fn durability_tier(&self) -> lashlang::DurabilityTier {
        lashlang::DurabilityTier::Durable
    }

    async fn publish_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        artifact: &lashlang::ModuleArtifact,
    ) -> std::result::Result<(), lashlang::ArtifactStoreError> {
        lash_lashlang_runtime::LashlangArtifactStore::publish_module_artifact(
            self.inner.as_ref(),
            owner,
            artifact,
        )
        .await
    }

    async fn retain_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> std::result::Result<(), lashlang::ArtifactStoreError> {
        lash_lashlang_runtime::LashlangArtifactStore::retain_module_artifact(
            self.inner.as_ref(),
            owner,
            module_ref,
        )
        .await
    }

    async fn transfer_module_artifact(
        &self,
        from: &lash_core::ArtifactOwner,
        to: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> std::result::Result<(), lashlang::ArtifactStoreError> {
        lash_lashlang_runtime::LashlangArtifactStore::transfer_module_artifact(
            self.inner.as_ref(),
            from,
            to,
            module_ref,
        )
        .await
    }

    async fn release_module_artifact(
        &self,
        owner: &lash_core::ArtifactOwner,
        module_ref: &lashlang::ModuleRef,
    ) -> std::result::Result<(), lashlang::ArtifactStoreError> {
        lash_lashlang_runtime::LashlangArtifactStore::release_module_artifact(
            self.inner.as_ref(),
            owner,
            module_ref,
        )
        .await
    }

    async fn retire_module_artifact_owner(
        &self,
        owner: &lash_core::ArtifactOwner,
    ) -> std::result::Result<(), lashlang::ArtifactStoreError> {
        lash_lashlang_runtime::LashlangArtifactStore::retire_module_artifact_owner(
            self.inner.as_ref(),
            owner,
        )
        .await
    }

    async fn get_module_artifact(
        &self,
        module_ref: &lashlang::ModuleRef,
    ) -> std::result::Result<Option<Arc<lashlang::ModuleArtifact>>, lashlang::ArtifactStoreError>
    {
        if self.unavailable.load(std::sync::atomic::Ordering::SeqCst) {
            self.failed_reads
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err(lashlang::ArtifactStoreError::Backend(
                "simulated durable artifact store outage".to_string(),
            ));
        }
        lash_lashlang_runtime::LashlangArtifactStore::get_module_artifact(
            self.inner.as_ref(),
            module_ref,
        )
        .await
    }
}

#[derive(Clone)]
struct DurableAdmissionPaths {
    sessions: std::path::PathBuf,
    processes: std::path::PathBuf,
    triggers: std::path::PathBuf,
    effects: std::path::PathBuf,
    artifacts: std::path::PathBuf,
    attachments: std::path::PathBuf,
}

impl DurableAdmissionPaths {
    fn new(root: &std::path::Path) -> Self {
        Self {
            sessions: root.join("sessions"),
            processes: root.join("processes.db"),
            triggers: root.join("triggers.db"),
            effects: root.join("effects.db"),
            artifacts: root.join("artifacts.db"),
            attachments: root.join("attachments"),
        }
    }
}

async fn durable_admission_core(
    paths: &DurableAdmissionPaths,
    artifact_store: Arc<SwitchableArtifactStore>,
    registry: Arc<lash_sqlite_store::SqliteProcessRegistry>,
    sink: CollectingProcessEventSink,
    owner: &str,
) -> Result<LashCore> {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    let effect_host = Arc::new(
        lash_sqlite_store::SqliteEffectHost::open(&paths.effects)
            .await
            .expect("open durable effect journal"),
    );
    let trigger_store = Arc::new(
        lash_sqlite_store::SqliteTriggerStore::open(&paths.triggers)
            .await
            .expect("open durable trigger store"),
    );
    let store_factory = Arc::new(
        lash_sqlite_store::SqliteSessionStoreFactory::new_with_process_registry(
            &paths.sessions,
            &paths.processes,
        ),
    );
    let artifact: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> = artifact_store.clone();
    LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            artifact,
        ),
    )
    .session_spec(
        crate::SessionSpec::new()
            .provider_id(provider_id)
            .turn_budget(crate::TurnBudget::Unbounded),
    )
    .provider(provider)
    .model(mock_model_spec())
    .store_factory(store_factory)
    .attachment_store(Arc::new(crate::persistence::FileAttachmentStore::new(
        &paths.attachments,
    )))
    .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
    .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
    .process_env_store(artifact_store.inner.clone())
    .process_registry(registry)
    .trigger_store(trigger_store)
    .effect_host(effect_host)
    .process_event_sink(Arc::new(sink))
    .without_queued_work()
    .build(lash_core::LeaseOwnerIdentity::opaque(
        owner,
        format!("{owner}:incarnation"),
    ))
}

async fn wait_for_worker_fault(
    sink: &CollectingProcessEventSink,
    process_id: &ProcessId,
) -> lash_core::facade_support::ProcessWorkerFault {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(fault) = sink.faults().into_iter().find(|fault| {
                matches!(
                    fault,
                    lash_core::facade_support::ProcessWorkerFault::RecoveryRunFailed {
                        process_id: fault_process_id,
                        ..
                    } if fault_process_id == process_id
                )
            }) {
                return fault;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("worker reports the retryable artifact-store fault")
}

fn process_test_core_with_sink(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    sink: Arc<dyn lash_core::facade_support::ProcessEventSink>,
) -> Result<LashCore> {
    let provider = mock_provider();
    let provider_id = provider.kind().to_string();
    LashCore::rlm_builder(
        crate::TurnBudget::Unbounded,
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            artifact_store,
        ),
    )
    .session_spec(
        crate::SessionSpec::new()
            .provider_id(provider_id)
            .turn_budget(crate::TurnBudget::Unbounded),
    )
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .trigger_store(trigger_store)
    .process_registry(registry)
    .process_event_sink(sink)
    .without_queued_work()
    .advanced()
    .runtime_host_config(process_runtime_host_config(process_env_store, provider))
    .build(crate::testing::runtime_lease_owner())
}

/// FIG-1838 + FIG-1521: Start admission is a recorded-input decision. A live
/// artifact-store outage belongs to retryable worker execution, after the
/// Start outcome and process row are durable, and a cold reopen can redrive it.
#[tokio::test]
async fn durable_start_survives_artifact_store_outage_and_redrives_after_restart() -> Result<()> {
    const SESSION_ID: &str = "durable-artifact-outage-session";
    let dir = tempfile::tempdir().expect("durable admission tempdir");
    let paths = DurableAdmissionPaths::new(dir.path());
    let durable_store = Arc::new(
        lash_sqlite_store::Store::open(&paths.artifacts)
            .await
            .expect("open durable artifact and process-environment store"),
    );
    let artifact_store = Arc::new(SwitchableArtifactStore::new(durable_store));
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"process main() -> str { finish "redriven" }"#,
        "main",
    )
    .await;
    let process_input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: process.module_ref.clone(),
        process_ref: process.process_ref.clone(),
        host_requirements_ref: process.host_requirements_ref.clone(),
        process_name: process.process_name.clone(),
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .expect("durable witness input serializes");
    let start_request = lash_core::ProcessStartRequest::new(
        "intent-executor-replaces-this-id",
        process_input,
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessOriginator::host(),
        lash_core::ProcessLifecyclePolicy::new(
            lash_core::ParentScope::Host,
            lash_core::OnParentEnd::Abandon,
        ),
    )
    .with_env_spec(process_env_spec())
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types());
    let registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&paths.processes, &paths.sessions)
            .await
            .expect("open durable process registry"),
    );
    let first_sink = CollectingProcessEventSink::default();
    let first_core = durable_admission_core(
        &paths,
        Arc::clone(&artifact_store),
        Arc::clone(&registry),
        first_sink.clone(),
        "artifact-outage-first-host",
    )
    .await?;
    let first_session = first_core.session(SESSION_ID).open().await?;
    let first_effect_host = first_session.effect_host();
    let first_scoped = first_effect_host
        .scoped_static(lash_core::ExecutionScope::turn(
            SESSION_ID,
            "durable-artifact-outage-turn",
        ))?
        .expect("SQLite effect host owns a static scoped controller");
    let first_processes = {
        let writer = first_session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let intents = lash_core::ToolIntents::v1(vec![lash_core::ToolIntent::StartProcess(Box::new(
        lash_core::StartProcessIntent {
            session_id: SessionId::from(SESSION_ID),
            request: start_request,
        },
    ))]);
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
    let hook = lash_core::ToolChildExecutionTraceHook::new(move |started| {
        if let Some(sender) = started_tx.lock_recover().take() {
            let _ = sender.send(started.process_id);
        }
        panic!("simulate host interruption after the durable child Start");
    });
    artifact_store.set_unavailable(true);
    let first_intents = intents.clone();
    let interrupted = tokio::spawn(async move {
        lash_core::testing::execute_tool_intents_with_services_and_hook(
            first_scoped,
            first_processes,
            &SessionId::from(SESSION_ID),
            "durable-artifact-outage-start",
            &first_intents,
            Some(&hook),
        )
        .await
    });
    let process_id = started_rx
        .await
        .expect("the hook observes Start only after the command returns");
    let interruption = interrupted
        .await
        .expect_err("the host task must be interrupted");
    assert!(interruption.is_panic());
    let first_fault = wait_for_worker_fault(&first_sink, &process_id).await;
    assert!(
        matches!(
            first_fault,
            lash_core::facade_support::ProcessWorkerFault::RecoveryRunFailed {
                ref error,
                ..
            } if error.contains("simulated durable artifact store outage")
        ),
        "the interrupted host's first execution must fail on the injected outage: {first_fault:?}"
    );
    let first_retryable = wait_for_process(
        &first_core,
        &process_id,
        "claimable retry before restart",
        |process| {
            process.lifecycle == lash_core::ProcessStatus::Running
                && process.first_started.is_some()
                && process.lease_holder.is_none()
        },
    )
    .await;
    assert_eq!(first_retryable.lifecycle, lash_core::ProcessStatus::Running);
    assert!(first_retryable.first_started.is_some());
    assert!(first_retryable.lease_holder.is_none());
    assert_eq!(artifact_store.failed_reads(), 1);
    let committed = registry
        .get_process(&process_id)
        .await?
        .expect("the interrupted intent already committed its durable Start row");
    assert_eq!(committed.status, lash_core::ProcessStatus::Running);
    drop(first_session);
    drop(first_core);
    drop(registry);
    drop(artifact_store);

    let reopened_store = Arc::new(
        lash_sqlite_store::Store::open(&paths.artifacts)
            .await
            .expect("reopen durable artifact and process-environment store"),
    );
    let reopened_artifact_store = Arc::new(SwitchableArtifactStore::new(reopened_store));
    let reopened_registry = Arc::new(
        lash_sqlite_store::SqliteProcessRegistry::open(&paths.processes, &paths.sessions)
            .await
            .expect("reopen durable process registry"),
    );
    let reopened_sink = CollectingProcessEventSink::default();
    reopened_artifact_store.set_unavailable(true);
    let reopened_core = durable_admission_core(
        &paths,
        Arc::clone(&reopened_artifact_store),
        Arc::clone(&reopened_registry),
        reopened_sink.clone(),
        "artifact-outage-restarted-host",
    )
    .await?;
    let restarted_worker = lash_core::facade_support::DurableProcessWorker::new(
        reopened_core.durable_process_worker_config()?,
    )?;
    let restarted_drive = restarted_worker.drive_pending_processes().await?;
    assert_eq!(restarted_drive.admitted, vec![process_id.clone()]);
    let fault = wait_for_worker_fault(&reopened_sink, &process_id).await;
    assert!(
        matches!(
            fault,
            lash_core::facade_support::ProcessWorkerFault::RecoveryRunFailed {
                ref error,
                ..
            } if error.contains("simulated durable artifact store outage")
        ),
        "the outage is a retryable worker infrastructure fault: {fault:?}"
    );
    assert_eq!(reopened_artifact_store.failed_reads(), 1);
    let retryable = wait_for_process(
        &reopened_core,
        &process_id,
        "claimable retry after worker fault",
        |process| {
            process.lifecycle == lash_core::ProcessStatus::Running
                && process.first_started.is_some()
                && process.lease_holder.is_none()
        },
    )
    .await;
    assert_eq!(retryable.lifecycle, lash_core::ProcessStatus::Running);
    assert!(retryable.first_started.is_some());
    assert!(retryable.lease_holder.is_none());

    reopened_artifact_store.set_unavailable(false);
    let recovered_drive = restarted_worker.drive_pending_processes().await?;
    assert_eq!(
        recovered_drive.intake,
        lash_core::facade_support::ProcessAdmissionIntake::Scanned
    );
    let admitted_retry =
        recovered_drive.admitted == vec![process_id.clone()] && recovered_drive.deferred.is_empty();
    let coalesced_retry = recovered_drive.admitted.is_empty()
        && recovered_drive.deferred
            == vec![lash_core::facade_support::ProcessAdmissionDeferred {
                process_id: process_id.clone(),
                disposition: lash_core::facade_support::ProcessRecoveryAttemptOutcome::Busy,
            }];
    assert!(
        admitted_retry || coalesced_retry,
        "the retry drive must either admit the claimable row or coalesce it onto the retiring attempt: {recovered_drive:?}"
    );
    let completed = wait_for_process(
        &reopened_core,
        &process_id,
        "redriven completion",
        |process| process.lifecycle == lash_core::ProcessStatus::Completed,
    )
    .await;
    assert!(completed.terminal);

    reopened_artifact_store.set_unavailable(true);
    let failed_reads_before_replay = reopened_artifact_store.failed_reads();
    let reopened_session = reopened_core.session(SESSION_ID).open().await?;
    let reopened_effect_host = reopened_session.effect_host();
    let reopened_processes = {
        let writer = reopened_session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let replay_scope = || {
        reopened_effect_host
            .scoped(lash_core::ExecutionScope::turn(
                SESSION_ID,
                "durable-artifact-outage-turn",
            ))
            .expect("reopen the recorded intent's durable scope")
    };
    let reopened_replay = lash_core::testing::execute_tool_intents_with_services(
        replay_scope(),
        Arc::clone(&reopened_processes),
        &SessionId::from(SESSION_ID),
        "durable-artifact-outage-start",
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    let [
        lash_core::ToolIntentExecutionOutcome::Executed {
            kind: lash_core::ToolIntentKind::StartProcess,
            result,
            ..
        },
    ] = reopened_replay.as_slice()
    else {
        panic!(
            "cold redrive must replay Start success, never persist Refused(CommandFailed): \
             {reopened_replay:?}"
        )
    };
    let recorded_start: lash_core::ProcessHandleView =
        serde_json::from_value(result.clone()).expect("Start records a process handle");
    assert_eq!(
        recorded_start,
        lash_core::ProcessHandleView::from_record(committed),
        "cold redrive must return the exact Start result committed before interruption"
    );
    assert_eq!(
        reopened_artifact_store.failed_reads(),
        failed_reads_before_replay,
        "intent redrive must not consult the unavailable artifact store"
    );
    let replayed_again = lash_core::testing::execute_tool_intents_with_services(
        replay_scope(),
        Arc::clone(&reopened_processes),
        &SessionId::from(SESSION_ID),
        "durable-artifact-outage-start",
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    assert_eq!(
        serde_json::to_vec(&reopened_replay)?,
        serde_json::to_vec(&replayed_again)?,
        "the durable Start result must replay byte-for-byte"
    );
    assert_eq!(
        reopened_artifact_store.failed_reads(),
        failed_reads_before_replay,
        "repeated intent redrive must not consult the unavailable artifact store"
    );

    drop(reopened_session);
    Ok(())
}

/// native-substrate end to end across the process wait, observation, and retention
/// interfaces: a host starts a process, holds `ProcessWorkSubstrate::await_process_terminal`
/// (through `core.processes().await_output`), signals it to completion, and
/// observes its intermediate events through a wired `ProcessEventSink` — then
/// prunes the terminal registry rows while the host's projected copies survive.
#[tokio::test]
async fn native_process_await_sink_and_prune_end_to_end() -> Result<()> {
    let artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore> =
        Arc::new(lash_lashlang_runtime::InMemoryLashlangArtifactStore::new());
    let trigger_store: Arc<dyn lash_core::TriggerStore> =
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let registry: Arc<dyn lash_core::ProcessRegistry> =
        Arc::new(TestLocalProcessRegistry::default());
    let process_env_store = in_memory_process_env_store();
    let sink = CollectingProcessEventSink::default();
    let core = process_test_core_with_sink(
        Arc::clone(&artifact_store),
        Arc::clone(&trigger_store),
        Arc::clone(&registry),
        Arc::clone(&process_env_store),
        Arc::new(sink.clone()),
    )?;
    let process = LinkedTestProcess::new(
        artifact_store.as_ref(),
        r#"
        process main() signals { ready: any } {
          value = wait_signal("ready")
          finish value
        }
        "#,
        "main",
    )
    .await;

    let process_id = "e2e-await-sink-prune";
    core.processes()
        .start(
            process.start_request(&ProcessId::from(process_id)),
            runtime_operation_scope(&core, "e2e-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, &ProcessId::from(process_id), "ready").await;

    // Hold the terminal await while the process is still running; it must resolve
    // only once the signal drives the process to finish.
    let await_core = core.clone();
    let await_id = process_id.to_string();
    let started = std::time::Instant::now();
    let waiter = tokio::spawn(async move {
        await_core
            .processes()
            .await_output(&ProcessId::from(await_id))
            .await
    });

    let payload = serde_json::json!({ "ok": true, "answer": 42 });
    core.processes()
        .signal(
            &ProcessId::from(process_id),
            "ready",
            "e2e-signal-1",
            signal_request(
                &ProcessId::from(process_id),
                "ready",
                "e2e-signal-1",
                payload.clone(),
            ),
            runtime_operation_scope(&core, "e2e-signal"),
        )
        .await?;

    let output = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("held await_terminal resolves within bound")
        .expect("join await task")?;
    let elapsed = started.elapsed();
    let output = output.into_tool_output();
    let lash_core::ToolCallOutcome::Success(value) = output.outcome else {
        panic!("process did not succeed: {output:#?}");
    };
    let value = value.to_json_value();
    assert_eq!(
        value, payload,
        "the held await_terminal yields exactly the process's finish value"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the held await resolves promptly once the process completes (waited {elapsed:?})"
    );

    // The wired sink observed lifecycle, signal, and terminal events in append
    // order. The await seam remains authoritative for terminal observation.
    let collected = sink.collected();
    let sequences: Vec<u64> = collected.iter().map(|(_, sequence)| *sequence).collect();
    let mut sorted = sequences.clone();
    sorted.sort_unstable();
    assert_eq!(
        sequences, sorted,
        "the sink observes appended events in per-process append order; got {collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|(event_type, _)| event_type == "signal.ready"),
        "the sink observed the intermediate signal event; got {collected:?}"
    );
    assert!(
        collected
            .iter()
            .any(|(event_type, _)| event_type == "process.completed"),
        "the sink observed the terminal append; got {collected:?}"
    );

    wait_for_terminal(
        &core,
        &ProcessId::from(process_id),
        lash_core::ProcessStatus::Completed,
    )
    .await;

    // Retention: prune the terminal registry rows. The registry forgets the
    // process, but the host's projected copies (the sink log) remain intact.
    let projected_before_prune = sink.collected();
    let report = core
        .processes()
        .prune(
            i64::MAX as u64,
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune terminal process");
    assert_eq!(
        report.pruned_processes, 1,
        "the single terminal process is pruned"
    );
    assert!(
        matches!(
            registry.get_process(&ProcessId::from(process_id)).await,
            Err(lash_core::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the pruned process returns the typed retained-history miss"
    );
    assert_eq!(
        sink.collected(),
        projected_before_prune,
        "the host's projected copies survive the registry prune untouched"
    );
    assert!(
        sink.collected()
            .iter()
            .any(|(event_type, _)| event_type == "signal.ready"),
        "the projected intermediate events remain available to the host after prune"
    );

    Ok(())
}

mod artifact_cleanup_round4;
mod recovery_dispositions;
