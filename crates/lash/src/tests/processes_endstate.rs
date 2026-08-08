use super::*;
use lash_core::TestProcessRegistryWriteExt;
use std::collections::BTreeMap;
use std::sync::Arc;

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
            .put_module_artifact(&linked.artifact)
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

    fn start_request(&self, process_id: &str) -> lash_core::ProcessStartRequest {
        lash_core::ProcessStartRequest::new(
            process_id,
            self.process_input(),
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessOriginator::host(),
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
            ..lash_core::SessionPolicy::default()
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
        .put_process_execution_env(&env_ref, &bytes)
        .await
        .expect("store process execution env");
    env_ref
}

fn signal_request(
    process_id: &str,
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
    process_id: &str,
    label: &str,
    matches: impl Fn(&lash_core::facade_support::ObservedProcess) -> bool,
) -> lash_core::facade_support::ObservedProcess {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
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
    core.processes()
        .get(process_id)
        .await
        .expect("get final process state")
        .unwrap_or_else(|| panic!("process `{process_id}` disappeared"))
}

async fn wait_for_waiting_signal(
    core: &LashCore,
    process_id: &str,
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
    process_id: &str,
    status: lash_core::ProcessStatus,
) -> lash_core::facade_support::ObservedProcess {
    wait_for_process(core, process_id, "terminal process", |process| {
        process.lifecycle == status
    })
    .await
}

fn process_test_core(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::new(
                lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                lash_protocol_rlm::ExecutionBound::secs(30),
            ),
            artifact_store,
        ),
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .trigger_store(trigger_store)
    .process_registry(registry)
    .advanced()
    .runtime_host_config({
        let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory();
        config.durability.process_env_store = process_env_store;
        config
    })
    .build()
}

