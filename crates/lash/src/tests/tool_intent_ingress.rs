use super::*;
use lash_core::ProcessEventLogTestSupport as _;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

const SESSION: &str = "intent-ingress-session";
const SCOPE: &str = "intent-ingress-turn";
const PROCESS: &str = "intent-ingress-process";
const EVENT: &str = "intent.ingress.realized";
const SIGNAL: &str = "ingress-signal";

/// The controller-owned (ordinal-addressed) tier: a memory backend whose
/// effect host is a [`KeyJournalController`].
async fn ingress_core() -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    ingress_core_with_effect_host(Arc::new(KeyJournalController::default())).await
}

/// A memory backend with its effect host replaced by `effect_host`.
async fn ingress_core_with_effect_host(
    effect_host: Arc<dyn lash_core::EffectHost>,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    ingress_core_over(memory_backend().await, Some(effect_host), None).await
}

async fn ingress_core_over(
    backend: Arc<lash_sqlite_store::SqliteBackend>,
    effect_host: Option<Arc<dyn lash_core::EffectHost>>,
    process_env_store: Option<Arc<dyn lash_core::ProcessExecutionEnvStore>>,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                PROCESS,
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
            .with_extra_event_types(vec![
                lash_core::ProcessEventType {
                    name: EVENT.to_string(),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
                lash_core::ProcessEventType {
                    name: format!("signal.{SIGNAL}"),
                    payload_schema: lash_core::LashSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
            &[SessionId::from(SESSION.to_string())],
        )
        .await?;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        ingress_backend(backend, effect_host, process_env_store),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

/// `backend`, with its effect host and process-env store replaced where
/// the test names its own.
fn ingress_backend(
    backend: Arc<dyn lash_core::Backend>,
    effect_host: Option<Arc<dyn lash_core::EffectHost>>,
    process_env_store: Option<Arc<dyn lash_core::ProcessExecutionEnvStore>>,
) -> Arc<dyn lash_core::Backend> {
    let mut decorated = DecoratedBackend::over(backend);
    if let Some(effect_host) = effect_host {
        decorated = decorated.effect_host(move |_| effect_host);
    }
    if let Some(process_env_store) = process_env_store {
        decorated = decorated.process_env_store(move |_| process_env_store);
    }
    Arc::new(decorated)
}

/// A second invocation over `first`'s durable backend with a fresh
/// [`KeyJournalController`]: a fresh effect journal, which is exactly what a
/// redelivered submission gets.
async fn second_invocation_of(first: &LashCore) -> Result<LashCore> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        ingress_backend(
            Arc::clone(first.backend()),
            Some(Arc::new(KeyJournalController::default())),
            None,
        ),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok(core)
}

/// Registers the subscription a submitted occurrence must reserve a delivery
/// for, with its execution environment published to `env_store`. Without it
/// every emit report is empty and the dedupe assertions below pass without
/// ever touching reservation or delivery state.
async fn register_ingress_trigger_subscription(
    store: &dyn lash_core::TriggerStore,
    env_store: &dyn lash_core::ProcessExecutionEnvStore,
) -> Result<lash_core::TriggerSubscriptionRecord> {
    let process_env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        env_store,
        &lash_core::ArtifactOwner::host("process-execution-env-fixture"),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        ),
    )
    .await?;
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "test/intent-ingress-delivery",
        process_env_ref,
        "intent.ingress.trigger",
        "intent-ingress-source",
        lash_core::ProcessInput::Engine {
            kind: "testing-fixture".to_string(),
            payload: serde_json::json!({"process": "intent-ingress-delivery"}),
        },
        lash_core::ProcessIdentity::labelled("testing-fixture", Some("intent-ingress-delivery")),
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
    Arc<dyn lash_core::TriggerStore>,
    lash_core::TriggerSubscriptionRecord,
    Arc<dyn ProcessRegistry>,
)> {
    let backend = memory_backend().await;
    let store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let subscription =
        register_ingress_trigger_subscription(store.as_ref(), backend.process_env_store().as_ref())
            .await?;
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        ingress_backend(backend, Some(effect_host), None),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, store, subscription, registry))
}

fn trigger_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
        session_id: SessionId::from(session_id.to_string()),
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
    let (core, store, subscription, _) =
        ingress_core_with_trigger_store(Arc::new(KeyJournalController::default())).await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("host-trigger-call", 0);

    let first = ingress
        .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
        .await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitTrigger,
                result,
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
    assert_eq!(occurrences[0].idempotency_key, key.identity().replay_key);
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
    let duplicate = ingress
        .submit(key, trigger_intent(&SessionId::from(SESSION)))
        .await;
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

