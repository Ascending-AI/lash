use super::*;

use lash_core::ProcessEventLogTestSupport as _;

use lash_sansio::ProcessId;

use lash_sansio::SessionId;

const SESSION: &str = "intent-ingress-session";

const SCOPE: &str = "intent-ingress-turn";

const EVENT: &str = "intent.ingress.realized";

const SIGNAL: &str = "ingress-signal";

/// The core, its registry, and the id of the fixture process every emit,
/// signal and cancel intent targets.
async fn ingress_core(
    backend: lash_core::Backend,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>, ProcessId)> {
    ingress_core_over(backend, None).await
}

async fn ingress_core_over(
    backend: lash_core::Backend,
    process_env_store: Option<Arc<dyn lash_core::ProcessExecutionEnvStore>>,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>, ProcessId)> {
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let process = registry
        .register_process_with_observers(
            lash_core::testing::held_engine_registration(
                serde_json::Value::Null,
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_extra_event_types(vec![
                lash_core::ProcessEventType {
                    name: EVENT.to_string(),
                    payload_schema: lash_core::JsonSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
                lash_core::ProcessEventType {
                    name: format!("signal.{SIGNAL}"),
                    payload_schema: lash_core::JsonSchema::any(),
                    semantics: lash_core::ProcessEventSemanticsSpec::default(),
                },
            ]),
            &[SessionId::fixture(SESSION.to_string())],
        )
        .await?
        .id;
    let core = explicit_ephemeral_facets(LashCore::standard_builder(ingress_backend(
        backend,
        process_env_store,
    )))
    .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                },
            ),
        )
        .await?;
    let _session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, registry, process))
}

/// The id of the process a start intent registered, read off the handle its
/// executed outcome answers.
fn started_process_id(outcome: &crate::tools::ToolIntentIngressOutcome) -> ProcessId {
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::StartProcess(result),
                ..
            },
        ..
    } = outcome
    else {
        panic!("the start executed: {outcome:?}");
    };
    result.process_id.clone()
}

/// Every process the registry holds.
async fn registered_process_count(registry: &Arc<dyn ProcessRegistry>) -> Result<usize> {
    Ok(registry
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await?
        .len())
}

/// `backend`, with its process-env store replaced where the test names its
/// own.
fn ingress_backend(
    backend: lash_core::Backend,
    process_env_store: Option<Arc<dyn lash_core::ProcessExecutionEnvStore>>,
) -> lash_core::Backend {
    let mut decorated = DecoratedBackend::over(backend);
    if let Some(process_env_store) = process_env_store {
        decorated = decorated.process_env_store(move |_| process_env_store);
    }
    decorated.into()
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
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ),
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
    .with_payload_schema(lash_core::JsonSchema::any());
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
    Ok(receipt.record)
}

async fn ingress_core_with_trigger_store(
    backend: lash_core::Backend,
) -> Result<(
    LashCore,
    Arc<dyn lash_core::TriggerStore>,
    lash_core::TriggerSubscriptionRecord,
    Arc<dyn ProcessRegistry>,
)> {
    let store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let subscription =
        register_ingress_trigger_subscription(store.as_ref(), backend.process_env_store().as_ref())
            .await?;
    let registry: Arc<dyn ProcessRegistry> = backend.process_registry();
    let core =
        explicit_ephemeral_facets(LashCore::standard_builder(ingress_backend(backend, None)))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(lash_core::testing::process_engine_plugin_fixture())
            .build(crate::testing::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                },
            ),
        )
        .await?;
    let _session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, store, subscription, registry))
}

fn trigger_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
        owner: crate::RuntimeOwner::Session(session_id.clone()),
        request: lash_core::TriggerOccurrenceRequest::new(
            "intent.ingress.trigger",
            "intent-ingress-source",
            serde_json::json!({"law": "host-submitted-emission"}),
            "intent-ingress-occurrence",
        ),
    })
}