fn in_memory_process_env_store() -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
    Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new())
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
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_identity(lash_core::ProcessIdentity::new("test")),
        )
        .await?;
    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await?;

    // A retention filter carrying the `ProcessListFilter` default status selects
    // `running`, which no prunable row can hold. Refusing it is what keeps a
    // scoped retention call from reporting a silent zero (ADR 0023).
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
        .expect_err("a non-terminal retention filter must be refused");
    assert!(
        refused
            .to_string()
            .contains("non-terminal status `running`"),
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
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_identity(lash_core::ProcessIdentity::new("test")),
        )
        .await?;
    registry
        .complete_process(
            &orphaned_process_id,
            lash_core::ProcessAwaitOutput::Success {
                value: serde_json::json!("done"),
                control: None,
            },
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
        registry.get_process(&orphaned_process_id).await?.is_none(),
        "the reconciled tombstone is compacted"
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

    core.processes()
        .start(
            process.start_request("sessionless-direct"),
            runtime_operation_scope(&core, "sessionless-direct-start"),
        )
        .await?;
    let waiting = wait_for_waiting_signal(&core, "sessionless-direct", "ready").await;
    assert!(matches!(
        waiting.originator,
        lash_core::ProcessOriginator::Host { .. }
    ));
    let waiting_events = core.processes().events("sessionless-direct", 0).await?;
    assert!(
        waiting_events
            .iter()
            .any(|event| event.event_type == "process.waiting")
    );

    let cancelled = core
        .processes()
        .cancel(
            "sessionless-direct",
            runtime_operation_scope(&core, "sessionless-direct-cancel"),
        )
        .await?;
    assert_eq!(cancelled.status, lash_core::ProcessStatus::Waiting);
    wait_for_terminal(
        &core,
        "sessionless-direct",
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
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("triggered process did not succeed: {output:#?}");
    };
    assert_eq!(value, serde_json::json!({ "ok": true }));
    let signal_events = core.processes().events(triggered_process_id, 0).await?;
    assert!(
        signal_events
            .iter()
            .any(|event| event.event_type == "signal.ready")
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
            process.start_request(process_id),
            runtime_operation_scope(&core, "signal-validation-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, process_id, "ready").await;

    let undeclared = core
        .processes()
        .signal(
            process_id,
            "nope",
            "undeclared-1",
            signal_request(process_id, "nope", "undeclared-1", serde_json::json!("x")),
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
            process_id,
            "ready",
            "mistyped-1",
            signal_request(
                process_id,
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
    let still_waiting = wait_for_waiting_signal(&core, process_id, "ready").await;
    assert_eq!(still_waiting.lifecycle, lash_core::ProcessStatus::Waiting);
    assert!(
        core.processes()
            .events(process_id, 0)
            .await?
            .iter()
            .all(|event| event.event_type != "signal.ready" && event.event_type != "signal.nope")
    );

    core.processes()
        .signal(
            process_id,
            "ready",
            "valid-1",
            signal_request(process_id, "ready", "valid-1", serde_json::json!("done")),
            runtime_operation_scope(&core, "signal-validation-valid"),
        )
        .await?;
    let output = core.processes().await_output(process_id).await?;
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("process did not succeed after valid signal: {output:#?}");
    };
    assert_eq!(value, serde_json::json!("done"));
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
            process.start_request(process_id),
            runtime_operation_scope(&core, "repeated-waits-start"),
        )
        .await?;

    let first_wait = wait_for_waiting_signal(&core, process_id, "ready").await;
    let lash_core::WaitKind::Signal { ordinal, .. } =
        first_wait.wait.expect("first wait facet").kind;
    assert_eq!(ordinal, 1, "first wait must use ordinal 1");
    core.processes()
        .signal(
            process_id,
            "ready",
            "order-1",
            signal_request(process_id, "ready", "order-1", serde_json::json!(1)),
            runtime_operation_scope(&core, "repeated-waits-signal-1"),
        )
        .await?;

    let second_wait = wait_for_process(&core, process_id, "second signal wait", |process| {
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
            process_id,
            "ready",
            "order-2",
            signal_request(process_id, "ready", "order-2", serde_json::json!(2)),
            runtime_operation_scope(&core, "repeated-waits-signal-2"),
        )
        .await?;

    let output = core.processes().await_output(process_id).await?;
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("process did not succeed: {output:#?}");
    };
    assert_eq!(value, serde_json::json!({ "first": 1, "second": 2 }));

    // The suspension history is on the event log: two waits, two resumes.
    let events = core.processes().events(process_id, 0).await?;
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
            process.start_request(process_id),
            runtime_operation_scope(&core, "parent-joins-child-start"),
        )
        .await?;
    let output = core.processes().await_output(process_id).await?;
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("parent process did not succeed: {output:#?}");
    };
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
            status: lash_core::ProcessStatusFilter::Completed,
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
        .processes()
        .start(
            {
                let mut request = process.start_request(process_id);
                request.originator =
                    lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id));
                request
            }
            .with_wake_session_id(Some(session_id.to_string()))
            .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "chain-parent-start"),
        )
        .await?;
    wait_for_terminal(&core, process_id, lash_core::ProcessStatus::Completed).await;

    // The child inherited the session originator and indexed wake target, while
    // observation remained the explicit start-time decision for the parent.
    // No child observer is minted implicitly from its wake subscription.
    let completed = core
        .processes()
        .list(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Completed,
            ..lash_core::ProcessListFilter::default()
        })
        .await?;
    assert_eq!(completed.len(), 2);
    for observed in &completed {
        match &observed.originator {
            lash_core::ProcessOriginator::Session {
                session_id: originator_session_id,
            } => {
                assert_eq!(originator_session_id, session_id)
            }
            other => panic!("expected session originator, got {other:?}"),
        }
    }
    let snapshot = core.processes().session_snapshot(session_id).await?;
    assert_eq!(
        snapshot.items.len(),
        1,
        "only the explicitly observed parent is visible in the originating session"
    );
    assert_eq!(snapshot.items[0].process.process_id, process_id);
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
        .processes()
        .start(
            process
                .start_request(process_id)
                .with_observers([session_id.to_string()]),
            runtime_operation_scope(&core, "outliving-process-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, process_id, "ready").await;
    drop(session);

    let report = core
        .delete_session(session_id, session_delete_scope(&core, session_id).await)
        .await?;
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
    let still_waiting = wait_for_waiting_signal(&core, process_id, "ready").await;
    assert!(still_waiting.env_ref.is_some());

    let wake_after_delete = registry
        .append_event(
            process_id,
            lash_core::ProcessEventAppendRequest::new(
                "process.wake",
                serde_json::json!({ "text": "wake after deleted session" }),
            ),
        )
        .await?;
    assert_eq!(wake_after_delete.event.event_type, "process.wake");
    assert!(
        core.processes()
            .events(process_id, 0)
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
            process_id,
            "ready",
            "outliving-host-signal",
            signal_request(
                process_id,
                "ready",
                "outliving-host-signal",
                serde_json::json!({ "after_delete": true }),
            ),
            runtime_operation_scope(&core, "outliving-process-signal"),
        )
        .await?;
    let output = core.processes().await_output(process_id).await?;
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("outliving process did not succeed: {output:#?}");
    };
    assert_eq!(
        value,
        serde_json::json!({ "resumed": { "after_delete": true } })
    );
    wait_for_terminal(&core, process_id, lash_core::ProcessStatus::Completed).await;
    Ok(())
}

