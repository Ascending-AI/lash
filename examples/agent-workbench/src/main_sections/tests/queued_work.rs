fn queued_work_test_draft(
    session_id: &str,
    source_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::QueuedWorkBatchDraft::new(
        session_id,
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        vec![lash::persistence::QueuedWorkPayload::agent_frame_task(
            "workbench-queued-work-test-frame",
            source_key,
            None,
        )],
    )
    .with_source_key(source_key)
}

fn workbench_process_wake_draft(
    wake: lash::process::ProcessWakeDelivery,
) -> lash::persistence::QueuedWorkBatchDraft {
    let source_key = lash::process::process_wake_source_key(&wake.process_id, wake.sequence);
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    lash::persistence::QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        vec![lash::persistence::QueuedWorkPayload::process_wake(wake)],
    )
    .with_merge_key(lash::persistence::PROCESS_WAKE_MERGE_KEY)
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
}

#[test]
fn workbench_lists_and_controls_individual_queued_batches() {
    run_async_test_on_stack_budget("workbench-queued-work-controls", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-queued-controls-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create queued-work controls dir");
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let mut state = recoverable_chat_test_state_with_dependencies(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-queued-controls-test")
                .complete_error("queued-work controls should not call the provider")
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
        )
        .await;
        state.restate_ingress_url = restate_ingress_url;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open queued-work controls session");
        let cursor = session.observe().current_observation().cursor;
        let store = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open queued-work controls store");
        let first = store
            .enqueue_queued_work(queued_work_test_draft(
                &session_id,
                "workbench-control:first",
            ))
            .await
            .expect("enqueue first controlled batch");
        let second = store
            .enqueue_queued_work(queued_work_test_draft(
                &session_id,
                "workbench-control:second",
            ))
            .await
            .expect("enqueue second controlled batch");

        let Json(listed) = list_queued_work(
            State(state.clone()),
            Query(SessionQuery::default()),
        )
        .await
        .expect("list workbench queued work");
        assert_eq!(
            listed
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![first.batch_id.as_str(), second.batch_id.as_str()]
        );

        let Json(run) = run_queued_work_batch(
            AxumPath(second.batch_id.clone()),
            State(state.clone()),
            Query(SessionQuery::default()),
        )
        .await
        .expect("submit selected queued batch");
        assert!(run.accepted);
        assert_eq!(run.batch_id, second.batch_id);
        let submitted = tokio::time::timeout(Duration::from_secs(1), restate_requests.recv())
            .await
            .expect("selected queued turn submission")
            .expect("selected queued turn request body");
        assert_eq!(
            submitted
                .pointer("/body/batch_ids/0")
                .and_then(Value::as_str),
            Some(second.batch_id.as_str()),
            "removing TurnBuilder::batch_ids from the workbench must fail this contract"
        );
        assert_eq!(
            submitted.pointer("/body/drain_id").and_then(Value::as_str),
            Some(format!("workbench-queued-batch:{}", second.batch_id).as_str()),
            "a selected batch must carry a stable drain idempotency key"
        );

        let Json(cancelled) = cancel_queued_work_batch(
            AxumPath(first.batch_id.clone()),
            State(state.clone()),
            Query(SessionQuery::default()),
        )
        .await
        .expect("cancel first queued batch");
        assert!(cancelled.accepted);
        assert_eq!(cancelled.batch_id, first.batch_id);
        let remaining = session.queued_work().await.expect("list after cancel");
        assert_eq!(
            remaining
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![second.batch_id.as_str()]
        );
        let lash::observe::SessionResume::Replayed { events } =
            session.observe().resume_from_cursor(&cursor).expect("resume queue events")
        else {
            panic!("recent workbench cursor must replay queue events");
        };
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            lash::observe::SessionObservationEventPayload::QueueChanged { kind, batch_ids }
                if *kind == lash::observe::SessionQueueEventKind::Cancelled
                    && batch_ids.as_slice() == std::slice::from_ref(&first.batch_id)
        )));

        assert!(ui::INDEX_HTML.contains("id=\"queuedWorkList\""));
        assert!(ui::INDEX_HTML.contains("Run only this queued-work batch now"));
        assert!(ui::INDEX_HTML.contains("Cancel this pending queued-work batch"));
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

