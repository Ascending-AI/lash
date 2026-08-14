use super::*;

const SESSION: &str = "intent-ingress-session";
const SCOPE: &str = "intent-ingress-turn";
const PROCESS: &str = "intent-ingress-process";
const EVENT: &str = "intent.ingress.realized";

async fn ingress_core() -> Result<(LashCore, Arc<TestLocalProcessRegistry>)> {
    ingress_core_with_effect_host(Arc::new(KeyJournalController::default())).await
}

async fn ingress_core_with_effect_host(
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> Result<(LashCore, Arc<TestLocalProcessRegistry>)> {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                PROCESS,
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryDisposition::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
            )
            .with_extra_event_types(vec![lash_core::ProcessEventType {
                name: EVENT.to_string(),
                payload_schema: lash_core::LashSchema::any(),
                semantics: lash_core::ProcessEventSemanticsSpec::default(),
            }]),
            &[SESSION.to_string()],
        )
        .await?;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(effect_host)
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build()?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

#[derive(Default)]
struct KeyJournalController {
    inner: lash_core::facade_support::InlineRuntimeEffectController,
    recorded: std::sync::Mutex<std::collections::HashMap<String, lash_core::RuntimeEffectOutcome>>,
}

impl lash_core::AwaitEventResolver for KeyJournalController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::KeyAddressed
    }

    fn allows_process_lifetime_completion_keys(&self) -> bool {
        true
    }
}

impl lash_core::EffectHost for KeyJournalController {
    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::borrowed(self, scope)
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for KeyJournalController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        let replay_key = envelope
            .invocation
            .replay
            .as_ref()
            .expect("ingress effect carries replay identity")
            .key
            .clone();
        if let Some(recorded) = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&replay_key)
            .cloned()
        {
            return Ok(recorded);
        }
        let outcome = lash_core::RuntimeEffectController::execute_effect(
            &self.inner,
            envelope,
            local_executor,
        )
        .await?;
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay_key, outcome.clone());
        Ok(outcome)
    }
}

#[derive(Default)]
struct AdmissionCrashController {
    inner: lash_core::facade_support::InlineRuntimeEffectController,
    first_admission: std::sync::atomic::AtomicBool,
    admitted: tokio::sync::Notify,
    realizations: std::sync::atomic::AtomicUsize,
    recorded: std::sync::Mutex<Option<lash_core::RuntimeEffectOutcome>>,
}

impl lash_core::AwaitEventResolver for AdmissionCrashController {
    fn replay_ownership(&self) -> lash_core::EffectReplayOwnership {
        lash_core::EffectReplayOwnership::Controller
    }

    fn journal_addressing(&self) -> lash_core::EffectJournalAddressing {
        lash_core::EffectJournalAddressing::KeyAddressed
    }
}

impl lash_core::EffectHost for AdmissionCrashController {
    fn scoped<'run>(
        &'run self,
        scope: lash_core::ExecutionScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::borrowed(self, scope)
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for AdmissionCrashController {
    async fn execute_effect(
        &self,
        envelope: lash_core::RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> std::result::Result<lash_core::RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError>
    {
        if let Some(recorded) = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Ok(recorded);
        }
        if !self
            .first_admission
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            self.admitted.notify_one();
            std::future::pending::<()>().await;
        }
        self.realizations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let outcome = lash_core::RuntimeEffectController::execute_effect(
            &self.inner,
            envelope,
            local_executor,
        )
        .await?;
        *self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome.clone());
        Ok(outcome)
    }
}

fn emit_intent(session_id: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        event_type: EVENT.to_string(),
        payload: serde_json::json!({"law": "duplicate-submit"}),
    })
}

#[tokio::test]
async fn duplicate_host_submit_returns_the_same_outcome_and_realizes_once() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("host-call", 0);

    let first = ingress.submit(key.clone(), emit_intent(SESSION)).await;
    let mut conflicting_duplicate = emit_intent(SESSION);
    let lash_core::ToolIntent::EmitProcessEvent(intent) = &mut conflicting_duplicate else {
        unreachable!("fixture is an event intent")
    };
    intent.payload = serde_json::json!({"law": "same-key-different-payload"});
    let duplicate = ingress.submit(key, conflicting_duplicate).await;

    assert_eq!(duplicate, first, "duplicate admission outcome is stable");
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. }
        }
    ));
    let events = registry.events_after(PROCESS, 0).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1,
        "one identity quadruple realizes exactly once"
    );
    Ok(())
}

