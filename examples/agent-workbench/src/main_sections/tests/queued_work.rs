fn queued_work_test_draft(
    session_id: &str,
    source_key: &str,
) -> lash::persistence::QueuedWorkBatchDraft {
    lash::persistence::QueuedWorkBatchDraft::new(
        session_id,
        lash::persistence::DeliveryPolicy::EarliestSafeBoundary,
        lash::persistence::SlotPolicy::Exclusive,
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
        lash::persistence::SlotPolicy::Exclusive,
        vec![lash::persistence::QueuedWorkPayload::process_wake(wake)],
    )
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
        let state = recoverable_chat_test_state_with_dependencies(
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
            lash::testing::conformance::WakeDeliveryConformanceClock::new(
                1_800_000_000_000,
            ),
        );
        let registry = Arc::new(
            lash::testing::TestLocalProcessRegistry::default()
                .with_clock(Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>)
                .with_wake_delivery_config(
                    lash::process::WakeDeliveryConfig::new(10_000)
                        .expect("valid wake expiry")
                        .with_enqueuing_stale_after_ms(25)
                        .expect("valid stale claim age"),
                ),
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
            .queued_turn(&session)
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

        clock.advance(26);
        let redelivery = lash::process::WakeDeliveryDriver::drive_pending_once(
            Arc::clone(&registry),
            Arc::clone(&store_factory),
            None,
            Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>,
            32,
        )
        .await
        .expect("redeliver earlier wake through host driver");
        assert_eq!(redelivery.enqueued, 1);
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
                .queued_turn(&session)
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
                .queued_turn(&session)
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
