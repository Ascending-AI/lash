use super::*;

/// The persisted configuration a queued-run admission carries.
fn queued_run_configuration(
    session_id: &lash_core::SessionId,
) -> lash_core::PersistedSessionConfig {
    let state = lash_core::RuntimeSessionState {
        session_id: session_id.clone(),
        ..lash_core::RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ))
    };
    lash_core::RuntimeCommit::persisted_state_for_test(&state, &[]).config
}

#[tokio::test]
async fn deployment_drain_status_keeps_waiting_process_non_drained() {
    let backend = memory_store_backend().await;
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(backend, crate::TurnBudget::Unbounded).model(mock_model_spec()),
    )
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
        let backend: lash_core::Backend = memory_store_backend().await;
        let factory = backend.session_store_factory();
        let core = explicit_ephemeral_facets(
            LashCore::standard_builder(backend.clone(), crate::TurnBudget::Unbounded)
                .model(mock_model_spec()),
        )
        .build(crate::testing::runtime_lease_owner())
        .expect("build core");
        let idle = core
            .drain_status(false)
            .await
            .expect("read idle drain status");
        assert_eq!((idle.parked_turns, idle.in_flight_turns), (0, 0));
        assert!(idle.drained());

        let session_id = lash_core::SessionId::from("drain-parked-turn");
        let mut policy = lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded);
        policy.session_id = Some(session_id.clone());
        let store = factory
            .create_store(&lash_core::SessionStoreCreateRequest {
                owning_process_id: None,
                pending_observer_intents: Vec::new(),
                session_id: session_id.clone(),
                relation: lash_core::SessionRelation::default(),
                policy,
            })
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
                engine: None,
                after_redrive: None,
                build_generation: None,
            })
            .await
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
            ))
        };
        let commit = lash_core::store::RuntimeCommit::persisted_state_with_operation_for_testing(
            &state,
            &[],
            lash_core::store::OperationId::turn(
                session_id.clone(),
                lash_core::TurnId::from("parked-turn"),
                "final",
            ),
        );
        lash_core::testing::store_fixtures::commit_runtime_state_for_test(
            &store,
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

/// FIG-3659 NOW-B: `parked_work()` lists parked turns and parked processes
/// as one oldest-first page set, summarizes both kinds, follows both feeds
/// through one cursor, and `drain_status` counts the parked processes.
#[tokio::test]
async fn parked_work_merges_parked_turns_and_processes() {
    let backend: lash_core::Backend = double_backend().await;
    let factory = backend.session_store_factory();
    let registry = backend.process_registry();
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(backend.clone(), crate::TurnBudget::Unbounded)
            .model(mock_model_spec()),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build core");

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
        .expect("register the process")
        .id;
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "parked-work-process-run",
    )
    .bind_attempt(1);
    let started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    registry
        .record_first_started_with_authority(&process_id, started, &authority)
        .await
        .expect("record the process start");
    let parked_process = registry
        .park_process_with_authority(
            &process_id,
            lash_core::store::ParkReason::EffectReplayDivergence {
                effect_kind: "llm_call".to_string(),
                message: "diverged".to_string(),
            }
            .into(),
            &authority,
        )
        .await
        .expect("park the process");
    let process_park = parked_process.park.as_deref().cloned().expect("parked");

    let session_id = lash_core::SessionId::from("parked-work-turn");
    let mut policy = lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded);
    policy.session_id = Some(session_id.clone());
    let store = factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session_id.clone(),
            relation: lash_core::SessionRelation::default(),
            policy,
        })
        .await
        .expect("create the session store");
    // Parked at epoch 1, so the turn is the older park.
    let turn_park = store
        .record_turn_park(&lash_core::store::TurnParkWrite {
            session_id: session_id.clone(),
            turn_id: lash_core::TurnId::from("parked-turn"),
            reason: lash_core::store::ParkReason::ReplayDivergence {
                message: "diverged".to_string(),
            },
            at_ms: 1,
            engine: None,
            after_redrive: None,
            build_generation: None,
        })
        .await
        .expect("park the turn");

    let parked = core.parked_work();
    let limit = std::num::NonZeroUsize::new(1).expect("non-zero");
    let first = parked
        .list(&crate::ParkedWorkQuery::all(limit))
        .await
        .expect("list the first page");
    assert_eq!(
        first
            .records
            .iter()
            .map(|record| record.target.clone())
            .collect::<Vec<_>>(),
        vec![crate::ParkedWorkRef::Turn {
            session_id: session_id.clone(),
            turn_id: lash_core::TurnId::from("parked-turn"),
        }],
        "the older park comes first"
    );
    assert_eq!(first.records[0].park_id, turn_park.park_id);
    let mut next = crate::ParkedWorkQuery::all(limit);
    next.after = Some(first.next.clone().expect("a second page follows"));
    let second = parked.list(&next).await.expect("list the second page");
    assert_eq!(
        second
            .records
            .iter()
            .map(|record| (record.target.clone(), record.park_id, record.attempts))
            .collect::<Vec<_>>(),
        vec![(
            crate::ParkedWorkRef::Process {
                process_id: process_id.clone(),
            },
            process_park.park_id,
            1,
        )]
    );
    assert_eq!(second.next, None, "nothing follows the last park");

    let summary = parked.summary().await.expect("summarize parked work");
    assert_eq!((summary.turns.total(), summary.processes.total()), (1, 1));
    assert_eq!(summary.oldest_since_ms(), Some(1));

    let events = parked
        .events(
            &crate::ParkedWorkEventsCursor::initial(),
            std::num::NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("follow both park feeds");
    assert_eq!(
        events
            .events
            .iter()
            .map(|event| (event.target.clone(), event.kind.kind_code()))
            .collect::<Vec<_>>(),
        vec![
            (
                crate::ParkedWorkRef::Turn {
                    session_id: session_id.clone(),
                    turn_id: lash_core::TurnId::from("parked-turn"),
                },
                "parked"
            ),
            (
                crate::ParkedWorkRef::Process {
                    process_id: process_id.clone(),
                },
                "parked"
            ),
        ],
        "both feeds merge by transition time"
    );
    let resumed = parked
        .events(
            &events.next,
            std::num::NonZeroUsize::new(16).expect("non-zero"),
        )
        .await
        .expect("resume both feeds");
    assert!(resumed.events.is_empty(), "a resume repeats nothing");

    let status = core.drain_status(false).await.expect("read drain status");
    assert_eq!(status.parked_processes, 1);
    assert_eq!(status.parked_turns, 1);
    assert_eq!(status.oldest_parked_since_ms, Some(1));
    let wire = serde_json::to_value(&status).expect("serialize drain status");
    assert_eq!(wire["parked_processes"], 1);
}

