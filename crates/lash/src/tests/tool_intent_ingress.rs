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
    ingress_core_with_effect_host_and_env_store(
        effect_host,
        Arc::new(lash_core::facade_support::InMemoryProcessExecutionEnvStore::new()),
    )
    .await
}

async fn ingress_core_with_effect_host_and_env_store(
    effect_host: Arc<dyn lash_core::EffectHost>,
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
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
        .process_env_store(process_env_store)
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

/// Registers the subscription a submitted occurrence must reserve a delivery
/// for. Without it every emit report is empty and the dedupe assertions below
/// pass without ever touching reservation or delivery state.
async fn register_ingress_trigger_subscription(
    store: &lash_core::facade_support::InMemoryTriggerStore,
) -> Result<lash_core::TriggerSubscriptionRecord> {
    use lash_core::TriggerStore as _;
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "test/intent-ingress-delivery",
        lash_core::ProcessExecutionEnvRef::new("process-env:intent-ingress-delivery"),
        "intent.ingress.trigger",
        "intent-ingress-source",
        lash_core::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"process": "intent-ingress-delivery"}),
        },
        lash_core::ProcessIdentity::new("test-engine").with_label(Some("intent-ingress-delivery")),
    )
    .with_payload_schema(lash_core::LashSchema::any());
    let outcome = store
        .execute_command(
            "intent-ingress-subscription",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("intent-ingress")
                    .expect("owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("intent-ingress"),
                draft,
            },
        )
        .await?
        .expect("register the ingress trigger subscription");
    let lash_core::TriggerCommandOutcome::Mutation { receipt } = outcome else {
        panic!("registration must return a mutation receipt")
    };
    Ok(receipt.record_snapshot)
}

async fn ingress_core_with_trigger_store(
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> Result<(
    LashCore,
    Arc<lash_core::facade_support::InMemoryTriggerStore>,
    lash_core::TriggerSubscriptionRecord,
)> {
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let subscription = register_ingress_trigger_subscription(&store).await?;
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .effect_host(effect_host)
        .provider(mock_provider())
        .model(mock_model_spec())
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_env_store(Arc::new(
            lash_core::facade_support::InMemoryProcessExecutionEnvStore::new(),
        ))
        .process_registry(registry as Arc<dyn lash_core::ProcessRegistry>)
        .trigger_store(Arc::clone(&store) as Arc<dyn lash_core::TriggerStore>)
        .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, store, subscription))
}

fn trigger_intent(session_id: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
        session_id: session_id.to_string(),
        request: lash_core::TriggerOccurrenceRequest::new(
            "intent.ingress.trigger",
            "intent-ingress-source",
            serde_json::json!({"law": "host-submitted-emission"}),
            "intent-ingress-occurrence",
        ),
    })
}

/// The host front door realizes the fifth intent kind through the trigger
/// router, and re-submitting the same identity cannot emit a second time.
#[tokio::test]
async fn host_submitted_trigger_intent_emits_one_occurrence() -> Result<()> {
    use lash_core::TriggerStore as _;

    let (core, store, subscription) =
        ingress_core_with_trigger_store(Arc::new(KeyJournalController::default())).await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("host-trigger-call", 0);

    let first = ingress.submit(key.clone(), trigger_intent(SESSION)).await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitTrigger,
                result,
                parent_end: None,
                ..
            },
        replayed: false,
    } = first
    else {
        panic!("the host front door must realize a recorded trigger emission")
    };
    let occurrences = store
        .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
        .await?;
    assert_eq!(occurrences.len(), 1);
    assert_eq!(
        result["occurrence_id"].as_str(),
        Some(occurrences[0].occurrence_id.as_str())
    );
    let deliveries = store
        .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
        .await?;
    assert_eq!(
        deliveries.len(),
        1,
        "the registered subscription is reserved"
    );
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscription.subscription_id
    );

    // The trigger route's dedupe point is the occurrence idempotency key at
    // the store, not an effect-journal key, so re-submitting the identity
    // re-ingests the same occurrence rather than creating a second one. The
    // reservation reads back as already reserved on that second pass, which is
    // exactly the live-state read a recorded outcome may not expose.
    let duplicate = ingress.submit(key, trigger_intent(SESSION)).await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitTrigger,
                result: duplicate_result,
                ..
            },
        ..
    } = duplicate
    else {
        panic!("a re-submitted trigger declaration stays inside the intent protocol")
    };
    assert_eq!(duplicate_result, result);
    assert_eq!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .len(),
        1,
        "a re-submitted identity cannot ingest a second occurrence"
    );
    assert_eq!(
        store
            .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
            .await?
            .len(),
        1,
        "a re-submitted identity cannot reserve a second delivery"
    );
    Ok(())
}