async fn host_register_trigger_realizes_and_fires(backend: lash_core::Backend) -> Result<()> {
    let store = backend.trigger_store();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ),
        ),
    )
    .await?;
    let (core, _, _) = ingress_core(backend).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let register =
        lash_core::ToolIntent::RegisterTrigger(Box::new(lash_core::RegisterTriggerIntent {
            owner: crate::RuntimeOwner::Session(SessionId::from(SESSION)),
            owner_scope: lash_core::TriggerOwnerScope::session(SESSION),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(SESSION)),
            draft: lash_core::TriggerSubscriptionDraft::for_process(
                "test/host-ingress-registration",
                env_ref,
                "intent.ingress.trigger",
                "intent-ingress-source",
                lash_core::ProcessInput::Engine {
                    kind: "testing-fixture".to_string(),
                    payload: serde_json::json!({"process": "host-ingress-registration"}),
                },
                lash_core::ProcessIdentity::labelled(
                    "testing-fixture",
                    Some("host-ingress-registration"),
                ),
            )
            .with_payload_schema(lash_core::JsonSchema::any()),
        }));
    let outcome = ingress
        .submit(
            ingress
                .key("host-register-call", 0)
                .expect("a host submission handle"),
            register,
        )
        .await;
    assert!(
        matches!(
            &outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                    realized: lash_core::ToolIntentRealized::RegisterTrigger(_),
                    ..
                },
                replayed: false,
            }
        ),
        "host registration must realize: {outcome:?}"
    );
    let subscriptions = store
        .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
        .await?;
    assert_eq!(subscriptions.len(), 1);

    let emitted = ingress
        .submit(
            ingress
                .key("host-fire-call", 0)
                .expect("a host submission handle"),
            trigger_intent(&SessionId::from(SESSION)),
        )
        .await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::EmitTrigger(result),
                ..
            },
        ..
    } = emitted
    else {
        panic!("registered trigger must fire: {emitted:?}");
    };
    let report = result;
    assert_eq!(report.started_process_ids().len(), 1);
    let occurrences = store
        .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
        .await?;
    assert_eq!(occurrences.len(), 1);
    let deliveries = store
        .list_deliveries_by_occurrence_id(&occurrences[0].occurrence_id)
        .await?;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(
        deliveries[0].subscription.subscription_id,
        subscriptions[0].subscription_id
    );
    Ok(())
}

#[tokio::test]
async fn host_register_trigger_realizes_and_fires_on_sqlite_memory() -> Result<()> {
    Box::pin(host_register_trigger_realizes_and_fires(
        sqlite_memory_store_backend().await,
    ))
    .await
}

#[tokio::test]
async fn host_register_trigger_realizes_and_fires_in_sqlite() -> Result<()> {
    let directory = tempfile::tempdir().expect("SQLite test directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(directory.path().join("lash.db"))
            .await
            .expect("open SQLite store set"),
    );
    Box::pin(host_register_trigger_realizes_and_fires(
        lash_conformance::backend_over(stores),
    ))
    .await
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the optional PostgreSQL service URL"
)]
async fn host_register_trigger_realizes_and_fires_in_postgres() -> Result<()> {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url()).await?;
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open((attachments.path()).join("attachments.db"))
            .await
            .expect("SQLite attachment store")
            .attachment_store(),
    ));
    Box::pin(host_register_trigger_realizes_and_fires(
        lash_conformance::backend_over(stores),
    ))
    .await
}