/// Records `(event_type, sequence)` for each pushed event, in emit order, as a
/// host would project the freshness feed into its own store.
#[derive(Clone, Default)]
struct CollectingProcessEventSink {
    events: Arc<std::sync::Mutex<Vec<(String, u64)>>>,
}

impl CollectingProcessEventSink {
    fn collected(&self) -> Vec<(String, u64)> {
        self.events.lock().expect("sink lock").clone()
    }
}

#[async_trait::async_trait]
impl lash_core::facade_support::ProcessEventSink for CollectingProcessEventSink {
    async fn emit(&self, event: &lash_core::ProcessEvent) {
        self.events
            .lock()
            .expect("sink lock")
            .push((event.event_type.clone(), event.sequence));
    }
}

fn process_test_core_with_sink(
    artifact_store: Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>,
    trigger_store: Arc<dyn lash_core::TriggerStore>,
    registry: Arc<dyn lash_core::ProcessRegistry>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    sink: Arc<dyn lash_core::facade_support::ProcessEventSink>,
) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::new(
                lash_protocol_rlm::ExecutionBound::instructions(1_000_000),
                lash_protocol_rlm::ExecutionBound::secs(30),
            ),
            artifact_store,
        ),
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .store_factory(Arc::new(
        lash_core::facade_support::InMemorySessionStoreFactory::new(),
    ))
    .trigger_store(trigger_store)
    .process_registry(registry)
    .process_event_sink(sink)
    .advanced()
    .runtime_host_config({
        let mut config = lash_core::facade_support::RuntimeHostConfig::in_memory();
        config.durability.process_env_store = process_env_store;
        config
    })
    .build()
}

/// Inline-tier end to end across the process wait, observation, and retention
/// interfaces: a host starts a process, holds `ProcessWorkDriver::await_terminal`
/// (through `core.processes().await_output`), signals it to completion, and
/// observes its intermediate events through a wired `ProcessEventSink` — then
/// prunes the terminal registry rows while the host's projected copies survive.
#[tokio::test]
async fn inline_process_await_sink_and_prune_end_to_end() -> Result<()> {
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
            process.start_request(process_id),
            runtime_operation_scope(&core, "e2e-start"),
        )
        .await?;
    wait_for_waiting_signal(&core, process_id, "ready").await;

    // Hold the terminal await while the process is still running; it must resolve
    // only once the signal drives the process to finish.
    let await_core = core.clone();
    let await_id = process_id.to_string();
    let started = std::time::Instant::now();
    let waiter = tokio::spawn(async move { await_core.processes().await_output(&await_id).await });

    let payload = serde_json::json!({ "ok": true, "answer": 42 });
    core.processes()
        .signal(
            process_id,
            "ready",
            "e2e-signal-1",
            signal_request(process_id, "ready", "e2e-signal-1", payload.clone()),
            runtime_operation_scope(&core, "e2e-signal"),
        )
        .await?;

    let output = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("held await_terminal resolves within bound")
        .expect("join await task")?;
    let elapsed = started.elapsed();
    let lash_core::ProcessAwaitOutput::Success { value, .. } = output else {
        panic!("process did not succeed: {output:#?}");
    };
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

    wait_for_terminal(&core, process_id, lash_core::ProcessStatus::Completed).await;

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
            registry.get_process(process_id).await,
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

// --- ADR 0019 disposition-driven recovery, at the facade seam ---------------
//
// The disposition/evidence verdicts themselves are pinned by unit and
// conformance coverage (`process_worker::recovery_tests`, the conformance
// registry cases). These tests are the END-TO-END evidence the wave scoped: the
// facade await/observe/prune seams (`core.processes()`) resolving and reflecting
// those verdicts, and a foreign worker's later sweep never resurrecting a
// terminalized row.

fn recovery_local_owner(
    owner_id: &str,
    _host_id: &str,
    _process_start: &str,
) -> lash_core::LeaseOwnerIdentity {
    lash_core::LeaseOwnerIdentity::opaque(owner_id, format!("{owner_id}:incarnation"))
}