/// A runtime-owned host has no journal to replay the emission from, so the
/// submission row is the whole record: the first submit realizes the emission
/// and completes its row, and the second is refused against that row rather
/// than re-entering the router.
#[tokio::test]
async fn runtime_owned_trigger_submission_records_its_outcome_once() -> Result<()> {
    use lash_core::TriggerStore as _;

    let (core, store, _subscription) =
        ingress_core_with_trigger_store(Arc::new(crate::durability::InlineEffectHost::default()))
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("runtime-owned-trigger-call", 0);

    let first = ingress.submit(key.clone(), trigger_intent(SESSION)).await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitTrigger,
                ..
            },
        ..
    } = first
    else {
        panic!("a runtime-owned host realizes the trigger declaration: {first:?}")
    };

    let duplicate = ingress.submit(key, trigger_intent(SESSION)).await;
    assert!(
        matches!(
            duplicate,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                    kind: lash_core::ToolIntentKind::EmitTrigger
                },
                ..
            }
        ),
        "the completed submission row refuses the second submit: {duplicate:?}"
    );
    assert_eq!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .len(),
        1,
        "the refusal keeps the second submit out of the router"
    );
    Ok(())
}

#[derive(Default)]
struct ProbeProcessEnvStore {
    puts: std::sync::atomic::AtomicUsize,
    fail_put: std::sync::atomic::AtomicBool,
    values: tokio::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for ProbeProcessEnvStore {
    async fn put_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(lash_core::PluginError::Session(
                "injected process env persist failure".to_string(),
            ));
        }
        self.values
            .lock()
            .await
            .insert(env_ref.as_str().to_string(), bytes.to_vec());
        Ok(())
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<Option<Vec<u8>>, lash_core::PluginError> {
        Ok(self.values.lock().await.get(env_ref.as_str()).cloned())
    }
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
    admission: std::sync::Mutex<Option<MockEffectAdmission>>,
    realizations: std::sync::atomic::AtomicUsize,
    recorded: std::sync::Mutex<Option<lash_core::RuntimeEffectOutcome>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MockEffectAdmission {
    replay_key: String,
    envelope_hash: String,
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
        let replay_key = envelope
            .invocation
            .replay_key()
            .expect("ingress envelope has a replay key")
            .to_string();
        let envelope_hash = envelope.stable_hash()?;
        let submitted_admission = MockEffectAdmission {
            replay_key,
            envelope_hash,
        };
        let first_admission = {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match admission.as_ref() {
                None => {
                    *admission = Some(submitted_admission.clone());
                    true
                }
                Some(recorded) if recorded == &submitted_admission => false,
                Some(recorded) => {
                    return Err(lash_core::RuntimeEffectControllerError::foreign(
                        "test_admission_envelope_hash_conflict",
                        format!(
                            "replay key `{}` was admitted with envelope hash `{}` but redriven with `{}`",
                            recorded.replay_key,
                            recorded.envelope_hash,
                            submitted_admission.envelope_hash,
                        ),
                    ));
                }
            }
        };
        if let Some(recorded) = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
        {
            return Ok(recorded);
        }
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

fn start_intent_with_env(session_id: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: session_id.to_string(),
        request: lash_core::ProcessStartRequest::new(
            "ingress-env-start",
            lash_core::ProcessInput::ToolCall {
                call: lash_core::PreparedToolCall::from_parts(
                    "ingress-env-call",
                    "tool:ingress-env",
                    "ingress_env",
                    serde_json::Value::Null,
                    None,
                    serde_json::Value::Null,
                ),
            },
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessOriginator::host(),
        )
        .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy {
                model: mock_model_spec(),
                ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
            },
        )),
        on_parent_end: Default::default(),
    }))
}