/// The host front door realizes the fifth intent kind through the trigger
/// router, and re-submitting the same identity cannot emit a second time.
#[tokio::test]
async fn host_submitted_trigger_intent_emits_one_occurrence() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let (core, store, subscription, _) = ingress_core_with_trigger_store(backend).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let key = ingress
        .key("host-trigger-call", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
        .await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::EmitTrigger(result),
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
        Some(result.occurrence_id.as_str()),
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
                realized: lash_core::ToolIntentRealized::EmitTrigger(duplicate_result),
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

/// A host front door confers only its own session's registration authority:
/// a `register_trigger` intent claiming another owner scope or actor is
/// refused with a typed refusal before admission and installs nothing
/// (FIG-3116).
#[tokio::test]
async fn register_trigger_intent_claiming_foreign_authority_is_refused() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let store: Arc<dyn lash_core::TriggerStore> = backend.trigger_store();
    let env_ref = lash_core::testing::publish_process_execution_env_for_testing(
        backend.process_env_store().as_ref(),
        &lash_core::testing::host_pin_claim_for_testing(),
        &lash_core::ProcessExecutionEnvSpec::new(
            lash_core::AdmittedPluginConfig::default(),
            lash_core::SessionPolicy::new(
                lash_core::TurnBudget::Unbounded,
                lash_core::MaxToolCalls::new(1024),
            ),
        ),
    )
    .await?;
    let core =
        explicit_ephemeral_facets(LashCore::standard_builder(ingress_backend(backend, None)))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(lash_core::testing::process_engine_plugin_fixture())
            .build(crate::testing::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                },
            ),
        )
        .await?;
    let _session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;

    let draft = || {
        lash_core::TriggerSubscriptionDraft::for_process(
            "test/intent-ingress-registration",
            env_ref.clone(),
            "intent.ingress.trigger",
            "intent-ingress-source",
            lash_core::ProcessInput::Engine {
                kind: "testing-fixture".to_string(),
                payload: serde_json::json!({"process": "intent-ingress-registration"}),
            },
            lash_core::ProcessIdentity::labelled(
                "testing-fixture",
                Some("intent-ingress-registration"),
            ),
        )
        .with_payload_schema(lash_core::JsonSchema::any())
    };
    let session_id = SessionId::from(SESSION);
    let register = |owner_scope, actor| {
        lash_core::ToolIntent::RegisterTrigger(Box::new(lash_core::RegisterTriggerIntent {
            owner: crate::RuntimeOwner::Session(session_id.clone()),
            owner_scope,
            actor,
            draft: draft(),
        }))
    };
    let own_owner = lash_core::TriggerOwnerScope::session(SESSION);
    let own_actor =
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new(session_id.clone()));

    let forged_owner = lash_core::TriggerOwnerScope::session("some-other-session");
    assert_eq!(
        ingress
            .submit(
                ingress
                    .key("foreign-owner-register", 0)
                    .expect("a host submission handle"),
                register(forged_owner.clone(), own_actor.clone()),
            )
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignTriggerOwnerScope {
                expected: own_owner.clone(),
                recorded: forged_owner,
            }
        },
        "a forged owner scope is refused before admission"
    );

    let forged_actors = [
        lash_core::ProcessOriginator::host_scoped("host-binding-elsewhere"),
        lash_core::ProcessOriginator::session(lash_core::SessionScope::new("some-other-session")),
        // A frame id is an elevation this front door cannot confer.
        lash_core::ProcessOriginator::session(lash_core::SessionScope::for_agent_frame(
            session_id.clone(),
            lash_core::FrameNodeId::new("forged-frame").expect("non-empty frame id"),
        )),
    ];
    for (index, forged_actor) in forged_actors.into_iter().enumerate() {
        assert_eq!(
            ingress
                .submit(
                    ingress
                        .key(format!("foreign-actor-register-{index}"), 0)
                        .expect("a host submission handle"),
                    register(own_owner.clone(), forged_actor.clone()),
                )
                .await,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::ForeignTriggerActor {
                    expected: own_actor.clone(),
                    recorded: forged_actor,
                }
            },
            "a forged actor is refused before admission"
        );
    }

    assert_eq!(
        store
            .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
            .await?
            .len(),
        0,
        "refused registrations install nothing"
    );

    Ok(())
}

#[tokio::test]
async fn distinct_host_trigger_declarations_create_two_occurrences_and_redrive_exactly_once()
-> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let (core, store, _subscription, _) = ingress_core_with_trigger_store(backend).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let first_key = ingress
        .key("host-trigger-call-a", 0)
        .expect("a host submission handle");
    let second_key = ingress
        .key("host-trigger-call-b", 0)
        .expect("a host submission handle");

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
    let backend = sqlite_memory_store_backend().await;
    let (core, store, _, _) = ingress_core_with_trigger_store(backend).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let mut predecessor = serde_json::to_value(
        ingress
            .key("predecessor-trigger-call", 0)
            .expect("a host submission handle"),
    )?;
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

