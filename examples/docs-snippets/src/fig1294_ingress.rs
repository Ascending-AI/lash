use std::sync::Arc;

use async_trait::async_trait;
use lash::process::ProcessRegistry as _;
use lash::tools::{
    StaticToolExecute, StaticToolProvider, ToolCall, ToolDefinition, ToolIntentExecutionOutcome,
    ToolOutcome, ToolProvider,
};

const SESSION: &str = "docs-ingress-session";
const SCOPE: &str = "docs-ingress-turn";
const PROCESS: &str = "docs-ingress-process";
const EVENT: &str = "docs.ingress.event";

fn test_core(
    registry: Arc<lash::testing::TestLocalProcessRegistry>,
) -> lash::Result<lash::LashCore> {
    lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .with_native_queued_work()
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("docs-ingress-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid docs ingress model"),
        )
        .effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(registry)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "docs-fig1294-worker",
            "docs-fig1294-boot",
        ))
}

fn event_intent(session_id: &str) -> lash::tools::ToolIntent {
    lash::tools::ToolIntent::EmitProcessEvent(lash::tools::EmitProcessEventIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        event_type: EVENT.to_string(),
        payload: serde_json::json!({"source": "docs-snippet"}),
    })
}

struct AttemptIntentProvider;

#[async_trait]
impl ToolProvider for AttemptIntentProvider {
    fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
        vec![
            ToolDefinition::raw(
                "tool:attempt_intent_docs",
                "attempt_intent_docs",
                "Return a durable event declaration.",
                ToolDefinition::default_input_schema(),
                serde_json::json!({"type": "object"}),
            )
            .manifest(),
        ]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
        (name == "attempt_intent_docs").then(|| {
            Arc::new(
                ToolDefinition::raw(
                    "tool:attempt_intent_docs",
                    "attempt_intent_docs",
                    "Return a durable event declaration.",
                    ToolDefinition::default_input_schema(),
                    serde_json::json!({"type": "object"}),
                )
                .contract(),
            )
        })
    }

    async fn execute(&self, _call: ToolCall<'_>) -> ToolOutcome {
        panic!("attempt-intent provider must use the sealed attempt context")
    }

    async fn execute_attempt(
        &self,
        call: lash::tools::ToolCall<'_>,
    ) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolAttemptOutcome::done(
            lash::tools::ToolOutcomeDone::ok(serde_json::json!({"declared": true})),
            lash::tools::ToolIntents::v1(vec![event_intent(call.context.session_id())]),
        )
    }
}

#[tokio::test]
async fn attempt_provider_returns_a_behaviorally_checked_intent_batch() {
    let legacy = lash::testing::mock_tool_context();
    let attempt = lash::tools::AttemptContext::__for_testing(&legacy, SCOPE);
    let args = serde_json::json!({});
    let result = ToolProvider::execute_attempt(
        &AttemptIntentProvider,
        lash::tools::ToolCall {
            name: "attempt_intent_docs",
            args: &args,
            context: &attempt,
        },
    )
    .await;
    let lash::tools::ToolAttemptOutcome::Done { result, intents } = result else {
        panic!("the provider must complete atomically")
    };
    assert!(result.into_output().is_success());
    assert!(!intents.is_empty());
    assert_eq!(
        intents.protocol_version,
        lash::tools::TOOL_INTENT_PROTOCOL_V1
    );
    assert_eq!(intents.intents.len(), 1);
    assert_eq!(intents.intents[0].session_id(), attempt.session_id());
}