fn cancel_intent(session_id: &str) -> lash_core::ToolIntent {
    cancel_intent_with_reason(session_id, "kind-swap probe")
}

fn cancel_intent_with_reason(session_id: &str, reason: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        reason: Some(reason.to_string()),
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
async fn recorded_outcome_outside_intent_protocol_is_a_typed_ingress_refusal() -> Result<()> {
    let controller = Arc::new(KeyJournalController::default());
    let (core, registry) =
        ingress_core_with_effect_host(Arc::clone(&controller) as Arc<dyn lash_core::EffectHost>)
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("seeded-outside-protocol", 0);
    controller
        .recorded
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(
            key.identity().replay_key.clone(),
            lash_core::RuntimeEffectOutcome::Process {
                result: lash_core::ProcessEffectOutcome::List {
                    entries: Vec::new(),
                },
            },
        );

    let outcome = ingress.submit(key, emit_intent(SESSION)).await;
    assert!(matches!(
        outcome,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal:
                crate::tools::ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol {
                    recorded,
                }
        } if recorded == "list"
    ));
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0,
        "a seeded non-protocol outcome cannot fabricate an intent realization"
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
async fn runtime_owned_cancel_duplicate_identity_is_typed_and_realizes_once() -> Result<()> {
    let (core, registry) =
        ingress_core_with_effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;

    for (call_id, first_reason, duplicate_reason) in [
        ("cancel-same-reason", "same", "same"),
        ("cancel-changed-reason", "first", "changed"),
    ] {
        let key = ingress.key(call_id, 0);
        let first = ingress
            .submit(
                key.clone(),
                cancel_intent_with_reason(SESSION, first_reason),
            )
            .await;
        assert!(matches!(
            first,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    kind: lash_core::ToolIntentKind::CancelProcess,
                    ..
                },
                replayed: false,
            }
        ));
        let duplicate = ingress
            .submit(key, cancel_intent_with_reason(SESSION, duplicate_reason))
            .await;
        assert!(matches!(
            duplicate,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                    kind: lash_core::ToolIntentKind::CancelProcess,
                }
            }
        ));
    }

    let concurrent_key = ingress.key("cancel-concurrent", 0);
    let (left, right) = tokio::join!(
        ingress.submit(
            concurrent_key.clone(),
            cancel_intent_with_reason(SESSION, "concurrent"),
        ),
        ingress.submit(
            concurrent_key,
            cancel_intent_with_reason(SESSION, "concurrent"),
        ),
    );
    let concurrent_outcomes = [left, right];
    assert_eq!(
        concurrent_outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                crate::tools::ToolIntentIngressOutcome::Admitted {
                    outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                        kind: lash_core::ToolIntentKind::CancelProcess,
                        ..
                    },
                    replayed: false,
                }
            ))
            .count(),
        1,
        "one concurrent submit realizes the cancellation"
    );
    assert_eq!(
        concurrent_outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                crate::tools::ToolIntentIngressOutcome::Refused {
                    refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                        kind: lash_core::ToolIntentKind::CancelProcess,
                    }
                }
            ))
            .count(),
        1,
        "the racing duplicate is a typed refusal"
    );

    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        3,
        "each ingress identity realizes one cancellation"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_owned_identity_is_bound_before_a_different_target_is_submitted() -> Result<()> {
    let (core, registry) =
        ingress_core_with_effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
            .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("runtime-cross-target-kind", 0);

    let started = ingress.submit(key.clone(), start_intent(SESSION)).await;
    assert!(matches!(
        started,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::StartProcess,
                ..
            },
            replayed: false,
        }
    ));

    let refused = ingress.submit(key, emit_intent(SESSION)).await;
    assert!(matches!(
        refused,
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
        "the different target must not hide the first identity binding"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_owned_identity_gate_is_shared_across_independent_ingress_handles() -> Result<()> {
    let (core, registry) =
        ingress_core_with_effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
            .await?;
    let left = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let right = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = left.key("runtime-cross-handle-cancel", 0);

    let (left_outcome, right_outcome) = tokio::join!(
        left.submit(
            key.clone(),
            cancel_intent_with_reason(SESSION, "cross-handle"),
        ),
        right.submit(key, cancel_intent_with_reason(SESSION, "cross-handle"),),
    );
    let outcomes = [left_outcome, right_outcome];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                crate::tools::ToolIntentIngressOutcome::Admitted {
                    replayed: false,
                    ..
                }
            ))
            .count(),
        1,
        "exactly one handle reports a fresh realization"
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(
                outcome,
                crate::tools::ToolIntentIngressOutcome::Refused {
                    refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity { .. }
                }
            ))
            .count(),
        1,
        "the independently bound handle observes the authoritative duplicate"
    );
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == "process.cancel_requested")
            .count(),
        1,
        "the identity realizes one cancellation"
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
        (
            crate::tools::ToolIntentIngressRefusal::RecordedOutcomeOutsideIntentProtocol {
                recorded: "list".to_string(),
            },
            &["recorded"][..],
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
            .as_ref()
            .map(|admission| admission.replay_key.as_str()),
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

    let mut conflicting_redrive = emit_intent(SESSION);
    let lash_core::ToolIntent::EmitProcessEvent(intent) = &mut conflicting_redrive else {
        unreachable!("fixture is an event intent")
    };
    intent.payload = serde_json::json!({"law": "conflicting-redrive-payload"});
    let redriven = ingress.submit(key.clone(), conflicting_redrive).await;
    assert!(
        matches!(
            &redriven,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused {
                    kind: lash_core::ToolIntentKind::EmitProcessEvent,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed {
                        code,
                        ..
                    },
                    ..
                },
                replayed: false,
            } if code == "tool_intent_ingress_realization_failed"
        ),
        "a conflicting redrive must be rejected as a typed command failure: {redriven:?}"
    );
    assert_eq!(controller.realizations.load(Ordering::SeqCst), 0);
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0,
        "the redrive cannot replace the admitted command"
    );

    let matching_redrive = ingress.submit(key, emit_intent(SESSION)).await;
    assert!(
        matches!(
            &matching_redrive,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                replayed: false,
            }
        ),
        "the admitted command remains redrivable: {matching_redrive:?}"
    );
    assert_eq!(controller.realizations.load(Ordering::SeqCst), 1);
    assert_eq!(
        registry
            .events_after(PROCESS, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1,
        "the originally admitted command realizes exactly once"
    );
    Ok(())
}