#[tokio::test]
async fn testing_facade_run_tool_executes_provider() {
    let outcome = crate::testing::run_tool(
        &AppTools,
        "app_lookup",
        &serde_json::json!({ "query": "weather" }),
    )
    .await;

    let lash_core::ToolAttemptOutcome::Done { result, intents } = outcome else {
        panic!("app_lookup must complete inline");
    };
    assert!(intents.is_empty());
    let output = result.into_output();
    assert!(output.is_success());
    assert_eq!(
        output.value_for_projection(),
        serde_json::json!({ "ok": true })
    );
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
}

#[tokio::test]
async fn testing_facade_run_tool_granted_honors_the_granted_source_binding() {
    let grant = lash_core::ToolExecutionGrant::from_definition(grant_bound_tool_definition())
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

/// FIG-3799: a core marks another build's generation draining, never its
/// own, and the generation's status counts the live processes its segments
/// hold until they are gone; the mark is what makes it drainable at all.
#[tokio::test]
async fn generation_drain_status_counts_the_generations_live_processes() {
    let backend = memory_store_backend().await;
    let registry = backend.process_registry();
    let own = backend.build_generation().clone();
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(backend, crate::TurnBudget::Unbounded).model(mock_model_spec()),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build core with a process registry");
    let retired = lash_core::engine::BuildGeneration::for_test("fig-3799-retired");
    assert_ne!(retired, own);

    let refused = core
        .drain_generation(&own)
        .await
        .expect_err("a core never drains its own generation");
    assert!(
        matches!(&refused, crate::EmbedError::DrainOwnGeneration { generation } if *generation == own),
        "{refused:?}"
    );
    assert!(refused.is_terminal() && !refused.is_retryable());

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
        .expect("register the retired generation's process")
        .id;
    let authority = lash_core::ProcessExecutionWriteAuthority::invocation(
        process_id.clone(),
        "generation-drain-status-run",
    )
    .bind_attempt(1);
    let mut started = authority
        .invocation_started()
        .expect("attempt-bound invocation has a start fact");
    started.build_generation = Some(retired.clone());
    registry
        .record_first_started_with_authority(&process_id, started, &authority)
        .await
        .expect("start the process under the retired generation");

    // FIG-3884: a queued run the retired generation's drive admitted counts
    // as its in-flight turn until the run settles.
    let turn_session = lash_core::SessionId::from("generation-drain-status-turn");
    let session_store = core
        .backend()
        .session_store_factory()
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: turn_session.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        })
        .await
        .expect("create the in-flight turn's session");
    let lease = session_store
        .try_claim_session_execution_lease_with_token(
            &turn_session,
            &crate::testing::runtime_lease_owner(),
            "generation-drain-status-executor",
            &lash_core::LeaseClaimNonce::for_testing("generation-drain-status-token"),
            60_000,
        )
        .await
        .expect("claim the session lease")
        .acquired()
        .expect("a fresh session's lease is free");
    let expected_head_revision = session_store
        .load_session_head_meta()
        .await
        .expect("read the head")
        .map_or(0, |head| head.head_revision);
    let drain_scope = lash_core::ExecutionScope::queue_drain(
        turn_session.clone(),
        "generation-drain-status-drain",
    );
    let admission = session_store
        .begin_or_resume_queued_run(
            &lease.fence(),
            lash_core::store::BeginQueuedRun {
                session_id: turn_session.clone(),
                identity: Some(drain_scope.clone()),
                request: lash_core::store::QueuedRunRequest::Automatic,
                configuration: queued_run_configuration(&turn_session),
                expected_head_revision,
                initial_turn_index: 1,
                generation: None,
                admitted_generation: retired.clone(),
            },
        )
        .await
        .expect("begin the queued run");

    let unmarked = core
        .generation_drain_status(&retired)
        .await
        .expect("read the unmarked generation");
    assert_eq!(
        (
            unmarked.draining_since_ms,
            unmarked.live_processes,
            unmarked.in_flight_turns
        ),
        (None, 1, 1)
    );
    assert!(!unmarked.drained(), "an unmarked generation is not drained");

    assert!(core.drain_generation(&retired).await.expect("mark"));
    assert!(
        !core.drain_generation(&retired).await.expect("mark again"),
        "the first mark stands"
    );
    let draining = core
        .generation_drain_status(&retired)
        .await
        .expect("read the draining generation");
    assert!(draining.draining_since_ms.is_some());
    assert_eq!(
        (
            draining.live_processes,
            draining.parked_processes,
            draining.parked_turns,
            draining.in_flight_turns,
        ),
        (1, 0, 0, 1)
    );
    assert_eq!(
        draining.stalled_obligations.len(),
        lash_core::store::ObligationKind::ALL.len(),
        "every obligation kind is read"
    );
    assert!(!draining.drained(), "a live process holds the generation");
    let wire = serde_json::to_value(&draining).expect("serialize the status");
    assert_eq!(wire["drained"], serde_json::json!(false));
    assert_eq!(wire["live_processes"], serde_json::json!(1));
    assert_eq!(wire["in_flight_turns"], serde_json::json!(1));

    let selected = session_store
        .select_queued_run(
            &lease.fence(),
            &drain_scope,
            &lease.owner,
            64,
            &admission.configuration,
            lash_core::testing::queued_work_claim_policy(64),
        )
        .await
        .expect("freeze the empty selection");
    session_store
        .settle_queued_run(
            &lease.fence(),
            lash_core::store::QueuedRunCommit {
                scope: drain_scope,
                expected_revision: selected.admission.revision,
                progress: lash_core::store::QueuedRunProgress::Settle {
                    terminal: lash_core::store::QueuedRunTerminal::Empty,
                },
            },
        )
        .await
        .expect("settle the queued run");
    let still_held = core
        .generation_drain_status(&retired)
        .await
        .expect("read the status with the turn settled");
    assert_eq!(still_held.in_flight_turns, 0);
    assert!(
        !still_held.drained(),
        "the live process still holds the generation"
    );

    registry
        .complete_process(
            &process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            lash_core::ProcessCompletionAuthority::workflow_key("generation-drain-status-run"),
        )
        .await
        .expect("end the retired generation's process");
    let emptied = core
        .generation_drain_status(&retired)
        .await
        .expect("read the emptied generation");
    assert_eq!(emptied.live_processes, 0);
    assert!(emptied.drained(), "{emptied:?}");

    assert!(core.end_generation_drain(&retired).await.expect("clear"));
    let cleared = core
        .generation_drain_status(&retired)
        .await
        .expect("read the cleared generation");
    assert_eq!(cleared.draining_since_ms, None);
    assert!(!cleared.drained());
}

/// FIG-3873 S4: a closing session keeps every draining generation undrained
/// until its physical delete runs. Its close ended its roots, but each
/// root's turn-control waits stay registered with the engine, on whichever
/// build ran the root, until the delete revokes them: a generation retired
/// before then would strand them.
#[tokio::test]
async fn a_closing_session_holds_a_generation_drain_until_its_physical_delete() {
    let backend = memory_store_backend().await;
    let factory = backend.session_store_factory();
    let clock = backend.clock();
    let core = explicit_ephemeral_facets(
        LashCore::standard_builder(backend, crate::TurnBudget::Unbounded).model(mock_model_spec()),
    )
    .build(crate::testing::runtime_lease_owner())
    .expect("build the core");
    let retired = lash_core::engine::BuildGeneration::for_test("fig-3873-s4-retired");
    assert!(core.drain_generation(&retired).await.expect("mark"));
    let session = lash_core::SessionId::from("fig-3873-s4-closing");
    factory
        .create_store(&lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: session.clone(),
            relation: lash_core::SessionRelation::Root,
            policy: lash_core::SessionPolicy::new(crate::TurnBudget::Unbounded),
        })
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