/// A bare recovery worker over `registry` with a known lease owner. It runs no
/// engine (empty `PluginHost`); it drains and sweeps the registry only, so it
/// stands in for a host that started OwnerBound work under `owner`.
fn recovery_process_worker(
    registry: Arc<dyn lash_core::ProcessRegistry>,
    owner: lash_core::LeaseOwnerIdentity,
) -> lash_core::facade_support::DurableProcessWorker {
    lash_core::facade_support::DurableProcessWorker::new(
        lash_core::facade_support::DurableProcessWorkerConfig::new(
            Arc::new(lash_core::facade_support::PluginHost::new(Vec::new())),
            lash_core::facade_support::RuntimeHostConfig::in_memory(),
            Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new()),
            registry,
        )
        .with_lease_owner(owner),
    )
}

fn owner_bound_external_registration(id: &str) -> lash_core::ProcessRegistration {
    lash_core::ProcessRegistration::new(
        id,
        lash_core::ProcessInput::External {
            metadata: serde_json::json!({}),
        },
        lash_core::RecoveryDisposition::OwnerBound,
        lash_core::ProcessProvenance::host(),
    )
}

/// Graceful-drain end to end (ADR 0019): a host holds `await_output` on its own
/// started OwnerBound work, then drains at close. The held awaiter resolves with
/// the `Abandoned{OwnerDrain}` terminal naming this owner; a foreign worker's
/// later sweep never resurrects or re-runs it; the facade prune reclaims it.
#[tokio::test]
async fn owner_bound_graceful_drain_resolves_awaiter_and_prunes_end_to_end() -> Result<()> {
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

    let drain_owner = recovery_local_owner("drain-host", "host-a", "drain-start");
    let worker = recovery_process_worker(Arc::clone(&registry), drain_owner.clone());

    // A started OwnerBound row this host owns (first_started under `drain_owner`).
    let process_id = "owner-bound-drain";
    registry
        .register_process(owner_bound_external_registration(process_id))
        .await?;
    registry
        .record_first_started(
            process_id,
            lash_core::ProcessStarted {
                owner: drain_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await?;

    // Hold the terminal await on the still-running row through the facade await
    // seam — it must resolve only once drain terminalizes the work.
    let await_core = core.clone();
    let await_id = process_id.to_string();
    let waiter = tokio::spawn(async move { await_core.processes().await_output(&await_id).await });

    // The host drains its own started OwnerBound work inline at close.
    let report = worker.drain_owner_bound_work().await?;
    assert_eq!(
        report.abandoned,
        vec![process_id.to_string()],
        "drain terminalizes exactly this host's started OwnerBound work"
    );

    let output = tokio::time::timeout(std::time::Duration::from_secs(5), waiter)
        .await
        .expect("held await resolves within bound")
        .expect("join await task")?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = output else {
        panic!("held await did not resolve to an Abandoned terminal: {output:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::OwnerDrain);
    assert_eq!(
        evidence.owner.as_ref(),
        Some(&drain_owner),
        "the owner-drain evidence names this host as the abandoned owner"
    );

    // The Abandoned terminal is model-visible read-side (a fourth terminal peer).
    let observed = core
        .processes()
        .get(process_id)
        .await?
        .expect("abandoned row observed through the facade");
    assert_eq!(observed.lifecycle, lash_core::ProcessStatus::Abandoned);
    assert!(observed.terminal);

    // A foreign worker's sweep never resurrects or re-runs an abandoned row: the
    // row is terminal, so it is off the worklist and untouched.
    let foreign = recovery_process_worker(
        Arc::clone(&registry),
        recovery_local_owner("foreign-host", "host-b", "foreign-start"),
    );
    foreign.drive_pending_processes().await?;
    assert!(
        registry.list_non_terminal().await?.is_empty(),
        "a foreign sweep must not resurrect the abandoned row onto the worklist"
    );
    let re_awaited = core.processes().await_output(process_id).await?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = re_awaited else {
        panic!("the abandoned terminal was mutated by a foreign sweep: {re_awaited:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::OwnerDrain);
    assert_eq!(
        evidence.owner.as_ref(),
        Some(&drain_owner),
        "the immutable owner-drain evidence survives a foreign sweep — the row is never re-run"
    );

    // Retention: the facade prune reclaims the terminal row.
    let prune = core
        .processes()
        .prune(
            i64::MAX as u64,
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await?;
    assert_eq!(prune.pruned_processes, 1);
    assert!(
        matches!(
            core.processes().get(process_id).await,
            Err(crate::EmbedError::Plugin(
                lash_core::PluginError::ProcessNoLongerRetained { .. }
            ))
        ),
        "the pruned abandoned row is a typed retained-history miss"
    );

    Ok(())
}

/// Silent-owner classification + abandon-request reconciliation end to end (ADR
/// 0019): a started OwnerBound row whose holder lease expired
/// (a different host) survives a sweep non-terminal; the facade read exposes the
/// lease facts so a host can classify staleness; an operator's `request_abandon`
/// is visible read-side while pending; and once the stale lease lapses the next
/// sweep reconciles it into `Abandoned{ReconciledRequest}` naming the lapsed
/// owner. Elapsed time alone never terminalized it — only the recorded
/// authorization did.
#[tokio::test]
async fn silent_owner_stays_running_then_abandon_request_reconciles_end_to_end() -> Result<()> {
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

    // The sweep runs on host-a; the started owner is on host-b, so it is never
    // available for a claimant — a silent, foreign, expired holder.
    let sweep_owner = recovery_local_owner("recovery-host", "host-a", "claimant-start");
    let worker = recovery_process_worker(Arc::clone(&registry), sweep_owner);
    let silent_owner = recovery_local_owner("silent-owner", "host-b", "silent-start");

    let process_id = "silent-owner-bound";
    registry
        .register_process(owner_bound_external_registration(process_id))
        .await?;
    registry
        .record_first_started(
            process_id,
            lash_core::ProcessStarted {
                owner: silent_owner.clone(),
                fencing_token: 0,
                attempt: 1,
                started_at_ms: 1,
            },
        )
        .await?;
    let silent_lease = registry
        .claim_process_lease(process_id, &silent_owner, 60_000)
        .await?
        .acquired()
        .expect("silent holder claims its lease");

    // A sweep leaves the silent holder's row non-terminal:
    // elapsed time / silence alone never terminalizes.
    worker.drive_pending_processes().await?;
    let observed = core
        .processes()
        .get(process_id)
        .await?
        .expect("silent row observed");
    assert_eq!(
        observed.lifecycle,
        lash_core::ProcessStatus::Running,
        "a silent holder is left non-terminal by the sweep"
    );
    // The read exposes the lease facts a host classifies staleness from.
    assert_eq!(
        observed.disposition,
        lash_core::RecoveryDisposition::OwnerBound
    );
    assert_eq!(
        observed.lease_holder.as_ref(),
        Some(&silent_owner),
        "ObservedProcess exposes the lease holder identity read-side"
    );
    assert!(
        observed.lease_expires_at_ms.is_some(),
        "ObservedProcess exposes the lease expiry read-side"
    );
    assert!(
        observed.abandon_request.is_none(),
        "no abandon request has been recorded yet"
    );

    // A third party records an Abandon Request through the facade: authorization
    // to accept uncertainty. It terminalizes nothing itself and is visible to
    // observers while pending.
    let after_request = core
        .processes()
        .request_abandon(process_id, "operator", Some("host retired".to_string()))
        .await?;
    let request = after_request
        .abandon_request
        .as_ref()
        .expect("abandon request visible on the returned observation");
    assert_eq!(request.requested_by, "operator");
    assert_eq!(
        after_request.lifecycle,
        lash_core::ProcessStatus::Running,
        "the abandon request does not terminalize the row"
    );
    assert!(
        core.processes()
            .get(process_id)
            .await?
            .and_then(|process| process.abandon_request)
            .is_some(),
        "the pending abandon request is visible read-side before reconciliation"
    );

    // The stale lease lapses (the foreign host finally drops it). Only now may
    // the sweep reconcile the request.
    registry
        .complete_process_lease(&lash_core::ProcessLeaseCompletion::from_lease(
            &silent_lease,
        ))
        .await?;
    worker.drive_pending_processes().await?;

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        core.processes().await_output(process_id),
    )
    .await
    .expect("reconciled terminal resolves within bound")?;
    let lash_core::ProcessAwaitOutput::Abandoned { evidence, .. } = output else {
        panic!("reconciliation did not produce an Abandoned terminal: {output:#?}");
    };
    assert_eq!(evidence.writer, lash_core::AbandonWriter::ReconciledRequest);
    assert_eq!(
        evidence.owner.as_ref().map(|owner| owner.owner_id.as_str()),
        Some("silent-owner"),
        "the reconciled abandonment names the started owner as the lapsed owner"
    );

    Ok(())
}
