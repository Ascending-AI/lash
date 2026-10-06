use super::*;

#[tokio::test]
async fn deployment_drain_status_keeps_waiting_process_non_drained() {
    let backend = sqlite_memory_store_backend().await;
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("build core with a process registry");
    let process_id = registry
        .register_process(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::Engine {
                    kind: "test-engine".to_string(),
                    payload: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(lash_core::ProcessExecutionEnvRef::new(
                "process-env:test-engine",
            ))),
        )
        .await
        .expect("register waiting process")
        .id;
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "deployment-drain-status-waiting-run",
    )
    .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    registry
        .record_first_started_with_authority(&process_id, started, &authority)
        .await
        .expect("record process start");
    registry
        .set_process_wait_with_authority(
            &process_id,
            lash_core::WaitState {
                since_ms: 1,
                kind: lash_core::WaitKind::Signal {
                    name: "deployment-drain-status".to_string(),
                    event_type: "deployment.drain_status".to_string(),
                    key: "deployment-drain-status-waiting:signal".to_string(),
                    ordinal: 1,
                },
            },
            Vec::new(),
            &authority,
        )
        .await
        .expect("set process waiting");

    let status = core
        .drain_status(false)
        .await
        .expect("read deployment drain status");
    assert_eq!(status.remaining_invocations, 1);
    assert!(!status.drained());
}

/// FIG-3586: a parked turn keeps the deployment from reporting drained, and
/// its commit releases it.
#[tokio::test]
async fn deployment_drain_status_counts_parked_and_in_flight_turns() {
    {
        let backend: lash_core::Backend = sqlite_memory_store_backend().await;
        let factory = backend.session_store_factory();
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .build(crate::testing::runtime_lease_owner())
            .expect("build core");
        let idle = core
            .drain_status(false)
            .await
            .expect("read idle drain status");
        assert_eq!((idle.parked_turns, idle.in_flight_turns), (0, 0));
        assert!(idle.drained());

        let session_id = lash_core::SessionId::from("drain-parked-turn");
        let policy = lash_core::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        );
        let store = lash_core::runtime::admit_session_view(
            &factory,
            &lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::default(),
                config: (&policy).into(),
                head: lash_core::SessionCreationHead::Config,
            },
        )
        .await
        .expect("create the session store");
        let stored = store
            .record_turn_park(&lash_core::store::TurnParkWrite {
                session_id: session_id.clone(),
                turn_id: lash_core::TurnId::from("parked-turn"),
                reason: lash_core::store::ParkReason::ReplayDivergence {
                    message: "diverged".to_string(),
                },
                at_ms: 1,
                origin: lash_core::store::TurnParkOrigin::Refusal,
                build_generation: None,
            })
            .await
            .map(lash_core::store::StoreTransition::into_record)
            .expect("park the turn");
        assert_eq!(stored.since_ms, 1);
        let parked = core
            .drain_status(false)
            .await
            .expect("read parked drain status");
        assert_eq!((parked.parked_turns, parked.in_flight_turns), (1, 1));
        assert_eq!(
            parked.oldest_parked_since_ms,
            Some(1),
            "the drain status exposes the oldest park"
        );
        assert!(!parked.drained(), "a parked turn is not drained");
        let wire = serde_json::to_value(&parked).expect("serialize drain status");
        assert_eq!(wire["parked_turns"], 1);
        assert_eq!(wire["oldest_parked_since_ms"], 1);
        assert_eq!(wire["drained"], false);

        let state = lash_core::RuntimeSessionState {
            session_id: session_id.clone(),
            ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ))
        };
        let commit = lash_core::store::RuntimeCommit::persisted_state_with_operation_for_testing(
            &state,
            lash_core::store::OperationId::turn(
                session_id.clone(),
                lash_core::TurnId::from("parked-turn"),
                "final",
            ),
        );
        // A final commit lands only under its run's admission (FIG-4848).
        let commit = lash_conformance::prepare_final_commit(store.store(), commit).await;
        lash_core::testing::store_fixtures::commit_runtime_state_for_test(
            store.store(),
            commit,
            "drain-settler",
        )
        .await
        .expect("commit settles the parked turn");
        let settled = core
            .drain_status(false)
            .await
            .expect("read settled drain status");
        assert_eq!((settled.parked_turns, settled.in_flight_turns), (0, 0));
        assert_eq!(settled.oldest_parked_since_ms, None);
        assert!(settled.drained());
    }
}