#[tokio::test]
async fn start_env_is_persisted_after_admission_and_matching_redrive_completes() -> Result<()> {
    let controller = Arc::new(AdmissionCrashController::default());
    let env_store = Arc::new(ProbeProcessEnvStore::default());
    let (core, registry) = ingress_core_with_effect_host_and_env_store(
        Arc::clone(&controller) as Arc<dyn lash_core::EffectHost>,
        Arc::clone(&env_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>,
    )
    .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("start-env-crash-redrive", 0);
    let process_id = key.identity().replay_key.clone();

    let crashed_ingress = ingress.clone();
    let crashed_key = key.clone();
    let crashed = tokio::spawn(async move {
        crashed_ingress
            .submit(crashed_key, start_intent_with_env(SESSION))
            .await
    });
    controller.admitted.notified().await;
    assert_eq!(
        env_store.puts.load(Ordering::SeqCst),
        0,
        "journal admission must precede every durable env-store mutation"
    );
    crashed.abort();
    assert!(crashed.await.expect_err("injected crash").is_cancelled());

    let redriven = ingress.submit(key, start_intent_with_env(SESSION)).await;
    assert!(
        matches!(
            &redriven,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    kind: lash_core::ToolIntentKind::StartProcess,
                    ..
                },
                replayed: false,
            }
        ),
        "matching start redrive must complete the admitted command: {redriven:?}"
    );
    assert_eq!(env_store.puts.load(Ordering::SeqCst), 1);
    let process = registry
        .get_process(&process_id)
        .await?
        .expect("redrive registers the process");
    let env_ref = process
        .env_ref
        .expect("registered process keeps the env ref");
    assert!(
        env_store
            .get_process_execution_env(&env_ref)
            .await?
            .is_some(),
        "the redriven process environment is usable"
    );
    Ok(())
}