#[tokio::test]
async fn distinct_host_trigger_declarations_create_two_occurrences_and_redrive_exactly_once()
-> Result<()> {
    let (core, store, _subscription, _) =
        ingress_core_with_trigger_store(Arc::new(KeyJournalController::default())).await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let first_key = ingress.key("host-trigger-call-a", 0);
    let second_key = ingress.key("host-trigger-call-b", 0);

    let mut first_outcomes = Vec::new();
    for key in [&first_key, &second_key] {
        let outcome = ingress
            .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
            .await;
        assert!(matches!(
            outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted { .. }
        ));
        first_outcomes.push(outcome);
    }
    let occurrences = store
        .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
        .await?;
    assert_eq!(occurrences.len(), 2);
    assert_eq!(
        occurrences
            .iter()
            .map(|occurrence| occurrence.idempotency_key.clone())
            .collect::<std::collections::BTreeSet<_>>(),
        [
            first_key.identity().replay_key.clone(),
            second_key.identity().replay_key.clone(),
        ]
        .into_iter()
        .collect()
    );

    for (key, first) in [first_key, second_key].into_iter().zip(first_outcomes) {
        let redriven = ingress
            .submit(key, trigger_intent(&SessionId::from(SESSION)))
            .await;
        // The typed outcome is byte-stable across the redrive. The `replayed`
        // bit beside it is not, and must not be: the first submission recorded
        // the occurrence and the redrive coalesced onto it, which is the whole
        // point of the occurrence idempotency key (FIG-3070).
        let (
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: first_outcome,
                replayed: first_replayed,
            },
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: redriven_outcome,
                replayed: redriven_replayed,
            },
        ) = (&first, &redriven)
        else {
            panic!(
                "both the first emission and its redrive are admitted, got {first:?} then {redriven:?}"
            )
        };
        assert_eq!(
            redriven_outcome, first_outcome,
            "redrive returns the byte-stable outcome"
        );
        assert!(!first_replayed, "the first emission records the occurrence");
        assert!(
            redriven_replayed,
            "the redrive coalesces onto the recorded occurrence"
        );
    }
    assert_eq!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .len(),
        2,
        "redriving both identities must not add occurrences"
    );
    assert_eq!(store.list_deliveries().await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn predecessor_host_trigger_key_is_refused_before_store_ingress() -> Result<()> {
    let (core, store, _, _) =
        ingress_core_with_trigger_store(Arc::new(KeyJournalController::default())).await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let mut predecessor = serde_json::to_value(ingress.key("predecessor-trigger-call", 0))?;
    predecessor
        .as_object_mut()
        .expect("versioned ingress key")
        .remove("protocol_version");
    let predecessor = serde_json::from_value(predecessor)?;

    assert!(matches!(
        ingress
            .submit(predecessor, trigger_intent(&SessionId::from(SESSION)))
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::UnsupportedProtocolVersion {
                recorded: 1
            }
        }
    ));
    assert!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .is_empty()
    );
    assert!(store.list_deliveries().await?.is_empty());
    Ok(())
}

/// The backend's process-env store, counting and optionally failing puts.
struct ProbeProcessEnvStore {
    puts: std::sync::atomic::AtomicUsize,
    fail_put: std::sync::atomic::AtomicBool,
    inner: Arc<dyn lash_core::ProcessExecutionEnvStore>,
}

impl ProbeProcessEnvStore {
    fn over(inner: Arc<dyn lash_core::ProcessExecutionEnvStore>) -> Self {
        Self {
            puts: std::sync::atomic::AtomicUsize::new(0),
            fail_put: std::sync::atomic::AtomicBool::new(false),
            inner,
        }
    }
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for ProbeProcessEnvStore {
    async fn publish_process_execution_env(
        &self,
        owner: &lash_core::ArtifactOwner,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::PluginError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(lash_core::PluginError::Session(
                "injected process env persist failure".to_string(),
            ));
        }
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
        self.inner.get_process_execution_env(env_ref).await
    }
}