#[tokio::test]
async fn host_ingress_has_typed_identity_dedupe_and_refusals() -> anyhow::Result<()> {
    let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                PROCESS,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash::process::ProcessEventType {
                name: EVENT.to_string(),
                payload_schema: lash::triggers::LashSchema::any(),
                semantics: lash::process::ProcessEventSemanticsSpec::default(),
            }]),
            &[SESSION.to_string()],
        )
        .await?;
    let core = test_core(Arc::clone(&registry))?;
    let _session = core.session(SESSION).open().await?;
    let ingress: lash::tools::ToolIntentIngress =
        core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;
    let key: lash::tools::ToolIntentIngressKey = ingress.key("docs-call", 0);
    let derived = lash::tools::ToolIntentIngressKey::derive(SESSION, SCOPE, "docs-call", 0);
    assert_eq!(key.identity(), derived.identity());

    let admitted = ingress.submit(key.clone(), event_intent(SESSION)).await;
    let duplicate = ingress.submit(key, event_intent(SESSION)).await;
    let lash::tools::ToolIntentIngressOutcome::Admitted { outcome, replayed } = admitted else {
        panic!("the valid host intent must be admitted")
    };
    assert!(!replayed);
    assert!(matches!(
        outcome,
        ToolIntentExecutionOutcome::Executed { .. }
    ));
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal: lash::tools::ToolIntentIngressRefusal::DuplicateIdentity { kind },
    } = duplicate
    else {
        panic!("the runtime-owned tier must type its process-store duplicate")
    };
    assert_eq!(kind, lash::tools::ToolIntentKind::EmitProcessEvent);

    let cross_kind_key = ingress.key("docs-cross-kind", 0);
    let start = lash::tools::ToolIntent::StartProcess(Box::new(lash::tools::StartProcessIntent {
        session_id: SESSION.to_string(),
        request: lash::process::ProcessStartRequest::external(
            "host-id-is-replaced",
            lash::process::ProcessOriginator::host_scoped("docs-ingress"),
            serde_json::json!({"source": "docs-snippet"}),
        ),
        on_parent_end: lash::tools::ProcessParentEndPolicy::Cancel,
    }));
    let first_kind = start.kind();
    assert_eq!(start.session_id(), SESSION);
    assert_ne!(first_kind, event_intent(SESSION).kind());
    assert!(matches!(
        ingress.submit(cross_kind_key.clone(), start).await,
        lash::tools::ToolIntentIngressOutcome::Admitted { .. }
    ));
    let cross_kind = ingress.submit(cross_kind_key, event_intent(SESSION)).await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal:
            lash::tools::ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                recorded_kind,
                submitted_kind,
            },
    } = cross_kind
    else {
        panic!("the second kind must be refused by the durable first writer")
    };
    assert_eq!(recorded_kind, first_kind);
    assert_eq!(submitted_kind, event_intent(SESSION).kind());

    let foreign_session = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::derive("foreign", SCOPE, "docs-call", 1),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused { refusal } = foreign_session else {
        panic!("foreign session must be refused")
    };
    let lash::tools::ToolIntentIngressRefusal::ForeignSession { expected, recorded } = refusal
    else {
        panic!("foreign session refusal must retain both values")
    };
    assert_eq!((expected.as_str(), recorded.as_str()), (SESSION, "foreign"));

    let foreign_scope = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::derive(SESSION, "foreign-scope", "docs-call", 2),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal: lash::tools::ToolIntentIngressRefusal::ForeignExecutionScope { expected, recorded },
    } = foreign_scope
    else {
        panic!("foreign scope must be refused")
    };
    assert_eq!(expected, SCOPE);
    assert_eq!(recorded, "foreign-scope");

    let intent_session = ingress
        .submit(ingress.key("docs-call", 3), event_intent("foreign-intent"))
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal: lash::tools::ToolIntentIngressRefusal::IntentSessionMismatch { expected, recorded },
    } = intent_session
    else {
        panic!("foreign intent session must be refused")
    };
    assert_eq!(expected, SESSION);
    assert_eq!(recorded, "foreign-intent");

    let mut forged = derived.identity().clone();
    forged.replay_key = "forged".to_string();
    let malformed = ingress
        .submit(
            lash::tools::ToolIntentIngressKey::from_identity(forged),
            event_intent(SESSION),
        )
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal:
            lash::tools::ToolIntentIngressRefusal::MalformedKey {
                expected_replay_key,
                recorded_replay_key,
            },
    } = malformed
    else {
        panic!("forged replay key must be refused")
    };
    assert!(expected_replay_key.starts_with("tool-intent:v1:blake3:"));
    assert_eq!(recorded_replay_key, "forged");
    Ok(())
}