#[tokio::test]
async fn start_env_store_error_is_typed_and_registers_no_process() -> Result<()> {
    let env_store = Arc::new(ProbeProcessEnvStore::default());
    env_store.fail_put.store(true, Ordering::SeqCst);
    let (core, registry) = ingress_core_with_effect_host_and_env_store(
        Arc::new(crate::durability::InlineEffectHost::default()),
        Arc::clone(&env_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>,
    )
    .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("start-env-store-error", 0);
    let process_id = key.identity().replay_key.clone();

    let outcome = ingress
        .submit(key.clone(), start_intent_with_env(SESSION))
        .await;
    assert!(matches!(
        outcome,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Refused {
                kind: lash_core::ToolIntentKind::StartProcess,
                refusal: lash_core::ToolIntentRefusalReason::CommandFailed { .. },
                ..
            },
            replayed: false,
        }
    ));
    assert!(registry.get_process(&process_id).await?.is_none());
    assert!(matches!(
        ingress.submit(key, start_intent_with_env(SESSION)).await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                kind: lash_core::ToolIntentKind::StartProcess,
            }
        }
    ));
    Ok(())
}

#[tokio::test]
async fn ingress_start_default_cancel_is_retained_and_settled_after_scope_rebind() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::process(PROCESS))?;
    let key = ingress.key("parent-end-retention", 0);
    let child_id = key.identity().replay_key.clone();

    let started = ingress.submit(key, start_intent(SESSION)).await;
    assert!(matches!(
        started,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                parent_end: Some(lash_core::ToolIntentParentEnd {
                    policy: lash_core::ProcessParentEndPolicy::Cancel,
                    ..
                }),
                ..
            },
            ..
        }
    ));
    drop(ingress);

    let redriven_scope = core.tool_intents(SESSION, lash_core::ExecutionScope::process(PROCESS))?;
    let settled = redriven_scope.settle_parent_end().await?;
    assert!(matches!(
        settled.as_slice(),
        [lash_core::ToolIntentParentEndOutcome::Cancelled { process_id, .. }]
            if process_id == &child_id
    ));
    assert!(
        registry
            .events_after(&child_id, 0)
            .await?
            .iter()
            .any(|event| event.event_type == "process.cancel_requested"),
        "default Cancel reaches child"
    );
    assert!(
        redriven_scope.settle_parent_end().await?.is_empty(),
        "settlement is durable and idempotent"
    );
    Ok(())
}

const INGRESS_ENGINE_KIND: &str = "ingress-admission-engine";

/// Engine registered on the ingress host, so a submitted start can be checked
/// against a kind that exists and one that does not.
struct IngressAdmissionEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for IngressAdmissionEngine {
    fn kind(&self) -> &'static str {
        INGRESS_ENGINE_KIND
    }

    async fn run(
        &self,
        _context: lash_core::ProcessEngineRunContext<'_>,
        _payload: serde_json::Value,
    ) -> std::result::Result<lash_core::ProcessRunOutcome, lash_core::ProcessInfraError> {
        Ok(lash_core::ProcessAwaitOutput::Success {
            value: serde_json::json!({"ingress_engine": "ran"}),
            control: None,
        }
        .into())
    }

    fn identity(&self, payload: &serde_json::Value) -> lash_core::ProcessIdentity {
        lash_core::ProcessIdentity::new(INGRESS_ENGINE_KIND)
            .with_label(payload.get("program").and_then(serde_json::Value::as_str))
            .with_definition(Some(payload.clone()))
    }
}