/// A controller-owned tier: its journal is the `recorded` map, keyed by replay
/// key, and each first execution runs locally. Clones share the journal, so
/// a static scoped controller journals into the same map.
#[derive(Clone, Default)]
struct KeyJournalController {
    recorded:
        Arc<std::sync::Mutex<std::collections::HashMap<String, lash_core::RuntimeEffectOutcome>>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for KeyJournalController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        Some("key-journal-controller".to_string())
    }

    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> std::result::Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        if !may_defer {
            return Ok(lash_core::CompletionKeyPreparation::NotNeeded);
        }
        lash_core::AwaitEventResolver::await_event_key(self, scope, wait)
            .await
            .map(lash_core::CompletionKeyPreparation::Issued)
    }

    /// A controller-owned tier is its own durable-promise authority, the way
    /// the Restate boundary is: the key is derived from the scope and the wait
    /// identity, so a redelivered invocation derives the same key rather than
    /// minting a second wait.
    async fn await_event_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
    ) -> std::result::Result<lash_core::AwaitEventKey, lash_core::RuntimeError> {
        let key_id = lash_core::facade_support::promise_semantics::derive_key_id(scope, &wait)?;
        Ok(lash_core::AwaitEventKey {
            scope: scope.clone(),
            wait,
            key_id,
            signature: "key-journal-controller".to_string(),
        })
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for KeyJournalController {
    fn turn_control_binding_id(&self) -> String {
        "key-journal-controller".to_string()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash_core::ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> std::result::Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        Ok(lash_core::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::borrowed(self, scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        Ok(Some(lash_core::ScopedEffectController::shared(
            Arc::new(self.clone()),
            scope,
        )?))
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
        let replay_key = envelope.invocation.replay_key().to_owned();
        if let Some(recorded) = self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&replay_key)
            .cloned()
        {
            return Ok(recorded);
        }
        let outcome = lash_core::testing::execute_effect_locally(envelope, local_executor).await?;
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay_key, outcome.clone());
        Ok(outcome)
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> std::result::Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError>
    {
        Err(lash_core::effect_groups_unsupported("KeyJournalController"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> std::result::Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError>
    {
        Err(lash_core::effect_groups_unsupported("KeyJournalController"))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> std::result::Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported("KeyJournalController"))
    }
}

/// A controller-owned tier that parks its first admission forever, as a
/// crash between admission and realization would, and journals one outcome.
/// Clones share its state, so a static scoped controller is the same tier.
#[derive(Clone, Default)]
struct AdmissionCrashController {
    admitted: Arc<tokio::sync::Notify>,
    admission: Arc<std::sync::Mutex<Option<MockEffectAdmission>>>,
    realizations: Arc<std::sync::atomic::AtomicUsize>,
    recorded: Arc<std::sync::Mutex<Option<lash_core::RuntimeEffectOutcome>>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MockEffectAdmission {
    replay_key: String,
    envelope_hash: String,
}

impl lash_core::AwaitEventResolver for AdmissionCrashController {
    /// A mock admission host mints keys under no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl lash_core::EffectHost for AdmissionCrashController {
    fn turn_control_binding_id(&self) -> String {
        "admission-crash-controller".to_string()
    }

    fn await_event_resolver(&self) -> &dyn lash_core::AwaitEventResolver {
        self
    }

    async fn prepare_tool_intent(
        &self,
        _sink: &dyn lash_core::ToolIntentOutcomeSink,
        _identity: &lash_core::ToolIntentIdentity,
        _intent: lash_core::ToolIntent,
    ) -> std::result::Result<lash_core::ToolIntentPreparation, lash_core::RuntimeError> {
        Ok(lash_core::ToolIntentPreparation::ControllerOwned)
    }

    async fn record_tool_intent_outcome(
        &self,
        sink: &dyn lash_core::ToolIntentOutcomeSink,
        identity: &lash_core::ToolIntentIdentity,
        submitted: lash_core::ToolIntent,
        outcome: lash_core::ToolIntentExecutionOutcome,
    ) -> std::result::Result<(), lash_core::RuntimeError> {
        sink.retain_in_journal(identity, submitted, outcome).await
    }

    fn scoped<'run>(
        &'run self,
        scope: lash_core::AdmittedScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        lash_core::ScopedEffectController::borrowed(self, scope)
    }

    fn scoped_static(
        &self,
        scope: lash_core::AdmittedScope,
    ) -> std::result::Result<
        Option<lash_core::ScopedEffectController<'static>>,
        lash_core::RuntimeError,
    > {
        Ok(Some(lash_core::ScopedEffectController::shared(
            Arc::new(self.clone()),
            scope,
        )?))
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
        let replay_key = envelope.invocation.replay_key().to_string();
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
                        lash_core::TurnFailureCause::Outcome,
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
        let outcome = lash_core::testing::execute_effect_locally(envelope, local_executor).await?;
        *self
            .recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(outcome.clone());
        Ok(outcome)
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> std::result::Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError>
    {
        Err(lash_core::effect_groups_unsupported(
            "AdmissionCrashController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> std::result::Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError>
    {
        Err(lash_core::effect_groups_unsupported(
            "AdmissionCrashController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> std::result::Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "AdmissionCrashController",
        ))
    }
}

fn emit_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
        session_id: SessionId::from(session_id.to_string()),
        process_id: ProcessId::from(PROCESS.to_string()),
        event_type: EVENT.to_string(),
        payload: serde_json::json!({"law": "duplicate-submit"}),
    })
}

fn start_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: SessionId::from(session_id.to_string()),
        declaration: lash_core::ProcessStartDeclaration::external(
            lash_core::ProcessOriginator::host(),
            serde_json::Value::Null,
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        ),
    }))
}

fn start_intent_with_env(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: SessionId::from(session_id.to_string()),
        declaration: lash_core::ProcessStartDeclaration::new(
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
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessOriginator::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
        .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy {
                model: mock_model_spec(),
                ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
            },
        )),
    }))
}