#[tokio::test]
async fn ingress_start_retains_and_settles_default_parent_end() -> anyhow::Result<()> {
    let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                PROCESS,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            ),
            &[SESSION.to_string()],
        )
        .await?;
    let core = test_core(Arc::clone(&registry))?;
    let _session = core.session(SESSION).open().await?;

    // docs:start:host-ingress-start-parent-end
    let ingress = core.tool_intents(SESSION, lash::runtime::ExecutionScope::process(PROCESS))?;
    let key = ingress.key("spawn-report-worker", 0);
    let intent = lash::tools::ToolIntent::StartProcess(Box::new(lash::tools::StartProcessIntent {
        session_id: SESSION.to_string(),
        request: lash::process::ProcessStartRequest::external(
            "replaced-by-the-intent-key",
            lash::process::ProcessOriginator::host_scoped("report-worker"),
            serde_json::json!({"report": 42}),
        ),
        on_parent_end: lash::tools::ProcessParentEndPolicy::Cancel,
    }));
    let lash::tools::ToolIntent::StartProcess(start_intent) = &intent else {
        unreachable!("the documented intent is a process start")
    };
    assert_eq!(start_intent.session_id, SESSION);
    assert_eq!(
        start_intent.request.id.as_str(),
        "replaced-by-the-intent-key"
    );
    assert_eq!(intent.kind(), lash::tools::ToolIntentKind::StartProcess);
    let admitted = ingress.submit(key, intent).await;
    assert!(matches!(
        admitted,
        lash::tools::ToolIntentIngressOutcome::Admitted {
            replayed: false,
            ..
        }
    ));

    // Call this when the owning process scope ends. A crash may safely redrive it.
    let settled = ingress.settle_parent_end().await?;
    assert_eq!(settled.len(), 1);
    // docs:end:host-ingress-start-parent-end
    Ok(())
}

struct InternalProbe;

#[async_trait]
impl StaticToolExecute for InternalProbe {
    async fn execute(&self, call: ToolCall<'_>) -> ToolOutcome {
        ToolOutcome::ok(serde_json::json!({"fallback": call.name}))
    }

    async fn execute_internal(
        &self,
        call: lash::tools::InternalProcessToolCall<'_>,
    ) -> ToolOutcome {
        let lash::tools::InternalProcessToolCall {
            name,
            args,
            context,
        } = call;
        let admin: lash::tools::InternalProcessAdmin<'_> = context.processes();
        let start = admin
            .start(lash::process::ProcessStartRequest::external(
                "docs-internal-start",
                lash::process::ProcessOriginator::host(),
                serde_json::Value::Null,
            ))
            .await;
        let list = admin
            .list_handles_filtered(&lash::process::ProcessListFilter::default())
            .await;
        let await_process = admin.await_process("docs-missing").await;
        let cancel = admin.cancel("docs-missing").await;
        let signal = admin
            .signal("docs-missing", "resume", serde_json::Value::Null)
            .await;
        let complete = admin
            .complete_external(
                "docs-missing",
                lash::process::ProcessAwaitOutput::from_tool_output(
                    lash::tools::ToolCallOutput::success(serde_json::Value::Null),
                ),
            )
            .await;
        let _events = context.process_events();
        ToolOutcome::ok(serde_json::json!({
            "name": name,
            "args": args,
            "session_id": context.session_id(),
            "process_id": context.process_id(),
            "has_cancellation": context.cancellation_token().is_some(),
            "start_ok": start.is_ok(),
            "list_ok": list.is_ok(),
            "await_error": await_process.is_err(),
            "cancel_error": cancel.is_err(),
            "signal_error": signal.is_err(),
            "complete_error": complete.is_err(),
        }))
    }
}

