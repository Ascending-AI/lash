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
    admitted: tokio::sync::Notify,
    admission: std::sync::Mutex<Option<String>>,
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
        let replay_key = envelope
            .invocation
            .replay_key()
            .expect("ingress envelope has a replay key")
            .to_string();
        let first_admission = {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if admission.is_none() {
                *admission = Some(replay_key);
                true
            } else {
                false
            }
        };
        if first_admission {
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

fn start_intent(session_id: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: session_id.to_string(),
        request: lash_core::ProcessStartRequest::external(
            "ingress-start",
            lash_core::ProcessOriginator::host(),
            serde_json::Value::Null,
        ),
        on_parent_end: Default::default(),
    }))
}

fn cancel_intent(session_id: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        reason: Some("kind-swap probe".to_string()),
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

    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome: first_outcome,
        replayed: first_replayed,
    } = first
    else {
        panic!("first submission must be admitted")
    };
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome: duplicate_outcome,
        replayed: duplicate_replayed,
    } = duplicate
    else {
        panic!("duplicate submission must replay the admission")
    };
    assert_eq!(
        duplicate_outcome, first_outcome,
        "duplicate admission outcome is stable"
    );
    assert!(!first_replayed, "the first submission executes locally");
    assert!(
        duplicate_replayed,
        "the duplicate returns a recorded outcome"
    );
    assert!(matches!(
        first_outcome,
        lash_core::ToolIntentExecutionOutcome::Executed { .. }
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
async fn identity_reused_from_start_to_emit_is_a_typed_refusal_without_panicking() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("kind-swap-start-emit", 0);

    let first = ingress.submit(key.clone(), start_intent(SESSION)).await;
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::StartProcess,
                ..
            },
            replayed: false,
        }
    ));

    let second = ingress.submit(key, emit_intent(SESSION)).await;
    assert!(matches!(
        second,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                recorded_kind: lash_core::ToolIntentKind::StartProcess,
                submitted_kind: lash_core::ToolIntentKind::EmitProcessEvent,
            }
        }
    ));
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0,
        "the rejected kind swap must not emit the submitted event"
    );
    Ok(())
}

#[tokio::test]
async fn identity_reused_from_emit_to_cancel_cannot_fabricate_cancel_success() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("kind-swap-emit-cancel", 0);

    let first = ingress.submit(key.clone(), emit_intent(SESSION)).await;
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitProcessEvent,
                ..
            },
            replayed: false,
        }
    ));

    let second = ingress.submit(key, cancel_intent(SESSION)).await;
    assert!(matches!(
        second,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                recorded_kind: lash_core::ToolIntentKind::EmitProcessEvent,
                submitted_kind: lash_core::ToolIntentKind::CancelProcess,
            }
        }
    ));
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1,
        "the first intent realizes once and the kind swap realizes nothing"
    );
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        0,
        "the refused submission must not cancel the process"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_owned_duplicate_identity_is_a_typed_ingress_refusal() -> Result<()> {
    let (core, registry) =
        ingress_core_with_effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("runtime-owned-duplicate", 0);

    let first = ingress.submit(key.clone(), emit_intent(SESSION)).await;
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            replayed: false,
            ..
        }
    ));
    let mut conflicting = emit_intent(SESSION);
    let lash_core::ToolIntent::EmitProcessEvent(intent) = &mut conflicting else {
        unreachable!("fixture is an emit intent")
    };
    intent.payload = serde_json::json!({"law": "conflicting-runtime-duplicate"});
    let duplicate = ingress.submit(key, conflicting).await;
    assert!(matches!(
        duplicate,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                kind: lash_core::ToolIntentKind::EmitProcessEvent,
            }
        }
    ));
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
        replayed: false,
    };
    let admitted = serde_json::to_value(admitted).expect("serialize admitted outcome");
    for field in ["outcome", "replayed"] {
        let mut stripped = admitted.clone();
        stripped
            .as_object_mut()
            .expect("tagged ingress outcome")
            .remove(field);
        assert!(
            serde_json::from_value::<crate::tools::ToolIntentIngressOutcome>(stripped).is_err(),
            "ingress outcome field `{field}` must not acquire a serde default"
        );
    }

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
        (
            crate::tools::ToolIntentIngressRefusal::IdentityBoundToDifferentIntent {
                recorded_kind: lash_core::ToolIntentKind::StartProcess,
                submitted_kind: lash_core::ToolIntentKind::EmitProcessEvent,
            },
            &["recorded_kind", "submitted_kind"][..],
        ),
        (
            crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                kind: lash_core::ToolIntentKind::EmitProcessEvent,
            },
            &["kind"][..],
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
    let expected_admission = key.identity().replay_key.clone();

    let crashed_ingress = ingress.clone();
    let crashed_key = key.clone();
    let crashed = tokio::spawn(async move {
        crashed_ingress
            .submit(crashed_key, emit_intent(SESSION))
            .await
    });
    controller.admitted.notified().await;
    assert_eq!(
        controller
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_deref(),
        Some(expected_admission.as_str()),
        "the mock durably records journal admission before the crash window"
    );
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
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                replayed: false,
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