fn cancel_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    cancel_intent_for_target(session_id, PROCESS)
}

fn cancel_intent_for_target(session_id: &SessionId, target: &str) -> lash_core::ToolIntent {
    lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
        session_id: SessionId::from(session_id.to_string()),
        process_id: ProcessId::from(target),
    })
}

#[tokio::test]
async fn duplicate_host_submit_returns_the_same_outcome_and_realizes_once() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("host-call", 0);

    let first = ingress
        .submit(key.clone(), emit_intent(&SessionId::from(SESSION)))
        .await;
    let mut conflicting_duplicate = emit_intent(&SessionId::from(SESSION));
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
    let events = registry
        .full_event_window(&ProcessId::from(PROCESS), 0)
        .await?;
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

    let first = ingress
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
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

    let second = ingress
        .submit(key, emit_intent(&SessionId::from(SESSION)))
        .await;
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
            .full_event_window(&ProcessId::from(PROCESS), 0)
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

    let first = ingress
        .submit(key.clone(), emit_intent(&SessionId::from(SESSION)))
        .await;
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

    let second = ingress
        .submit(key, cancel_intent(&SessionId::from(SESSION)))
        .await;
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
            .full_event_window(&ProcessId::from(PROCESS), 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1,
        "the first intent realizes once and the kind swap realizes nothing"
    );
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from(PROCESS), 0)
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

    let outcome = ingress
        .submit(key, emit_intent(&SessionId::from(SESSION)))
        .await;
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
            .full_event_window(&ProcessId::from(PROCESS), 0)
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
async fn foreign_session_and_turn_keys_are_typed_refusals() -> Result<()> {
    let (core, registry) = ingress_core().await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let foreign_session =
        crate::tools::ToolIntentIngressKey::derive("foreign-session", SCOPE, "host-call", 0);
    let foreign_turn =
        crate::tools::ToolIntentIngressKey::derive(SESSION, "foreign-turn", "host-call", 0);

    assert!(matches!(
        ingress
            .submit(foreign_session, emit_intent(&SessionId::from(SESSION)))
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignSession { .. }
        }
    ));
    assert!(matches!(
        ingress
            .submit(foreign_turn, emit_intent(&SessionId::from(SESSION)))
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignExecutionScope { .. }
        }
    ));
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from(PROCESS), 0)
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
    let mut malformed = serde_json::to_value(crate::tools::ToolIntentIngressKey::derive(
        SESSION,
        SCOPE,
        "host-call",
        0,
    ))?;
    malformed["replay_key"] = serde_json::json!("forged");
    let malformed = serde_json::from_value(malformed)?;

    assert!(matches!(
        ingress
            .submit(malformed, emit_intent(&SessionId::from(SESSION)))
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::MalformedKey { .. }
        }
    ));
    assert_eq!(
        registry
            .full_event_window(&ProcessId::from(PROCESS), 0)
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
            .expect("versioned identity object")
            .remove(field);
        assert!(
            serde_json::from_value::<crate::tools::ToolIntentIngressKey>(stripped).is_err(),
            "ingress key field `{field}` must not acquire a serde default"
        );
    }

    let mut predecessor = key_value;
    predecessor
        .as_object_mut()
        .expect("versioned identity object")
        .remove("protocol_version");
    let predecessor: crate::tools::ToolIntentIngressKey =
        serde_json::from_value(predecessor).expect("decode predecessor ingress key shape");
    assert_eq!(
        serde_json::to_value(predecessor).expect("re-encode predecessor ingress key")["protocol_version"],
        serde_json::json!(1)
    );

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
            .submit(crashed_key, emit_intent(&SessionId::from(SESSION)))
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

    let mut conflicting_redrive = emit_intent(&SessionId::from(SESSION));
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
            .full_event_window(&ProcessId::from(PROCESS), 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        0,
        "the redrive cannot replace the admitted command"
    );

    let matching_redrive = ingress
        .submit(key, emit_intent(&SessionId::from(SESSION)))
        .await;
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
            .full_event_window(&ProcessId::from(PROCESS), 0)
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
    let backend = memory_backend().await;
    let env_store = Arc::new(ProbeProcessEnvStore::over(backend.process_env_store()));
    let (core, registry) = ingress_core_over(
        backend,
        Some(Arc::clone(&controller) as Arc<dyn lash_core::EffectHost>),
        Some(Arc::clone(&env_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>),
    )
    .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("start-env-crash-redrive", 0);
    let process_id = key.identity().replay_key.clone();

    let crashed_ingress = ingress.clone();
    let crashed_key = key.clone();
    let crashed = tokio::spawn(async move {
        crashed_ingress
            .submit(
                crashed_key,
                start_intent_with_env(&SessionId::from(SESSION)),
            )
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

    let redriven = ingress
        .submit(key, start_intent_with_env(&SessionId::from(SESSION)))
        .await;
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
        .get_process(&ProcessId::from(process_id))
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
    let backend = memory_backend().await;
    let env_store = Arc::new(ProbeProcessEnvStore::over(backend.process_env_store()));
    env_store.fail_put.store(true, Ordering::SeqCst);
    let (core, registry) = ingress_core_over(
        backend,
        None,
        Some(Arc::clone(&env_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>),
    )
    .await?;
    let ingress = core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))?;
    let key = ingress.key("start-env-store-error", 0);
    let process_id = key.identity().replay_key.clone();

    let outcome = ingress
        .submit(
            key.clone(),
            start_intent_with_env(&SessionId::from(SESSION)),
        )
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
    assert!(
        registry
            .get_process(&ProcessId::from(process_id.clone()))
            .await?
            .is_none()
    );
    // The failed put is a live fault, not a recorded outcome: a resubmission
    // of the same identity retries it, meets the same fault, and still
    // registers nothing.
    let resubmitted = ingress
        .submit(key, start_intent_with_env(&SessionId::from(SESSION)))
        .await;
    assert!(
        matches!(
            resubmitted,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused {
                    kind: lash_core::ToolIntentKind::StartProcess,
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed { .. },
                    ..
                },
                replayed: false,
            }
        ),
        "{resubmitted:?}"
    );
    assert!(
        registry
            .get_process(&ProcessId::from(process_id))
            .await?
            .is_none()
    );
    Ok(())
}