fn emit_intent(session_id: &SessionId, process: &ProcessId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::EmitProcessEvent(lash_core::EmitProcessEventIntent {
        owner: crate::RuntimeOwner::Session(session_id.clone()),
        process_id: process.clone(),
        event_type: EVENT.to_string(),
        payload: serde_json::json!({"law": "duplicate-submit"}),
    })
}

/// A held process start under the session's captured environment.
fn start_intent(session_id: &SessionId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        owner: crate::RuntimeOwner::Session(session_id.clone()),
        declaration: lash_core::ProcessStartDeclaration::new(
            lash_core::testing::held_engine_input(serde_json::Value::Null),
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(session_env_ref()),
    }))
}

/// The environment an ingress test session captures.
fn session_env_ref() -> lash_core::ProcessExecutionEnvRef {
    (lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy {
            model: Some(recorded_llm_profile(mock_llm_profile_spec())),
            ..lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
        },
    ))
    .stable_ref()
    .expect("captured environment digest")
}

fn cancel_intent(session_id: &SessionId, process: &ProcessId) -> lash_core::ToolIntent {
    cancel_intent_for_target(session_id, process)
}

fn cancel_intent_for_target(session_id: &SessionId, target: &ProcessId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
        owner: crate::RuntimeOwner::Session(session_id.clone()),
        process_id: target.clone(),
    })
}