/// A provider whose `execute` forwards through its granted branch only when
/// the attempt context carries the grant's execution binding — the same seam
/// a deferred-resolution host branches on (FIG-3436). Its tool is a grant
/// target, not a catalog member, so `tool_manifests` is empty.
struct GrantBoundTools;

#[async_trait]
impl ToolProvider for GrantBoundTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        Vec::new()
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<lash_core::ToolContract>> {
        None
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!({
                "binding": call.context.tool_execution_binding().clone(),
            }))
        })
        .await
        .into()
    }
}

fn grant_bound_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:grant_bound",
        "grant_bound",
        "Executes only under a granted route.",
        serde_json::json!({ "type": "object", "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
}

#[tokio::test]
async fn testing_facade_run_tool_granted_honors_the_granted_source_binding() {
    let grant = lash_core::ToolExecutionGrant::from_definition(
        lash_core::plugin::PluginRevision::new("mock", lash_core::plugin::BehaviorRevision::ONE),
        grant_bound_tool_definition(),
    )
    .with_source_id("grant-source")
    .with_execution_binding(serde_json::json!({ "route": "deferred" }));
    let args = serde_json::json!({});

    let outcome = crate::testing::run_tool_granted(&GrantBoundTools, &grant, &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("granted call must complete inline");
    };
    assert!(intents.is_empty());
    let output = result.into_output();
    assert!(output.is_success());
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "binding": { "route": "deferred" } })
    );

    // The ungranted route cannot admit the tool: its manifest lives on the
    // grant, outside the provider's catalog membership.
    let outcome = crate::testing::run_tool(&GrantBoundTools, "grant_bound", &args).await;
    let lash_core::ToolAttemptOutcome::Done { result, .. } = outcome else {
        panic!("the catalog-route probe resolves to a completed failure");
    };
    assert!(!result.into_output().is_success());
}

/// FIG-3873 S4: a closing session keeps every draining generation undrained
/// until its physical delete runs. Its close ended its runs, but each
/// run's turn-control waits stay registered with the engine, on whichever
/// build ran the run, until the delete revokes them: a generation retired
/// before then would strand them.
#[tokio::test]
async fn a_closing_session_holds_a_generation_drain_until_its_physical_delete() {
    let backend = sqlite_memory_store_backend().await;
    let factory = backend.session_store_factory();
    let clock = backend.clock();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .build(crate::testing::runtime_lease_owner())
        .expect("build the core");
    let retired = lash_core::engine::BuildGeneration::for_test("fig-3873-s4-retired");
    assert!(core.drain_generation(&retired).await.expect("mark"));
    let session = lash_core::SessionId::from("fig-3873-s4-closing");
    lash_core::runtime::admit_session_view(
        &factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            )
            .into(),
            head: lash_core::SessionCreationHead::Config,
        },
    )
    .await
    .expect("create the session");
    let open = core
        .generation_drain_status(&retired)
        .await
        .expect("read the drain beside an open session");
    assert_eq!(open.closing_sessions, 0);
    assert!(open.drained(), "{open:?}");

    factory
        .begin_session_close(&session, clock.timestamp_ms())
        .await
        .expect("close the session")
        .expect("the session exists");
    let closing = core
        .generation_drain_status(&retired)
        .await
        .expect("read the drain beside a closing session");
    assert_eq!(closing.closing_sessions, 1);
    assert!(!closing.drained(), "a closing session holds the drain");
    let wire = serde_json::to_value(&closing).expect("serialize the status");
    assert_eq!(wire["closing_sessions"], serde_json::json!(1));
    assert_eq!(wire["drained"], serde_json::json!(false));

    factory
        .delete_session(&session)
        .await
        .expect("the physical delete");
    let deleted = core
        .generation_drain_status(&retired)
        .await
        .expect("read the drain after the physical delete");
    assert_eq!(deleted.closing_sessions, 0);
    assert!(deleted.drained(), "{deleted:?}");
}