#[tokio::test]
async fn internal_process_contract_is_separate_and_observable() {
    let tool = lash::testing::mock_tool_context();
    let context = lash::tools::InternalProcessContext::__for_testing(&tool);
    let definition = ToolDefinition::raw(
        "tool:internal_probe",
        "internal_probe",
        "exercise the internal process-engine context",
        ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object"}),
    )
    .with_activation(lash::tools::ToolActivation::Internal);
    let tool_id = definition.manifest.id.clone();
    let provider = StaticToolProvider::new(vec![definition], InternalProbe);
    let result = lash::tools::ToolProvider::execute_internal_by_id(
        &provider,
        &tool_id,
        &serde_json::json!({"probe": true}),
        &context,
    )
    .await;
    let value = result.as_output().value_for_projection();
    assert_eq!(value["name"], "internal_probe");
    assert_eq!(value["args"], serde_json::json!({"probe": true}));
    assert_eq!(value["session_id"], "test-session");
    assert!(value["process_id"].is_null());
    assert_eq!(value["has_cancellation"], false);
    assert_eq!(value["start_ok"], false);
    assert_eq!(value["list_ok"], false);
    assert_eq!(value["await_error"], true);
    assert_eq!(value["cancel_error"], true);
    assert_eq!(value["signal_error"], true);
    assert_eq!(value["complete_error"], true);

    let direct = lash::tools::StaticToolExecute::execute_internal(
        provider.executor(),
        lash::tools::InternalProcessToolCall {
            name: "internal_probe",
            args: &serde_json::json!({"direct": true}),
            context: &context,
        },
    )
    .await;
    assert_eq!(
        direct.as_output().value_for_projection()["name"],
        "internal_probe"
    );

    let fallback = lash::tools::ToolProvider::execute_internal(
        &provider,
        lash::tools::InternalProcessToolCall {
            name: "internal_probe",
            args: &serde_json::json!({"fallback": true}),
            context: &context,
        },
    )
    .await;
    assert_eq!(
        fallback.as_output().value_for_projection()["name"],
        "internal_probe"
    );
}

async fn ingress_core_with_effect_host(
    effect_host: Arc<dyn lash::durability::EffectHost>,
) -> anyhow::Result<(lash::LashCore, Arc<lash::testing::TestLocalProcessRegistry>)> {
    let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                PROCESS,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            )
            .with_extra_event_types([lash::process::ProcessEventType {
                name: EVENT.to_string(),
                payload_schema: lash::triggers::LashSchema::any(),
                semantics: lash::process::ProcessEventSemanticsSpec::default(),
            }]),
            &[SESSION.to_string()],
        )
        .await?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .with_native_queued_work()
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("pg-tool-intent-ingress-model")
                .context_window_tokens(4_096)
                .build()?,
        )
        .effect_host(effect_host)
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::clone(&registry) as Arc<dyn lash::process::ProcessRegistry>)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "docs-fig1294-evidence-worker",
            "docs-fig1294-evidence-boot",
        ))?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