#[tokio::test]
async fn duplicate_host_submit_returns_the_same_outcome_and_realizes_once() -> Result<()> {
    let (core, registry, process) = ingress_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let key = ingress
        .key("host-call", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(
            key.clone(),
            emit_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    let duplicate = ingress
        .submit(
            key.clone(),
            emit_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    let mut conflicting_duplicate = emit_intent(&SessionId::from(SESSION), &process);
    let lash_core::ToolIntent::EmitProcessEvent(intent) = &mut conflicting_duplicate else {
        unreachable!("fixture is an event intent")
    };
    intent.payload = serde_json::json!({"law": "same-key-different-payload"});
    let conflicting = ingress.submit(key, conflicting_duplicate).await;
    assert!(
        matches!(
            &conflicting,
            crate::tools::ToolIntentIngressOutcome::Refused {
                refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                    kind: lash_core::ToolIntentKind::EmitProcessEvent,
                },
            }
        ),
        "the same identity with different content is refused: {conflicting:?}"
    );

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
    let events = registry.full_event_window(&process, 0).await?;
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
    let (core, registry, process) = ingress_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let key = ingress
        .key("kind-swap-start-emit", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::StartProcess(_),
                ..
            },
            replayed: false,
        }
    ));

    let second = ingress
        .submit(key, emit_intent(&SessionId::from(SESSION), &process))
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
            .full_event_window(&process, 0)
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
    let (core, registry, process) = ingress_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let key = ingress
        .key("kind-swap-emit-cancel", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(
            key.clone(),
            emit_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert!(matches!(
        first,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: lash_core::ToolIntentExecutionOutcome::Executed {
                realized: lash_core::ToolIntentRealized::EmitProcessEvent(_),
                ..
            },
            replayed: false,
        }
    ));

    let second = ingress
        .submit(key, cancel_intent(&SessionId::from(SESSION), &process))
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
            .full_event_window(&process, 0)
            .await?
            .iter()
            .filter(|event| event.event_type == EVENT)
            .count(),
        1,
        "the first intent realizes once and the kind swap realizes nothing"
    );
    assert_eq!(
        registry
            .full_event_window(&process, 0)
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
async fn foreign_session_and_turn_keys_are_typed_refusals() -> Result<()> {
    let (core, registry, process) = ingress_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let foreign_session = crate::tools::ToolIntentIngressKey::derive(
        &lash_core::SessionId::from("foreign-session"),
        SCOPE,
        &lash_core::ToolCallId::fixture("host-call"),
        0,
    );
    let foreign_turn = crate::tools::ToolIntentIngressKey::derive(
        &lash_core::SessionId::from(SESSION),
        "foreign-turn",
        &lash_core::ToolCallId::fixture("host-call"),
        0,
    );

    assert!(matches!(
        ingress
            .submit(
                foreign_session,
                emit_intent(&SessionId::from(SESSION), &process)
            )
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignSession { .. }
        }
    ));
    assert!(matches!(
        ingress
            .submit(
                foreign_turn,
                emit_intent(&SessionId::from(SESSION), &process)
            )
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::ForeignExecutionScope { .. }
        }
    ));
    assert_eq!(
        registry
            .full_event_window(&process, 0)
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
    let (core, registry, process) = ingress_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let mut malformed = serde_json::to_value(crate::tools::ToolIntentIngressKey::derive(
        &lash_core::SessionId::from(SESSION),
        SCOPE,
        &lash_core::ToolCallId::fixture("host-call"),
        0,
    ))?;
    malformed["replay_key"] = serde_json::json!("forged");
    let malformed = serde_json::from_value(malformed)?;

    assert!(matches!(
        ingress
            .submit(malformed, emit_intent(&SessionId::from(SESSION), &process))
            .await,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::MalformedKey { .. }
        }
    ));
    assert_eq!(
        registry
            .full_event_window(&process, 0)
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
    let key = crate::tools::ToolIntentIngressKey::derive(
        &lash_core::SessionId::from(SESSION),
        SCOPE,
        &lash_core::ToolCallId::fixture("compat-call"),
        7,
    );
    let key_value = serde_json::to_value(&key).expect("serialize ingress key");
    for field in [
        "owner",
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
            refusal: lash_core::ToolIntentRefusalReason::IntentIndexOverflow,
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
            crate::tools::ToolIntentIngressRefusal::ForeignTriggerOwnerScope {
                expected: lash_core::TriggerOwnerScope::session(SESSION),
                recorded: lash_core::TriggerOwnerScope::session("foreign"),
            },
            &["expected", "recorded"][..],
        ),
        (
            crate::tools::ToolIntentIngressRefusal::ForeignTriggerActor {
                expected: lash_core::ProcessOriginator::session(lash_core::SessionScope::new(
                    SESSION,
                )),
                recorded: lash_core::ProcessOriginator::host_scoped("foreign"),
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

#[test]
fn ingress_start_without_lifetime_is_refused_before_submission() {
    let mut payload =
        serde_json::to_value(start_intent(&SessionId::from(SESSION))).expect("encode intent");
    fn remove_lifetime(value: &mut serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                if object.remove("lifetime").is_some() {
                    return true;
                }
                object.values_mut().any(remove_lifetime)
            }
            _ => false,
        }
    }
    assert!(
        remove_lifetime(&mut payload),
        "valid start had a required lifetime"
    );
    let error = serde_json::from_value::<lash_core::ToolIntent>(payload)
        .expect_err("missing lifetime must not decode");
    assert!(error.to_string().contains("lifetime"));
}

const INGRESS_ENGINE_KIND: &str = "ingress-admission-engine";

/// Engine registered on the ingress host, so a submitted start can be checked
/// against a kind that exists and one that does not.
struct IngressAdmissionEngine;

#[async_trait::async_trait]
impl lash_core::ProcessEngine for IngressAdmissionEngine {
    async fn check_args(
        &self,
        _signature: &lash_core::ProcessSignature,
        _args: &serde_json::Map<String, serde_json::Value>,
        _mode: lash_core::ArgsMode,
    ) -> std::result::Result<(), lash_core::ArgsMismatch> {
        Err(lash_core::ArgsMismatch::UnsupportedSignature {
            engine_kind: self.kind().into(),
        })
    }

    fn kind(&self) -> &'static str {
        INGRESS_ENGINE_KIND
    }

    fn start_artifacts(
        &self,
        _payload: &serde_json::Value,
    ) -> std::result::Result<Vec<lash_core::ArtifactName>, lash_core::PluginError> {
        Ok(Vec::new())
    }

    async fn end_artifact_referrer(
        &self,
        _cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        Ok(())
    }

    async fn acquire_engine_artifact(
        &self,
        _claim: &lash_core::ReferrerClaim,
        _artifact_ref: &str,
    ) -> std::result::Result<(), lash_core::PluginError> {
        unreachable!("the ingress engine stores no artifacts")
    }

    fn state_format(&self) -> lash_core::EngineStateFormat {
        lash_core::EngineStateFormat {
            kind: self.kind().to_owned(),
            version: 0,
        }
    }

    fn cancel_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    fn program_identity(
        &self,
        _payload: &serde_json::Value,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    fn creation_config(
        &self,
        _env_spec: &lash_core::ProcessExecutionEnvSpec,
    ) -> std::result::Result<Option<serde_json::Value>, lash_core::PluginError> {
        Ok(None)
    }

    fn advance(
        &self,
        state: lash_core::EngineState,
        _event: lash_core::EngineEvent,
    ) -> std::result::Result<
        (lash_core::EngineState, lash_core::EngineAction),
        lash_core::ProcessInfraError,
    > {
        Ok((
            state,
            lash_core::EngineAction::Terminal(lash_core::ProcessAwaitOutput::from_tool_output(
                lash_core::ToolCallOutput::success(serde_json::json!({"ingress_engine": "ran"})),
            )),
        ))
    }

    async fn resolve(
        &self,
        _reference: &lash_core::ProcessDefinitionRef,
    ) -> std::result::Result<
        lash_core::ProcessDefinitionResolution,
        lash_core::ProcessDefinitionRefusal,
    > {
        Ok(lash_core::ProcessDefinitionResolution::new(
            lash_core::ProcessSignature::Unknown,
            Vec::new(),
        ))
    }
}

fn admit_ingress_engine(
    _kind: &'static str,
    payload: &serde_json::Value,
    env: Option<&lash_core::ProcessExecutionEnvSpec>,
) -> std::result::Result<lash_core::ProcessIdentity, lash_core::PluginError> {
    if payload.get("program").and_then(serde_json::Value::as_str) == Some("invalid") {
        return Err(lash_core::PluginError::Session(
            "invalid ingress engine program".to_owned(),
        ));
    }
    let env = env.ok_or_else(|| {
        lash_core::PluginError::Session(
            "ingress admission requires the recorded execution environment".to_string(),
        )
    })?;
    let draft = lash_core::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": payload,
            "model": env.policy.profile_key().map(ToString::to_string),
        }),
        [],
    )
    .map_err(|error| lash_core::PluginError::Session(error.to_string()))?;
    let mut identity = lash_core::ProcessIdentity::labelled(
        INGRESS_ENGINE_KIND,
        payload.get("program").and_then(serde_json::Value::as_str),
    );
    identity.definition_id = Some(draft.id());
    Ok(identity)
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