#[test]
fn workbench_handles_typed_selected_drain_refusal_and_reselects() {
    run_async_test_on_stack_budget("workbench-selected-drain-refusal", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-selected-drain-refusal-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create selected-drain refusal dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        );
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-selected-drain-refusal-test")
                .complete(|_| async {
                    Ok(text_response(
                        "<lashlang>\nfinish \"processed selected row\"\n</lashlang>",
                    ))
                })
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
            32_768,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open selected-drain refusal session");
        let store = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open selected-drain refusal store");
        for (source_key, merge_key) in [
            ("workbench-selected-a1", "a"),
            ("workbench-selected-b1", "b"),
            ("workbench-selected-a2", "a"),
        ] {
            store
                .enqueue_queued_work(
                    queued_work_test_draft(&session_id, source_key).with_merge_key(merge_key),
                )
                .await
                .expect("enqueue selected-drain refusal row");
        }

        let error = session
            .queued_turn()
            .batch_ids(["recording-qwb-1", "recording-qwb-3"])
            .run()
            .await
            .expect_err("a key break refuses the original selected set");
        match error {
            lash::EmbedError::SelectedQueuedWorkDrainRefused { cause } => match cause {
                lash::SelectedQueuedWorkDrainRefusalCause::UnclaimableTogether {
                    unclaimed_batch_ids,
                } => assert_eq!(unclaimed_batch_ids, vec!["recording-qwb-3".to_string()]),
                lash::SelectedQueuedWorkDrainRefusalCause::
                    InterruptedBatchRequiresFullComposition { required_batch_ids } => panic!(
                    "key-break example did not create interrupted composition {required_batch_ids:?}"
                ),
                lash::SelectedQueuedWorkDrainRefusalCause::ExecutionLaneBusy => {
                    panic!("key-break example does not hold the execution lane")
                }
            },
            other => panic!("expected typed unclaimable-together refusal, got {other:?}"),
        }

        let output = session
            .queued_turn()
            .batch_ids(["recording-qwb-1"])
            .run()
            .await
            .expect("re-select the claimable prefix")
            .expect("the re-selected row executes");
        assert_eq!(
            output
                .activities
                .iter()
                .find_map(|activity| match &activity.event {
                    lash::TurnEvent::QueuedWorkStarted { batch_ids, .. } => {
                        Some(batch_ids.clone())
                    }
                    _ => None,
                }),
            Some(vec!["recording-qwb-1".to_string()])
        );
        assert_eq!(
            session
                .queued_work()
                .await
                .expect("list after selected-drain re-selection")
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec!["recording-qwb-2", "recording-qwb-3"]
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

#[test]
fn targeted_workbench_drain_preserves_earlier_wake_and_absorbs_live_redelivery() {
    run_async_test_on_stack_budget("workbench-targeted-wake-drain", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-targeted-wake-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create targeted wake dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-targeted-wake-test")
                .complete(|_| async {
                    Ok(text_response(
                        "<lashlang>\nfinish \"processed wake\"\n</lashlang>",
                    ))
                })
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
            32_768,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open targeted wake session");
        let target = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open targeted wake receiver");

        let clock = Arc::new(
            lash_core::testing::TestClock::new(
                1_800_000_000_000,
            ),
        );
        let wake_delivery_config = lash::process::WakeDeliveryConfig::new(10_000)
            .expect("valid wake expiry")
            .with_enqueuing_stale_after_ms(25)
            .expect("valid stale claim age");
        assert_eq!(wake_delivery_config.delivery_expiry_ms, 10_000);
        assert_eq!(wake_delivery_config.enqueuing_stale_after_ms, 25);
        let registry = Arc::new(
            lash::testing::TestLocalProcessRegistry::default()
                .with_clock(Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>)
                .with_wake_delivery_config(wake_delivery_config),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let process_id = "workbench-targeted-wake-process";
        registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    process_id,
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register targeted wake producer");
        let earlier_wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "earlier"}),
                ),
            )
            .await
            .expect("append earlier wake")
            .wake_delivery
            .expect("earlier wake delivery");
        let later_wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "later"}),
                ),
            )
            .await
            .expect("append later wake")
            .wake_delivery
            .expect("later wake delivery");
        assert!(earlier_wake.sequence < later_wake.sequence);
        let stale_earlier_claim = registry
            .claim_pending_wake_deliveries(1)
            .await
            .expect("claim earlier sender wake")
            .into_iter()
            .next()
            .expect("earlier sender wake is claimable");
        assert_eq!(stale_earlier_claim.wake.sequence, earlier_wake.sequence);
        let earlier = target
            .enqueue_queued_work(workbench_process_wake_draft(earlier_wake.clone()))
            .await
            .expect("enqueue earlier receiver wake");
        let later = target
            .enqueue_queued_work(workbench_process_wake_draft(later_wake))
            .await
            .expect("enqueue later receiver wake");

        let later_request = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: "workbench-targeted-later".to_string(),
            session_id: session_id.clone(),
            reason: "test_targeted_later".to_string(),
            batch_ids: vec![later.batch_id.clone()],
            drain_id: Some("workbench-targeted-later-drain".to_string()),
        };
        let later_output = later_request
            .selected_queued_turn(&session)
            .run()
            .await
            .expect("run only later workbench batch")
            .expect("later workbench batch produced a turn");
        let started_batch_ids = later_output
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            started_batch_ids,
            vec![later.batch_id.as_str()],
            "a targeted workbench run must not checkpoint-claim another queued batch"
        );
        assert_eq!(
            target
                .list_queued_work(&session_id)
                .await
                .expect("list after targeted later drain")
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![earlier.batch_id.as_str()],
            "removing selected-drain settlement must leave the wrong receiver evidence"
        );

        clock.advance(24);
        let before_stale_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&registry),
            Arc::clone(&store_factory),
            None,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("claimed wake stays unavailable before configured stale age");
        assert_eq!(before_stale_boundary.inspected, 0);

        clock.advance(1);
        let redelivery: lash::process::WakeDeliveryDriveReport =
            lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&registry),
            Arc::clone(&store_factory),
            None,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("redeliver earlier wake through host driver");
        assert_eq!(redelivery.inspected, 1);
        assert_eq!(redelivery.enqueued, 1);
        assert_eq!(redelivery.discarded_expired, 0);
        assert_eq!(redelivery.discarded_target_gone, 0);
        assert_eq!(redelivery.floor_absorbed, 1);
        assert_eq!(redelivery.discarded_sequence_rewound, 0);
        assert_eq!(redelivery.retryable_failures, 0);
        assert_eq!(
            target
                .list_queued_work(&session_id)
                .await
                .expect("list after earlier live-row absorption")
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![earlier.batch_id.as_str()],
            "live-row absorption must preserve the skipped earlier wake"
        );

        let expiry_clock = Arc::new(
            lash_core::testing::TestClock::new(
                1_900_000_000_000,
            ),
        );
        let expiry_config = lash::process::WakeDeliveryConfig::new(50)
            .expect("valid boundary wake expiry")
            .with_enqueuing_stale_after_ms(25)
            .expect("valid boundary stale age");
        let expiry_registry = Arc::new(
            lash::testing::TestLocalProcessRegistry::default()
                .with_clock(Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>)
                .with_wake_delivery_config(expiry_config),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        expiry_registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    "workbench-expiring-wake-process",
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some("workbench-never-created-target".to_string())),
            )
            .await
            .expect("register expiring wake producer");
        expiry_registry
            .append_event(
                "workbench-expiring-wake-process",
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "expires"}),
                ),
            )
            .await
            .expect("append expiring wake");
        let first_expiry_attempt = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&expiry_registry),
            Arc::clone(&store_factory),
            None,
            Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("defer wake for a target that has never existed");
        assert_eq!(first_expiry_attempt.inspected, 1);
        assert_eq!(first_expiry_attempt.retryable_failures, 1);
        assert_eq!(first_expiry_attempt.discarded_target_gone, 0);
        expiry_clock.advance(49);
        let before_expiry_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&expiry_registry),
            Arc::clone(&store_factory),
            None,
            Arc::clone(&expiry_clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("wake remains deferred before configured expiry");
        assert_eq!(before_expiry_boundary.inspected, 0);
        expiry_clock.advance(1);
        let at_expiry_boundary = lash::process::WakeDeliveryDriver::drive_pending_once(
            expiry_registry,
            Arc::clone(&store_factory),
            None,
            expiry_clock as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("discard wake at configured expiry");
        assert_eq!(at_expiry_boundary.inspected, 1);
        assert_eq!(at_expiry_boundary.discarded_expired, 1);
        assert_eq!(at_expiry_boundary.discarded_target_gone, 0);

        let deleted_target_id = "workbench-deleted-wake-target";
        store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: deleted_target_id.to_string(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("create wake target before deletion");
        store_factory
            .delete_session(deleted_target_id)
            .await
            .expect("delete wake target before delivery");
        let target_gone_registry = Arc::new(
            lash::testing::TestLocalProcessRegistry::default()
                .with_clock(Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>)
                .with_wake_delivery_config(wake_delivery_config),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        target_gone_registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    "workbench-target-gone-wake-process",
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(deleted_target_id.to_string())),
            )
            .await
            .expect("register target-gone wake producer");
        target_gone_registry
            .append_event(
                "workbench-target-gone-wake-process",
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "target gone"}),
                ),
            )
            .await
            .expect("append target-gone wake");
        let target_gone = lash::process::WakeDeliveryDriver::drive_pending_once(
            target_gone_registry,
            Arc::clone(&store_factory),
            None,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("discard wake for deleted target");
        assert_eq!(target_gone.inspected, 1);
        assert_eq!(target_gone.discarded_expired, 0);
        assert_eq!(target_gone.discarded_target_gone, 1);

        // The operator's queue view is a snapshot. "Run this one" can arrive
        // after something else already drained that batch, so a selection can
        // name a batch that is no longer claimable. It must then claim
        // *nothing*: the earlier ready wake the operator did not select stays
        // queued. Widening an unmatched selection into an ordinary
        // drain-everything would run work nobody asked for, and the operator
        // would see it as the one batch they clicked.
        let stale_selection = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: "workbench-stale-selection".to_string(),
            session_id: session_id.clone(),
            reason: "test_stale_selection".to_string(),
            batch_ids: vec![later.batch_id.clone()],
            drain_id: Some("workbench-stale-selection-drain".to_string()),
        };
        assert!(
            stale_selection
                .selected_queued_turn(&session)
                .run()
                .await
                .expect("a selection naming an already-drained batch is a no-op, not an error")
                .is_none(),
            "a selection with no claimable batch must not produce a turn"
        );
        let Json(still_queued) =
            list_queued_work(State(state.clone()), Query(SessionQuery::default()))
                .await
                .expect("workbench queue projection after the stale selection");
        assert_eq!(
            still_queued
                .iter()
                .map(|batch| batch.batch_id.as_str())
                .collect::<Vec<_>>(),
            vec![earlier.batch_id.as_str()],
            "an unmatched batch selection must not fall back to draining the \
             ready work the operator did not select"
        );

        let earlier_request = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: "workbench-targeted-earlier".to_string(),
            session_id: session_id.clone(),
            reason: "test_targeted_earlier".to_string(),
            batch_ids: vec![earlier.batch_id.clone()],
            drain_id: Some("workbench-targeted-earlier-drain".to_string()),
        };
        assert!(
            earlier_request
                .selected_queued_turn(&session)
                .run()
                .await
                .expect("run earlier workbench batch after later")
                .is_some()
        );
        assert!(
            target
                .list_queued_work(&session_id)
                .await
                .expect("list after earlier drain")
                .is_empty(),
            "the skipped earlier wake must still fire afterwards"
        );

        let third_wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "ordinary drain first"}),
                ),
            )
            .await
            .expect("append ordinary-drain first wake")
            .wake_delivery
            .expect("ordinary-drain first wake delivery");
        let fourth_wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "ordinary drain second"}),
                ),
            )
            .await
            .expect("append ordinary-drain second wake")
            .wake_delivery
            .expect("ordinary-drain second wake delivery");
        let third = target
            .enqueue_queued_work(workbench_process_wake_draft(third_wake))
            .await
            .expect("enqueue ordinary-drain first wake");
        let fourth = target
            .enqueue_queued_work(workbench_process_wake_draft(fourth_wake))
            .await
            .expect("enqueue ordinary-drain second wake");
        let drain_all_output = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: "workbench-drain-all".to_string(),
            session_id: session_id.clone(),
            reason: "test_drain_all".to_string(),
            batch_ids: Vec::new(),
            drain_id: Some("workbench-drain-all-idempotency".to_string()),
        }
        .queued_turn(&session)
        .run()
        .await
        .expect("run ordinary workbench drain")
        .expect("ordinary workbench drain produced a turn");
        let drain_all_started = drain_all_output
            .activities
            .iter()
            .filter_map(|activity| match &activity.event {
                lash::TurnEvent::QueuedWorkStarted { batch_ids, .. } => Some(batch_ids),
                _ => None,
            })
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            drain_all_started,
            vec![third.batch_id.as_str(), fourth.batch_id.as_str()],
            "the selected-drain guard must not disable ordinary checkpoint draining"
        );
        assert!(
            target
                .list_queued_work(&session_id)
                .await
                .expect("list after ordinary drain")
                .is_empty(),
            "ordinary drain-everything behavior must remain intact"
        );

        let _ = std::fs::remove_dir_all(data_dir);
    });
}