#[tokio::test]
async fn foreign_session_and_turn_keys_are_typed_refusals() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let foreign_session =
        crate::tools::ToolIntentIngressKey::derive("foreign-session", SCOPE, "host-call", 0);
    let foreign_turn =
        crate::tools::ToolIntentIngressKey::derive(SESSION, "foreign-turn", "host-call", 0);

    assert!(matches!(
        ingress.submit(foreign_session, emit_intent(SESSION)).await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignSession { .. }
        }
    ));
    assert!(matches!(
        ingress.submit(foreign_turn, emit_intent(SESSION)).await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignExecutionScope { .. }
        }
    ));
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn malformed_key_is_a_typed_refusal_before_realization() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let malformed =
        crate::tools::ToolIntentIngressKey::from_identity(lash_core::ToolIntentIdentity {
            session_id: SESSION.to_string(),
            execution_scope_id: SCOPE.to_string(),
            tool_call_id: "host-call".to_string(),
            intent_index: 0,
            replay_key: "forged".to_string(),
        });

    assert!(matches!(
        ingress.submit(malformed, emit_intent(SESSION)).await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::MalformedKey { .. }
        }
    ));
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0
    );
    Ok(())
}

#[test]
fn ingress_transport_fields_are_required_and_have_no_implicit_serde_defaults() {
    let key = crate::tools::ToolIntentIngressKey::derive(SESSION, SCOPE, "compat-call", 7);
    let key_value = serde_json::to_value(&key).expect("serialize ingress key");
    for field in [
        "session_id",
        "execution_scope_id",
        "tool_call_id",
        "intent_index",
        "replay_key",
    ] {
        let mut stripped = key_value.clone();
        stripped
            .as_object_mut()
            .expect("transparent identity object")
            .remove(field);
        assert!(
            serde_json::from_value::<crate::tools::ToolIntentIngressKey>(stripped).is_err(),
            "ingress key field `{field}` must not acquire a serde default"
        );
    }

    let admitted = crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome: lash_core::ToolIntentExecutionOutcome::ProtocolRefused {
            refusal: lash_core::ToolIntentRefusalReason::MissingToolCallId,
        },
    };
    let mut stripped = serde_json::to_value(admitted).expect("serialize admitted outcome");
    stripped
        .as_object_mut()
        .expect("tagged ingress outcome")
        .remove("outcome");
    assert!(
        serde_json::from_value::<crate::tools::ToolIntentIngressOutcome>(stripped).is_err(),
        "ingress outcome field `outcome` must not acquire a serde default"
    );

    let refusal = crate::tools::ToolIntentIngressRefusal::ForeignSession {
        expected: SESSION.to_string(),
        recorded: "foreign".to_string(),
    };
    let refusal_outcome = crate::tools::ToolIntentIngressOutcome::Refused {
        refusal: refusal.clone(),
    };
    let refusal_value = serde_json::to_value(refusal_outcome).expect("serialize refused outcome");
    let mut stripped = refusal_value.clone();
    stripped
        .as_object_mut()
        .expect("tagged ingress outcome")
        .remove("refusal");
    assert!(
        serde_json::from_value::<crate::tools::ToolIntentIngressOutcome>(stripped).is_err(),
        "ingress outcome field `refusal` must not acquire a serde default"
    );

    let refusals = [
        (
            crate::tools::ToolIntentIngressRefusal::MalformedKey {
                expected_replay_key: "expected".to_string(),
                recorded_replay_key: "recorded".to_string(),
            },
            &["expected_replay_key", "recorded_replay_key"][..],
        ),
        (refusal, &["expected", "recorded"][..]),
        (
            crate::tools::ToolIntentIngressRefusal::ForeignExecutionScope {
                expected: SCOPE.to_string(),
                recorded: "foreign".to_string(),
            },
            &["expected", "recorded"][..],
        ),
        (
            crate::tools::ToolIntentIngressRefusal::IntentSessionMismatch {
                expected: SESSION.to_string(),
                recorded: "foreign".to_string(),
            },
            &["expected", "recorded"][..],
        ),
    ];
    for (refusal, fields) in refusals {
        let refusal_value = serde_json::to_value(refusal).expect("serialize ingress refusal");
        for field in fields {
            let mut stripped = refusal_value.clone();
            stripped
                .as_object_mut()
                .expect("tagged ingress refusal")
                .remove(*field);
            assert!(
                serde_json::from_value::<crate::tools::ToolIntentIngressRefusal>(stripped).is_err(),
                "ingress refusal field `{field}` must not acquire a serde default"
            );
        }
    }
}

#[tokio::test]
async fn crash_after_admission_redrives_to_exactly_one_realization() -> Result<()> {
    let controller = Arc::new(AdmissionCrashController::default());
    let (core, registry) =
        ingress_core_with_effect_host(Arc::clone(&controller) as Arc<dyn lash_core::EffectHost>)
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("crash-redrive-call", 0);

    let crashed_ingress = ingress.clone();
    let crashed_key = key.clone();
    let crashed = tokio::spawn(async move {
        crashed_ingress
            .submit(crashed_key, emit_intent(SESSION))
            .await
    });
    controller.admitted.notified().await;
    crashed.abort();
    assert!(
        crashed
            .await
            .expect_err("injected crash aborts submit")
            .is_cancelled()
    );
    assert_eq!(controller.realizations.load(Ordering::SeqCst), 0);

    let redriven = ingress.submit(key, emit_intent(SESSION)).await;
    assert!(
        matches!(
            &redriven,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. }
            }
        ),
        "redriven outcome: {redriven:?}"
    );
    assert_eq!(controller.realizations.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1
    );
    Ok(())
}
