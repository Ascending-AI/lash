use super::*;
use lash_facade_tests::ingress::*;

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

/// The host front door records engine admission too: an unconfigured engine
/// registers no process, and an admitted one carries the engine identity stamp.
#[tokio::test]
async fn ingress_start_intent_crosses_the_engine_admission_gate() -> Result<()> {
    let (core, registry) = ingress_engine_core(sqlite_memory_store_backend().await).await?;
    let ingress = core.tool_intents(
        lash::SessionId::parse(SESSION).expect("nonblank host identity"),
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
        lash::tools::ToolIntentIngressOutcome::Admitted {
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
            lash::tools::ToolIntentIngressOutcome::Admitted {
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
            lash::tools::ToolIntentIngressOutcome::Admitted {
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
        admit_ingress_engine(INGRESS_ENGINE_KIND, &payload).expect("known payload"),
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
        lash::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
    ));
    assert_eq!(started_process_id(&replay), started.id);
    assert_eq!(registered_process_count(&registry).await?, 1);
    Ok(())
}

/// DECLARED-SETTINGS-REALIZE (D4, FIG-6061): a start declared under settings
/// S registers under S, though the node that registers it defaults to S′.
///
/// Node A submits a host engine start stated under A's own defaults. The
/// submission ledger admits it, its declaration with it, and the start is
/// refused before registration: the environment its launch names is not yet
/// published. Node B, whose engine defaults differ, realizes the same
/// identity once the environment is published.
#[tokio::test]
async fn a_journaled_host_start_registers_the_settings_it_declared() -> Result<()> {
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let node = |defaults: u64, boot: &str| {
        ingress_node_builder(&backend, Some(serde_json::json!({ "budget": defaults }))).build(
            lash_core::LeaseOwnerIdentity::opaque(
                lash_core::LeaseOwnerId::new(format!("declared-settings-{boot}")),
                lash_core::LeaseIncarnationId::new(format!("declared-settings-{boot}-boot")),
            ),
        )
    };
    let first = node(5, "first")?;
    let later = node(6, "later")?;
    let session = lash::SessionId::parse(SESSION).expect("nonblank host identity");
    let _session = created_session(&first, session.clone())
        .await
        .open()
        .await?;
    let payload = serde_json::json!({"program": "known"});
    let declared = first
        .processes()
        .engine_defaults(INGRESS_ENGINE_KIND, &payload)?;
    let restarted = later
        .processes()
        .engine_defaults(INGRESS_ENGINE_KIND, &payload)?;
    assert_ne!(declared, restarted, "the nodes' defaults differ");
    // An environment no node has published until A's submission refused.
    let env = lash_core::testing::process_env_with_limit(61);
    let env_ref = env.stable_ref().expect("the environment encodes");
    let start = || {
        lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
            owner: lash::RuntimeOwner::Session(session.clone()),
            declaration: lash_core::ProcessStartDeclaration::engine(
                INGRESS_ENGINE_KIND,
                payload.clone(),
                lash_core::testing::declared_launch_for_testing(lash_core::EngineLaunch::new(
                    env_ref.clone(),
                    declared.clone(),
                )),
                lash_core::ProcessOriginator::host(),
                lash_core::Lifetime::Detached,
            ),
        }))
    };
    let scope = || lash_core::ExecutionScope::turn(SESSION, "declared-settings");
    let ingress = first.tool_intents(session.clone(), scope())?;
    let refused = ingress
        .submit(
            ingress.key("declared-settings", 0).expect("submission key"),
            start(),
        )
        .await;
    assert!(
        matches!(
            &refused,
            lash::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Refused { .. },
                ..
            }
        ),
        "A's submission is admitted and refused before registration: {refused:?}"
    );
    assert_eq!(registered_process_count(&registry).await?, 0);

    later
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &env)
        .await?;
    let ingress = later.tool_intents(session.clone(), scope())?;
    let admitted = ingress
        .submit(
            ingress.key("declared-settings", 0).expect("submission key"),
            start(),
        )
        .await;
    let row = registry
        .get_process(&started_process_id(&admitted))
        .await?
        .expect("B registers the start");
    assert_eq!(
        row.engine_settings(),
        declared.as_ref(),
        "the row records the declared settings, not the registering node's"
    );
    Ok(())
}
