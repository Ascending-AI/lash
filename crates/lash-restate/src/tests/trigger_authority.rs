use super::*;

const TRIGGER_INTENT_CUTOVER_SESSION: &str = "trigger-intent-cutover-session";
const TRIGGER_INTENT_CUTOVER_TURN: &str = "trigger-intent-cutover-turn";

#[tokio::test]
async fn restate_double_refuses_foreign_register_trigger_authority_before_effects() {
    for foreign_field in ["owner_scope", "actor", "legitimate"] {
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set");
        let registry: Arc<dyn ProcessRegistry> = stores.process_registry();
        let store: Arc<dyn TriggerStore> = stores.trigger_store();
        let env_store = stores.process_env_store();
        // The registration holds the revision it commits, on its target engine's
        // artifacts, under the store set's artifact ports (ADR 0113 §3.4).
        let engines = lash_core::testing::process_engine_fixture().with_artifact_ports(
            lash_core::ArtifactReferrerPorts::new(
                lash_core::StoreSet::module_artifacts(&stores),
                stores.process_env_store(),
                lash_core::StoreSet::definition_store(&stores),
                lash_core::StoreSet::attachment_referrers(&stores),
                lash_core::StoreSet::artifact_cleanup(&stores),
                Arc::new(lash_core::facade_support::SystemClock),
            ),
        );
        let env_ref = lash_core::testing::process_execution_env_fixture(env_store.as_ref()).await;
        let mut registration = lash_core::RegisterTriggerIntent {
            owner: lash_core::RuntimeOwner::Session(SessionId::from(
                TRIGGER_INTENT_CUTOVER_SESSION,
            )),
            owner_scope: lash_core::TriggerOwnerScope::session(TRIGGER_INTENT_CUTOVER_SESSION),
            actor: lash_core::ProcessOriginator::session(lash_core::SessionScope::for_agent_frame(
                TRIGGER_INTENT_CUTOVER_SESSION,
                lash_core::FrameNodeId::new("test-frame").expect("test frame id"),
            )),
            draft: lash_core::TriggerSubscriptionDraft::for_process(
                format!("test/restate-foreign-{foreign_field}"),
                env_ref,
                "intent.restate.foreign",
                "source",
                lash_core::ProcessInput::Engine {
                    kind: "testing-fixture".to_string(),
                    payload: serde_json::Value::Null,
                },
                lash_core::ProcessIdentity::new("testing-fixture"),
            ),
        };
        if foreign_field == "owner_scope" {
            registration.owner_scope = lash_core::TriggerOwnerScope::session("foreign");
        } else if foreign_field == "actor" {
            registration.actor = lash_core::ProcessOriginator::host_scoped("foreign");
        }
        let scope =
            ExecutionScope::turn(TRIGGER_INTENT_CUTOVER_SESSION, TRIGGER_INTENT_CUTOVER_TURN);
        let context = Arc::new(ReplayableRecordingContext::default());
        let controller = RestateRuntimeEffectController::new_for_test(Arc::clone(&context));
        let router = lash_core::facade_support::TriggerRouter::new(
            Arc::clone(&store),
            lash_core::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
        );
        let outcomes = lash_core::testing::execute_tool_intents_with_services_and_trigger_router(
            controller
                .scoped_effect_controller(durable_admission(&scope))
                .expect("scope Restate controller"),
            lash_core::testing::effect_backed_process_service(registry, env_store),
            router,
            engines,
            &SessionId::from(TRIGGER_INTENT_CUTOVER_SESSION),
            &lash_core::ToolCallId::fixture("foreign-trigger-call"),
            &lash_core::ToolIntents::v3(vec![lash_core::ToolIntent::RegisterTrigger(Box::new(
                registration,
            ))]),
        )
        .await
        .expect("drain foreign registration");
        let subscriptions = store
            .list_subscriptions(lash_core::TriggerSubscriptionFilter::default())
            .await
            .expect("read subscriptions");
        match foreign_field {
            "owner_scope" => assert!(
                matches!(
                    outcomes.as_slice(),
                    [lash_core::ToolIntentExecutionOutcome::Refused {
                        refusal: lash_core::ToolIntentRefusalReason::ForeignTriggerOwnerScope { .. },
                        ..
                    }]
                ),
                "{outcomes:?}"
            ),
            "actor" => assert!(
                matches!(
                    outcomes.as_slice(),
                    [lash_core::ToolIntentExecutionOutcome::Refused {
                        refusal: lash_core::ToolIntentRefusalReason::ForeignTriggerActor { .. },
                        ..
                    }]
                ),
                "{outcomes:?}"
            ),
            "legitimate" => assert!(
                matches!(
                    outcomes.as_slice(),
                    [lash_core::ToolIntentExecutionOutcome::Executed { .. }]
                ),
                "{outcomes:?}"
            ),
            _ => unreachable!(),
        }
        assert_eq!(
            subscriptions.len(),
            usize::from(foreign_field == "legitimate")
        );
        if foreign_field != "legitimate" {
            assert!(
                context.journal_commands.lock_recover().is_empty(),
                "{foreign_field} must not reach Restate effect admission"
            );
        }
    }
}