impl lash_core::plugin::PluginDefinition for IngressAdmissionEngineFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("ingress-admission-engine-factory")
    }
}

async fn ingress_engine_core(
    backend: lash_core::Backend,
) -> Result<(LashCore, Arc<dyn ProcessRegistry>)> {
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .plugin(Arc::new(IngressAdmissionEngineFactory))
        .build(crate::testing::runtime_lease_owner())?;
    core.host_artifacts()
        .publish_process_env(
            &lash_core::HostArtifactPin::mint(),
            &lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                },
            ),
        )
        .await?;
    let _session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    Ok((core, registry))
}

fn engine_start_intent(kind: &str, payload: serde_json::Value) -> lash_core::ToolIntent {
    lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
        owner: crate::RuntimeOwner::Session(SessionId::fixture(SESSION.to_string())),
        declaration: lash_core::ProcessStartDeclaration::new(
            lash_core::ProcessInput::Engine {
                kind: kind.to_string(),
                payload,
            },
            lash_core::ProcessOriginator::host(),
            lash_core::Lifetime::Detached,
        )
        .with_env_ref(
            (lash_core::ProcessExecutionEnvSpec::new(
                lash_core::AdmittedPluginConfig::default(),
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                    )
                },
            ))
            .stable_ref()
            .expect("captured environment digest"),
        ),
    }))
}