#[test]
fn ingress_start_without_lifecycle_is_refused_before_submission() {
    let mut payload =
        serde_json::to_value(start_intent(&SessionId::from(SESSION))).expect("encode intent");
    fn remove_lifecycle(value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                if object.remove("lifecycle").is_some() {
                    return true;
                }
                object.values_mut().any(remove_lifecycle)
            }
            _ => false,
        }
    }
    assert!(
        remove_lifecycle(&mut payload),
        "valid start had a required policy"
    );
    let error = serde_json::from_value::<lash_core::ToolIntent>(payload)
        .expect_err("missing lifecycle must not decode");
    assert!(error.to_string().contains("lifecycle"));
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
        Ok(
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({"ingress_engine": "ran"}),
            ))
            .into(),
        )
    }
}

fn admit_ingress_engine(
    _kind: &'static str,
    payload: &serde_json::Value,
    env: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> std::result::Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    let env = env.ok_or_else(|| {
        lash_core::PluginError::Session(
            "ingress admission requires the recorded execution environment".to_string(),
        )
    })?;
    Ok(lash_core::ProcessIdentity::for_definition(
        lash_core::ProcessDefinitionRef::unclaimed(
            INGRESS_ENGINE_KIND,
            serde_json::json!({
                "payload": payload,
                "model": env.policy.model.id,
                "provider": env.policy.provider_id,
            }),
        ),
        payload.get("program").and_then(serde_json::Value::as_str),
    ))
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
    ) -> std::result::Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError>
    {
        Ok(vec![lash_core::ProcessEngineRegistration::new(
            Arc::new(IngressAdmissionEngine),
            lash_core::ProcessEngineAdmission::new(INGRESS_ENGINE_KIND, admit_ingress_engine),
        )?])
    }

    fn build(
        &self,
        _ctx: &lash_core::plugin::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError>
    {
        Ok(Arc::new(IngressAdmissionEnginePlugin))
    }
}