#[tokio::test]
async fn runtime_owned_cancel_uses_ingress_identity() -> anyhow::Result<()> {
    let (core, registry) =
        ingress_core_with_effect_host(Arc::new(lash::durability::NativeEffectHost::default()))
            .await?;
    let ingress =
        core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("docs-cancel-call", 0);
    let cancel_intent = |reason: &str| {
        lash::tools::ToolIntent::CancelProcess(lash::tools::CancelProcessIntent {
            session_id: SESSION.to_string(),
            process_id: PROCESS.to_string(),
            reason: Some(reason.to_string()),
        })
    };

    let first = ingress
        .submit(key.clone(), cancel_intent("first reason"))
        .await;
    assert!(matches!(
        first,
        lash::tools::ToolIntentIngressOutcome::Admitted {
            outcome: ToolIntentExecutionOutcome::Executed {
                kind: lash::tools::ToolIntentKind::CancelProcess,
                ..
            },
            replayed: false,
        }
    ));
    let duplicate = ingress.submit(key, cancel_intent("changed reason")).await;
    assert!(matches!(
        duplicate,
        lash::tools::ToolIntentIngressOutcome::Refused {
            refusal: lash::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                kind: lash::tools::ToolIntentKind::CancelProcess,
            }
        }
    ));
    let cancel_events = registry
        .events_after(PROCESS, 0)
        .await?
        .into_iter()
        .filter(|event| event.event_type == "process.cancel_requested")
        .collect::<Vec<_>>();
    assert_eq!(cancel_events.len(), 1);
    assert_eq!(cancel_events[0].payload["reason"], "first reason");
    Ok(())
}

struct SeededOutsideProtocolOutcome;

#[async_trait]
impl lash::runtime::AwaitEventResolver for SeededOutsideProtocolOutcome {}

#[async_trait]
impl lash::durability::EffectHost for SeededOutsideProtocolOutcome {
    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash::runtime::ToolIntentOutcomeSink,
        _identity: &lash::tools::ToolIntentIdentity,
        _intent: lash::tools::ToolIntent,
    ) -> Result<lash::runtime::ToolIntentPreparation, lash::runtime::RuntimeError> {
        Ok(lash::runtime::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash::runtime::ToolIntentOutcomeSink,
        identity: &lash::tools::ToolIntentIdentity,
        submitted: lash::tools::ToolIntent,
        outcome: lash::tools::ToolIntentExecutionOutcome,
    ) -> Result<(), lash::runtime::RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    fn scoped<'run>(
        &'run self,
        scope: lash::runtime::ExecutionScope,
    ) -> Result<lash::runtime::ScopedEffectController<'run>, lash::runtime::RuntimeError> {
        lash::runtime::ScopedEffectController::borrowed(self, scope)
    }
}

#[async_trait]
impl lash::runtime::RuntimeEffectController for SeededOutsideProtocolOutcome {
    async fn runtime_effect_failure_disposition(
        &self,
        _code: lash::runtime::RuntimeErrorCode,
    ) -> Result<lash::runtime::RuntimeEffectFailureDisposition, lash::runtime::RuntimeError> {
        Ok(lash::runtime::RuntimeEffectFailureDisposition::AbortInvocation)
    }

    async fn turn_control_participation(
        &self,
    ) -> Result<lash::runtime::TurnControlParticipation, lash::runtime::RuntimeError> {
        Ok(lash::runtime::TurnControlParticipation::DurableJournaled)
    }

    async fn execute_effect(
        &self,
        _envelope: lash::runtime::RuntimeEffectEnvelope,
        _local_executor: lash::runtime::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<lash::runtime::RuntimeEffectOutcome, lash::runtime::RuntimeEffectControllerError>
    {
        Ok(lash::runtime::RuntimeEffectOutcome::Process {
            result: lash::runtime::ProcessEffectOutcome::List {
                entries: Vec::new(),
            },
        })
    }
}

#[tokio::test]
async fn host_ingress_types_a_seeded_outside_protocol_outcome() -> anyhow::Result<()> {
    let (core, _registry) =
        ingress_core_with_effect_host(Arc::new(SeededOutsideProtocolOutcome)).await?;
    let ingress =
        core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;

    let outcome = ingress
        .submit(ingress.key("docs-seeded-outcome", 0), event_intent(SESSION))
        .await;
    let lash::tools::ToolIntentIngressOutcome::Refused {
        refusal:
            lash::tools::ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol { recorded },
    } = outcome
    else {
        panic!("the seeded List outcome must be a typed ingress refusal")
    };
    assert_eq!(recorded, "list");
    Ok(())
}