fn ingress_engine_env_spec() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy {
            model: Some(recorded_llm_profile(mock_llm_profile_spec())),
            ..lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
        },
    )
}

/// The host front door records engine admission too: an unconfigured engine
/// registers no process, and an admitted one carries the engine identity stamp.
#[tokio::test]
async fn ingress_start_intent_crosses_the_engine_admission_gate() -> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;

    let unregistered_key = ingress
        .key("ingress-unregistered-engine", 0)
        .expect("a host submission handle");
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
                    refusal: lash_core::ToolIntentRefusalReason::CommandFailed { cause, .. },
                    ..
                },
            replayed: false,
        } => assert!(
            cause
                .to_string()
                .contains("process engine `ingress-engine-never-registered` is not configured"),
            "the refusal must carry the engine registry's own typed miss: {cause}"
        ),
        other => panic!("unregistered engine kind must be refused, got {other:?}"),
    }
    assert_eq!(
        registered_process_count(&registry).await?,
        0,
        "a refused start must register nothing"
    );

    let invalid = ingress
        .submit(
            ingress
                .key("ingress-invalid-engine-payload", 0)
                .expect("submission key"),
            engine_start_intent(
                INGRESS_ENGINE_KIND,
                serde_json::json!({"program": "invalid"}),
            ),
        )
        .await;
    assert!(
        matches!(
            invalid,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused { .. },
                ..
            }
        ),
        "a plugin-contributed engine must validate its payload: {invalid:?}"
    );
    assert_eq!(registered_process_count(&registry).await?, 0);

    let payload = serde_json::json!({"program": "known"});
    let admitted_key = ingress
        .key("ingress-registered-engine", 0)
        .expect("a host submission handle");
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
                    realized: lash_core::ToolIntentRealized::StartProcess(_),
                    ..
                },
                ..
            }
        ),
        "a registered engine kind must still be admitted: {admitted:?}"
    );
    let started = registry
        .get_process(&started_process_id(&admitted))
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
    let replay = ingress
        .submit(
            ingress
                .key("ingress-registered-engine", 0)
                .expect("same submission key"),
            engine_start_intent(INGRESS_ENGINE_KIND, payload),
        )
        .await;
    assert!(matches!(
        replay,
        crate::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
    ));
    assert_eq!(started_process_id(&replay), started.id);
    assert_eq!(registered_process_count(&registry).await?, 1);
    Ok(())
}

/// FIG-1838: host ingress and session-owned recorded-intent execution must feed
/// the same immutable request environment into engine admission. Otherwise an
/// environment-derived identity changes solely with the route used to start it.
#[tokio::test]
async fn equivalent_recorded_start_has_same_environment_sensitive_identity_across_routes()
-> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let payload = serde_json::json!({"program": "environment-sensitive"});

    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, "host-ingress-route"),
    )?;
    let ingress_key = ingress
        .key("environment-sensitive-host", 0)
        .expect("a host submission handle");
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
        .get_process(&started_process_id(&host_outcome))
        .await?
        .expect("host ingress registers a process")
        .identity;

    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
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
    let realization = lash_core::testing::execute_tool_intents_with_services(
        scoped,
        processes,
        &SessionId::from(SESSION),
        &lash_core::ToolCallId::fixture("environment-sensitive-session"),
        &intents,
    )
    .await
    .map_err(lash_core::PluginError::from)?;
    let outcomes = &realization.receipt.outcomes;
    let [
        lash_core::ToolIntentExecutionOutcome::Executed {
            realized: lash_core::ToolIntentRealized::StartProcess(result),
            ..
        },
    ] = outcomes.as_slice()
    else {
        panic!("session recorded-intent route must execute: {outcomes:?}")
    };
    // The session route stages its start for the outcome commit of the call
    // that declares it: the identity is the one that commit registers.
    let [staged] = realization.store_local.as_slice() else {
        panic!(
            "the session route stages one start: {:?}",
            realization.store_local
        )
    };
    let staged = lash_core::testing::staged_start_record(staged)?
        .expect("the session route stages a process start");
    assert_eq!(staged.id, result.process_id);
    let session_identity = staged.identity;

    assert_eq!(ingress_identity, session_identity);
    let expected = lash_core::ProcessDefinitionDraft::new(
        INGRESS_ENGINE_KIND,
        serde_json::json!({
            "payload": {"program": "environment-sensitive"},
            "model": "mock-model",
        }),
        [],
    )
    .expect("the environment produces a canonical definition");
    assert_eq!(
        ingress_identity.definition_id,
        Some(expected.id()),
        "the shared identity must prove the recorded environment reached admission"
    );
    Ok(())
}