struct IngressAdmissionEnginePlugin;

impl lash_core::plugin::SessionPlugin for IngressAdmissionEnginePlugin {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-plugin"
    }

    fn register(
        &self,
        _reg: &mut lash_core::plugin::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

struct IngressAdmissionEngineFactory;

impl lash_core::plugin::PluginFactory for IngressAdmissionEngineFactory {
    fn id(&self) -> &'static str {
        "ingress-admission-engine-factory"
    }

    fn process_engine_contributions(
        &self,
        _ctx: &lash_core::ProcessEngineContributionContext<'_>,
    ) -> std::result::Result<Vec<Arc<dyn lash_core::ProcessEngine>>, lash_core::PluginError> {
        Ok(vec![Arc::new(IngressAdmissionEngine)])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(IngressAdmissionEnginePlugin))
    }
}

async fn ingress_engine_core() -> Result<(LashCore, Arc<TestLocalProcessRegistry>)> {
    let registry = Arc::new(TestLocalProcessRegistry::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(crate::TurnBudget::Unbounded))
        .provider(mock_provider())
        .model(mock_model_spec())
        .plugin(Arc::new(IngressAdmissionEngineFactory))
        .store_factory(Arc::new(
            lash_core::facade_support::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(Arc::clone(&registry) as Arc<dyn lash_core::ProcessRegistry>)
        .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

fn engine_start_intent(kind: &str, payload: serde_json::Value) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: SESSION.to_string(),
        request: lash_core::ProcessStartRequest::new(
            "ingress-engine-start",
            lash_core::ProcessInput::Engine {
                kind: kind.to_string(),
                payload,
            },
            lash_core::RecoveryDisposition::Rerunnable,
            lash_core::ProcessOriginator::host(),
        )
        .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy {
                model: mock_model_spec(),
                ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
            },
        )),
        on_parent_end: Default::default(),
    }))
}

/// FIG-1488: the host front door is a start route too. A submitted intent naming
/// an engine kind this host never registered must be refused before anything is
/// journaled or registered, and an admitted one must carry the engine identity
/// stamp — neither happened while ingress built its Start command unchecked.
#[tokio::test]
async fn ingress_start_intent_crosses_the_engine_admission_gate() -> Result<()> {
    let (core, registry) = ingress_engine_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;

    let unregistered_key = ingress.key("ingress-unregistered-engine", 0);
    let unregistered_id = unregistered_key.identity().replay_key.clone();
    let refused = ingress
        .submit(
            unregistered_key,
            engine_start_intent("ingress-engine-never-registered", serde_json::json!({})),
        )
        .await;
    match &refused {
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome:
                lash_core::ToolIntentExecutionOutcome::Refused {
                    kind: lash_core::ToolIntentKind::StartProcess,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed { message, .. },
                    ..
                },
            replayed: false,
        } => assert!(
            message.contains("process engine `ingress-engine-never-registered` is not configured"),
            "the refusal must carry the engine registry's own typed miss: {message}"
        ),
        other => panic!("unregistered engine kind must be refused, got {other:?}"),
    }
    assert!(
        registry.get_process(&unregistered_id).await?.is_none(),
        "a refused start must register nothing"
    );

    let payload = serde_json::json!({"program": "known"});
    let admitted_key = ingress.key("ingress-registered-engine", 0);
    let admitted_id = admitted_key.identity().replay_key.clone();
    let admitted = ingress
        .submit(
            admitted_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            &admitted,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    kind: lash_core::ToolIntentKind::StartProcess,
                    ..
                },
                ..
            }
        ),
        "a registered engine kind must still be admitted: {admitted:?}"
    );
    let started = registry
        .get_process(&admitted_id)
        .await?
        .expect("admitted start registers its row");
    assert_eq!(
        started.identity,
        lash_core::ProcessEngine::identity(&IngressAdmissionEngine, &payload),
        "the admitted row must carry the engine identity stamp"
    );
    Ok(())
}