/// One background wake turn must leave exactly one agent reply behind — once in
/// the durable transcript and once on screen.
///
/// A wake turn runs without `require_finish`, so a prose-only model reply
/// terminates naturally and the turn finishes with
/// `TurnFinish::AssistantMessage`. That is precisely the outcome the runtime
/// materializes its own terminal assistant message for
/// (`materialize_terminal_output`), so a host that also commits its own copy of
/// the same reply writes the answer into the transcript twice and renders it
/// twice. A foreground send never showed it because `require_finish` forces the
/// reply through `finish`, which the runtime does not materialize (FIG-984).
#[test]
fn wake_turn_leaves_exactly_one_agent_reply_committed_and_rendered() {
    run_async_test_on_stack_budget("workbench-wake-single-agent-reply", || async {
        const WAKE_REPLY: &str = "You pressed the Red button!";
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-wake-single-reply-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create wake single-reply dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-wake-single-reply-test")
                .complete(|_| async { Ok(text_response(WAKE_REPLY)) })
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
            32_768,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open wake single-reply session");
        let target = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open wake single-reply receiver");
        let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default())
            as Arc<dyn lash::process::ProcessRegistry>;
        let process_id = "workbench-wake-single-reply-process";
        registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    process_id,
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register wake single-reply producer");
        let wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "the user pressed the Red button"}),
                ),
            )
            .await
            .expect("append wake single-reply event")
            .wake_delivery
            .expect("wake single-reply delivery");
        target
            .enqueue_queued_work(workbench_process_wake_draft(wake))
            .await
            .expect("enqueue wake single-reply batch");

        let turn_id = "workbench-queued-wake-single-reply";
        state.track_turn(&session_id, turn_id);
        let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let output = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: turn_id.to_string(),
            session_id: session_id.clone(),
            reason: "test_wake_single_reply".to_string(),
            batch_ids: Vec::new(),
            drain_id: Some(format!("{turn_id}-drain")),
        }
        .queued_turn(&session)
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&turn_state),
        })
        .await
        .expect("run wake single-reply turn")
        .expect("the wake batch produced a turn");
        assert!(
            matches!(
                &output.outcome,
                lash::TurnOutcome::Finished(lash::TurnFinish::AssistantMessage { text })
                    if text == WAKE_REPLY
            ),
            "a wake turn's prose reply must terminate naturally as an assistant \
             message, which is the outcome the runtime materializes: {:?}",
            output.outcome
        );
        crate::restate::record_turn_output(
            &state,
            &session,
            turn_id,
            output,
            turn_state,
            "test.wake_single_reply.completed",
        )
        .await
        .expect("record wake single-reply turn output");
        let committed_agent_replies = session
            .read_view()
            .messages()
            .iter()
            .filter(|message| {
                lash::message_role(message) == "assistant"
                    && lash::message_text(message).contains(WAKE_REPLY)
            })
            .map(|message| message.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            committed_agent_replies.len(),
            1,
            "a completed wake turn must commit the agent reply exactly once, \
             got {committed_agent_replies:?}"
        );
        crate::restate::settle_workbench_turn(&state, &session_id, turn_id)
            .await
            .expect("settle wake single-reply turn");
        session
            .close()
            .await
            .expect("close wake single-reply session");

        let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("read wake single-reply snapshot");
        let rendered_agent_rows = snapshot
            .transcript
            .iter()
            .filter_map(|row| match row {
                TranscriptRow::Message { message }
                    if message.role == "assistant" && message.text.contains(WAKE_REPLY) =>
                {
                    Some(message.id.clone())
                }
                TranscriptRow::Message { .. }
                | TranscriptRow::Reasoning { .. }
                | TranscriptRow::CodeBlock { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rendered_agent_rows.len(),
            1,
            "the settled snapshot must render the agent reply exactly once, \
             got {rendered_agent_rows:?}"
        );
        assert_eq!(
            rendered_agent_rows, committed_agent_replies,
            "the rendered agent row must be the committed transcript copy"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

#[test]
fn selected_drain_reports_claimed_and_already_satisfied_batches() {
    run_async_test_on_stack_budget("workbench-selected-drain-outcome", || async {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-selected-drain-outcome-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create selected-drain outcome dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            Arc::new(lash::persistence::InMemorySessionStoreFactory::new());
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-selected-drain-outcome-test")
                .complete(|_| async {
                    Ok(text_response(
                        "<lashlang>\nfinish \"processed selected row\"\n</lashlang>",
                    ))
                })
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
            32_768,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open selected-drain outcome session");
        let store = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open selected-drain outcome store");
        let batch = store
            .enqueue_queued_work(queued_work_test_draft(
                &session_id,
                "workbench-selected-drain-outcome",
            ))
            .await
            .expect("enqueue selected-drain outcome row");

        let claimed = session
            .queued_turn()
            .batch_ids([batch.batch_id.clone()])
            .run()
            .await
            .expect("run selected-drain outcome row");
        let claimed_satisfaction = vec![lash::SelectedQueuedWorkBatchSatisfaction::ClaimedNow {
            batch_id: batch.batch_id.clone(),
        }];
        assert!(claimed.turn.is_some());
        assert!(claimed.is_some());
        assert_eq!(claimed.satisfied, claimed_satisfaction);

        let replay = session
            .queued_turn()
            .batch_ids([batch.batch_id.clone()])
            .run()
            .await
            .expect("replay selected-drain outcome row");
        let replay_satisfaction =
            vec![lash::SelectedQueuedWorkBatchSatisfaction::AlreadySatisfied {
                batch_id: batch.batch_id,
            }];
        assert!(replay.turn.is_none());
        assert!(replay.is_none());
        assert_eq!(replay.satisfied, replay_satisfaction);
        let _ = std::fs::remove_dir_all(data_dir);
    });
}

/// A wake turn must not retract the answer the turn before it left on screen.
///
/// A cause-only turn commits no turn input: its cause lands as an `Event`
/// message and the reply that follows belongs to the wake, not to the send
/// before it. A projection that settles a turn's protocol-authored reply only
/// at the next *turn input* folds the two turns together and drops the first
/// answer the moment the wake commits its own — an answer disappearing after
/// the user read it (FIG-1406).
#[test]
fn a_wake_turn_leaves_the_previous_reasoned_reply_rendered() {
    run_async_test_on_stack_budget("workbench-wake-keeps-previous-reply", || async {
        const REASONED_REPLY: &str = "FIG-1406 reasoned send answer";
        const WAKE_REPLY: &str = "FIG-1406 wake answer";
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-wake-keeps-previous-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create wake keeps-previous dir");
        let store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let state = recoverable_chat_test_state_with_dependencies_and_context(
            &data_dir,
            16,
            lash::testing::TestProvider::builder()
                .kind("workbench-wake-keeps-previous-test")
                .complete(move |_| {
                    let calls = Arc::clone(&calls);
                    async move {
                        let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if call == 0 {
                            // The send answers with reasoning attached, so the
                            // RLM protocol owns the committed copy.
                            let mut response = text_response(REASONED_REPLY);
                            response.parts.insert(
                                0,
                                lash::direct::LlmOutputPart::Reasoning {
                                    text: "FIG-1406 send reasoning".to_string(),
                                    replay: None,
                                },
                            );
                            Ok(response)
                        } else {
                            Ok(text_response(WAKE_REPLY))
                        }
                    }
                })
                .build()
                .into_handle(),
            in_memory_trigger_store(),
            Arc::clone(&store_factory),
            Some(inert_queued_work_driver()),
            32_768,
        )
        .await;
        let session_id = state.current_session_id();
        let session = state
            .core
            .session(session_id.clone())
            .open()
            .await
            .expect("open wake keeps-previous session");

        let send_turn_id = "workbench-turn-reasoned-send";
        let send_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let send_output = session
            .turn(lash::TurnInput::text("answer with reasoning"))
            .turn_id(send_turn_id)
            .stream_to(&ChannelTurnEvents {
                turn_state: Arc::clone(&send_turn_state),
            })
            .await
            .expect("run reasoned send turn");
        crate::restate::record_turn_output(
            &state,
            &session,
            send_turn_id,
            send_output,
            send_turn_state,
            "test.wake_keeps_previous.send",
        )
        .await
        .expect("record reasoned send output");
        crate::restate::settle_workbench_turn(&state, &session_id, send_turn_id)
            .await
            .expect("settle reasoned send turn");

        let target = store_factory
            .create_store(&lash::persistence::SessionStoreCreateRequest {
                session_id: session_id.clone(),
                relation: lash::persistence::SessionRelation::Root,
                policy: session.policy_snapshot(),
            })
            .await
            .expect("open wake keeps-previous receiver");
        let registry = Arc::new(lash::testing::TestLocalProcessRegistry::default())
            as Arc<dyn lash::process::ProcessRegistry>;
        let process_id = "workbench-wake-keeps-previous-process";
        registry
            .register_process(
                lash::process::ProcessRegistration::new(
                    process_id,
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "producer.wake".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: lash::process::ProcessEventSemanticsSpec {
                        wake: Some(lash::process::ProcessWakeSpec {
                            when: Some(lash::process::ProcessValueSelector::Present(
                                "/wake_input".to_string(),
                            )),
                            input: lash::process::ProcessValueSelector::Pointer(
                                "/wake_input".to_string(),
                            ),
                        }),
                        ..lash::process::ProcessEventSemanticsSpec::default()
                    },
                }])
                .with_wake_session_id(Some(session_id.clone())),
            )
            .await
            .expect("register wake keeps-previous producer");
        let wake = registry
            .append_event(
                process_id,
                lash::process::ProcessEventAppendRequest::new(
                    "producer.wake",
                    json!({"wake_input": "the producer woke this session"}),
                ),
            )
            .await
            .expect("append wake keeps-previous event")
            .wake_delivery
            .expect("wake keeps-previous delivery");
        target
            .enqueue_queued_work(workbench_process_wake_draft(wake))
            .await
            .expect("enqueue wake keeps-previous batch");

        let wake_turn_id = "workbench-queued-wake-keeps-previous";
        state.track_turn(&session_id, wake_turn_id);
        let wake_turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
        let wake_output = restate::WorkbenchQueuedTurnWorkflowRequest {
            turn_id: wake_turn_id.to_string(),
            session_id: session_id.clone(),
            reason: "test_wake_keeps_previous".to_string(),
            batch_ids: Vec::new(),
            drain_id: Some(format!("{wake_turn_id}-drain")),
        }
        .queued_turn(&session)
        .stream_to(&ChannelTurnEvents {
            turn_state: Arc::clone(&wake_turn_state),
        })
        .await
        .expect("run wake keeps-previous turn")
        .expect("the wake batch produced a turn");
        crate::restate::record_turn_output(
            &state,
            &session,
            wake_turn_id,
            wake_output,
            wake_turn_state,
            "test.wake_keeps_previous.wake",
        )
        .await
        .expect("record wake keeps-previous output");
        assert!(
            session
                .read_view()
                .messages()
                .iter()
                .any(|message| lash::message_role(message) == "event"),
            "the wake turn must commit its cause as an event message, which is \
             the only boundary this projection can read"
        );
        crate::restate::settle_workbench_turn(&state, &session_id, wake_turn_id)
            .await
            .expect("settle wake keeps-previous turn");
        session
            .close()
            .await
            .expect("close wake keeps-previous session");

        let Json(snapshot) = app_state(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("read wake keeps-previous snapshot");
        let agent_rows = snapshot
            .state
            .messages
            .iter()
            .filter(|message| message.role == "assistant")
            .map(|message| message.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            agent_rows,
            vec![REASONED_REPLY.to_string(), WAKE_REPLY.to_string()],
            "the send's reasoned answer must survive the wake that followed it"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    });
}