mod engine_owned;

/// A process-env store whose acquisitions fail while `fail_acquire` holds.
struct FailingEnvAcquire {
    fail_acquire: std::sync::atomic::AtomicBool,
    inner: Arc<dyn lash_core::ProcessExecutionEnvStore>,
}

#[async_trait::async_trait]
impl lash_core::ProcessExecutionEnvStore for FailingEnvAcquire {
    async fn publish_process_execution_env(
        &self,
        claim: &lash_core::ReferrerClaim,
        env_ref: &lash_core::ProcessExecutionEnvRef,
        bytes: &[u8],
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        self.inner
            .publish_process_execution_env(claim, env_ref, bytes)
            .await
    }

    async fn acquire_process_execution_env(
        &self,
        claim: &lash_core::ReferrerClaim,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        if self.fail_acquire.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(lash_core::ArtifactStoreError::Backend(
                "injected process env acquisition failure".to_string(),
            ));
        }
        self.inner
            .acquire_process_execution_env(claim, env_ref)
            .await
    }

    async fn end_process_env_referrer(
        &self,
        cleanup: &lash_core::ResolvedArtifactCleanup,
    ) -> std::result::Result<(), lash_core::ArtifactStoreError> {
        self.inner.end_process_env_referrer(cleanup).await
    }

    async fn get_process_execution_env(
        &self,
        env_ref: &lash_core::ProcessExecutionEnvRef,
    ) -> std::result::Result<Option<Vec<u8>>, lash_core::ArtifactStoreError> {
        self.inner.get_process_execution_env(env_ref).await
    }
}

/// A host-submitted start whose environment acquisition fails is a typed
/// `CommandFailed` refusal that registers no process; the failure is a live
/// fault, not a recorded outcome, so a resubmission of the same identity
/// retries it and still registers nothing (re-written on SQLite memory by
/// FIG-5307).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_env_store_error_is_typed_and_registers_no_process() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let env_store = Arc::new(FailingEnvAcquire {
        fail_acquire: std::sync::atomic::AtomicBool::new(false),
        inner: backend.process_env_store(),
    });
    let (core, registry, _process) = ingress_core_over(
        backend,
        Some(Arc::clone(&env_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>),
    )
    .await?;
    env_store
        .fail_acquire
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let ingress = core.tool_intents(
        crate::SessionId::parse(SESSION).expect("nonblank host identity"),
        lash_core::ExecutionScope::turn(SESSION, SCOPE),
    )?;
    let key = ingress
        .key("start-env-store-error", 0)
        .expect("a host submission handle");
    for attempt in ["first submission", "resubmission"] {
        let outcome = ingress
            .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
            .await;
        assert!(
            matches!(
                outcome,
                crate::tools::ToolIntentIngressOutcome::Admitted {
                    outcome: lash_core::ToolIntentExecutionOutcome::Refused {
                        kind: lash_core::ToolIntentKind::StartProcess,
                        refusal: lash_core::ToolIntentRefusalReason::CommandFailed { .. },
                        ..
                    },
                    replayed: false,
                }
            ),
            "{attempt}: {outcome:?}"
        );
        assert_eq!(
            registered_process_count(&registry).await?,
            1,
            "{attempt}: only the fixture process is registered"
        );
    }
    Ok(())
}