async fn ingress_engine_core() -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    let backend = memory_backend().await;
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        backend.clone(),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(Arc::new(IngressAdmissionEngineFactory))
    .build(crate::testing::runtime_lease_owner())?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, registry))
}

fn engine_start_intent(kind: &str, payload: serde_json::Value) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        session_id: SessionId::from(SESSION.to_string()),
        declaration: lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::Engine {
                kind: kind.to_string(),
                payload,
            },
            lash_core::RecoveryContract::Rerunnable,
            lash_core::ProcessOriginator::host(),
            lash_core::ProcessLifecyclePolicy::new(
                lash_core::ParentScope::Host,
                lash_core::OnParentEnd::Abandon,
            ),
        )
        .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
            lash_core::PluginOptions::default(),
            lash_core::SessionPolicy {
                model: mock_model_spec(),
                ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
            },
        )),
    }))
}

fn ingress_engine_env_spec() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: mock_model_spec(),
            ..lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded)
        },
    )
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
        registry
            .get_process(&ProcessId::from(unregistered_id))
            .await?
            .is_none(),
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
        .get_process(&ProcessId::from(admitted_id))
        .await?
        .expect("admitted start registers its row");
    assert_eq!(
        started.identity,
        admit_ingress_engine(
            INGRESS_ENGINE_KIND,
            &payload,
            Some(&ingress_engine_env_spec())
        )
        .expect("known payload and recorded environment"),
        "the admitted row must carry the engine identity stamp"
    );
    Ok(())
}

/// FIG-1838: host ingress and session-owned recorded-intent execution must feed
/// the same immutable request environment into engine admission. Otherwise an
/// environment-derived identity changes solely with the route used to start it.
#[tokio::test]
async fn equivalent_recorded_start_has_same_environment_sensitive_identity_across_routes()
-> Result<()> {
    let (core, registry) = ingress_engine_core().await?;
    let payload = serde_json::json!({"program": "environment-sensitive"});

    let ingress = core.tool_intents(
        SESSION,
        lash_core::ExecutionScope::turn(SESSION, "host-ingress-route"),
    )?;
    let ingress_key = ingress.key("environment-sensitive-host", 0);
    let ingress_process_id = ProcessId::from(ingress_key.identity().replay_key.clone());
    let host_outcome = ingress
        .submit(
            ingress_key,
            engine_start_intent(INGRESS_ENGINE_KIND, payload.clone()),
        )
        .await;
    assert!(
        matches!(
            host_outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                ..
            }
        ),
        "host ingress must admit the environment-sensitive engine"
    );
    let ingress_identity = registry
        .get_process(&ingress_process_id)
        .await?
        .expect("host ingress registers a process")
        .identity;

    let session = core.session(SESSION).open().await?;
    let effect_host = session.effect_host();
    let scoped = effect_host.scoped(lash_core::AdmittedScope::turn(
        SESSION,
        "session-recorded-intent-route",
    ))?;
    let processes = {
        let writer = session.runtime.writer();
        let runtime = writer.lock().await;
        runtime.process_service()?
    };
    let intents =
        lash_core::ToolIntents::v3(vec![engine_start_intent(INGRESS_ENGINE_KIND, payload)]);
    let outcomes = lash_core::testing::execute_tool_intents_with_services(
        scoped,
        processes,
        &SessionId::from(SESSION),
        "environment-sensitive-session",
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    let [lash_core::ToolIntentExecutionOutcome::Executed { identity, .. }] = outcomes.as_slice()
    else {
        panic!("session recorded-intent route must execute: {outcomes:?}")
    };
    let session_identity = registry
        .get_process(&ProcessId::from(identity.replay_key.clone()))
        .await?
        .expect("session route registers a process")
        .identity;

    assert_eq!(ingress_identity, session_identity);
    assert_eq!(
        ingress_identity
            .definition
            .as_ref()
            .map(|reference| reference.definition.as_json().clone()),
        Some(serde_json::json!({
            "payload": {"program": "environment-sensitive"},
            "model": mock_model_spec().id,
            "provider": "",
        })),
        "the shared identity must prove the recorded environment reached admission"
    );
    Ok(())
}

mod redelivery;
