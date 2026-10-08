mod tests {
    //! Unit tests for the process awaiter, watched registry, and work driver.

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::PluginError;
    use crate::runtime::ProcessRegistryFaults;
    use crate::runtime::process::*;
    use crate::runtime::work::*;

    use crate::{
        ProcessEventSink, ProcessExternalRef, ProcessInput, ProcessProvenance, ProcessRegistration,
        ProcessStarted, TestProcessRegistryWriteExt, WaitState, WatchedRegistry,
        watch_process_registry,
    };

    async fn memory_registry() -> Arc<dyn ProcessRegistry> {
        crate::support::sqlite_memory_process_store_set()
            .await
            .process_registry()
    }
    use lash_sansio::sync::MutexExt;

    fn watched_parts(watched: WatchedRegistry) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub) {
        (Arc::clone(watched.registry()), watched.hub().clone())
    }

    fn registration() -> ProcessRegistration {
        crate::testing::held_engine_registration(
            serde_json::json!({}),
            ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
    }

    #[derive(Clone, Default)]
    struct CollectingSink {
        events: Arc<Mutex<Vec<(String, u64)>>>,
    }

    impl CollectingSink {
        fn collected(&self) -> Vec<(String, u64)> {
            self.events.lock_recover().clone()
        }
    }

    #[async_trait::async_trait]
    impl ProcessEventSink for CollectingSink {
        async fn emit(&self, event: &ProcessEvent) {
            self.events
                .lock_recover()
                .push((event.fact.event_type().to_owned(), event.sequence));
        }
    }

    fn success(value: serde_json::Value) -> ProcessAwaitOutput {
        ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(value))
    }

    /// ADR 0016 pins the default awaiter cadence while allowing native backends
    /// to tune both bounds through `WorkCadencePolicy`.
    ///
    /// The paused clock moves only when the test advances it. Every poll the
    /// cadence is measured on reads a pinned pre-completion record, so no
    /// backend I/O idles the runtime while a poll sleep is pending: a paused
    /// runtime that idles on the store's connection thread auto-advances to the
    /// next timer, which would fire the poll early.
    #[tokio::test(start_paused = true)]
    async fn polling_awaiter_uses_configured_work_cadence_floor() {
        let registry = memory_registry().await;
        let registered = registry
            .register_process(registration())
            .await
            .expect("register");
        registry
            .complete_process(
                &registered.id,
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::workflow_key(&registered.id),
            )
            .await
            .expect("complete");
        let faults = ProcessRegistryFaults::new(Arc::clone(&registry));
        faults.set_process_read_pinned(Some(registered.clone()));
        let work_cadence = WorkCadencePolicy {
            poll_initial: Duration::from_secs(2),
            poll_max: Duration::from_secs(3),
            ..WorkCadencePolicy::default()
        };
        let awaiter = ProcessRegistryAwaiter::for_registry(Arc::new(faults.clone()))
            .with_work_cadence(work_cadence);
        let process_id = registered.id.clone();
        let waiter = crate::task::spawn(async move { awaiter.await_terminal(&process_id).await });
        tokio::task::yield_now().await;
        let parked = faults.process_point_reads();
        assert!(
            parked > 0 && !waiter.is_finished(),
            "the awaiter read the pinned running record and parked on its poll"
        );

        tokio::time::advance(Duration::from_millis(1_999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            faults.process_point_reads(),
            parked,
            "the awaiter must not poll before the configured initial delay"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            faults.process_point_reads(),
            parked + 1,
            "the awaiter polls once the configured initial delay elapses"
        );

        // The doubled backoff (4s) is capped at the configured maximum.
        tokio::time::advance(Duration::from_millis(2_999)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            faults.process_point_reads(),
            parked + 1,
            "the awaiter must not poll before the configured maximum delay"
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            faults.process_point_reads(),
            parked + 2,
            "the backoff is capped at the configured maximum delay"
        );

        faults.set_process_read_pinned(None);
        tokio::time::advance(Duration::from_secs(3)).await;
        let output = waiter
            .await
            .expect("configured-cadence waiter joins")
            .expect("configured-cadence wait resolves");
        assert_eq!(output, success(serde_json::json!("done")));
    }

    #[tokio::test]
    async fn await_event_returns_historical_event_immediately() {
        let raw = memory_registry().await;
        let (registry, hub) = watched_parts(watch_process_registry(raw));
        let proc_record = registry
            .register_process(registration())
            .await
            .expect("register");
        let appended = registry
            .request_process_cancel(
                &proc_record.id,
                crate::CancelOrigin::OperatorRequested,
                "actor:fixture:await_event_returns_historical_event_immediately".to_string(),
                None,
            )
            .await
            .expect("append");

        let event = ProcessRegistryAwaiter::new(Arc::clone(&registry), hub)
            .await_event(&proc_record.id, crate::ProcessEventKind::CancelRequested, 0)
            .await
            .expect("await event");
        assert_eq!(event.sequence, appended.last_event_sequence);
    }

    #[tokio::test]
    async fn await_terminal_unknown_process_errors() {
        let registry = memory_registry().await;
        let err = ProcessRegistryAwaiter::for_registry(registry)
            .await_terminal(&crate::ProcessId::fixture("missing"))
            .await
            .expect_err("unknown process should error");
        assert!(
            matches!(
                err,
                PluginError::ProcessUnknown { ref process_id } if process_id == crate::ProcessId::fixture("missing")
            ),
            "unknown process should return ProcessUnknown, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn watched_terminal_wait_releases_registration_on_completion() {
        let raw = memory_registry().await;
        let record = raw
            .register_process(registration())
            .await
            .expect("register");
        let faults = Arc::new(ProcessRegistryFaults::new(Arc::clone(&raw)));
        faults.set_process_read_pinned(Some(record.clone()));
        let (registry, hub) = watched_parts(watch_process_registry(faults.clone()));
        let awaiter = ProcessRegistryAwaiter::new(registry.clone(), hub.clone());
        let mut waiting = Box::pin(awaiter.await_terminal(&record.id));
        let mut another = Box::pin(awaiter.await_terminal(&record.id));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        assert!(futures_util::poll!(&mut another).is_pending());
        assert_eq!(hub.tracked_processes(), 1);

        registry
            .complete_process(
                &record.id,
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::workflow_key(&record.id),
            )
            .await
            .expect("complete");
        faults.set_process_read_pinned(None);
        assert_eq!(
            hub.tracked_processes(),
            1,
            "the waiter still owns its lease"
        );
        let output = waiting.await.expect("await terminal");
        assert_eq!(output, success(serde_json::json!("done")));
        assert_eq!(
            hub.tracked_processes(),
            1,
            "the second waiter still owns its lease"
        );
        assert_eq!(another.await.expect("second await terminal"), output);
        assert_eq!(hub.tracked_processes(), 0);
    }

    #[tokio::test]
    async fn watched_cancelled_wait_releases_registration() {
        let raw = memory_registry().await;
        let record = raw
            .register_process(registration())
            .await
            .expect("register");
        let faults = Arc::new(ProcessRegistryFaults::new(raw));
        faults.set_process_read_pinned(Some(record.clone()));
        let (registry, hub) = watched_parts(watch_process_registry(faults));
        let awaiter = ProcessRegistryAwaiter::new(registry, hub.clone());
        let mut waiting = Box::pin(awaiter.await_terminal(&record.id));
        assert!(futures_util::poll!(&mut waiting).is_pending());
        assert_eq!(hub.tracked_processes(), 1);
        drop(waiting);
        assert_eq!(hub.tracked_processes(), 0);
    }

    #[tokio::test]
    async fn watched_failed_point_read_releases_registration() {
        let raw = memory_registry().await;
        let record = raw
            .register_process(registration())
            .await
            .expect("register");
        let faults = Arc::new(ProcessRegistryFaults::new(raw));
        faults.set_process_read_pinned(Some(record.clone()));
        faults.set_process_read_error_after(
            1,
            PluginError::Session("subscribed point read failed".to_string()),
        );
        let (registry, hub) = watched_parts(watch_process_registry(faults.clone()));
        let error = ProcessRegistryAwaiter::new(registry, hub.clone())
            .await_terminal(&record.id)
            .await
            .expect_err("the read after subscription must fail");
        assert!(
            matches!(error, PluginError::Session(ref message) if message == "subscribed point read failed")
        );
        assert_eq!(faults.process_point_reads(), 2);
        assert_eq!(hub.tracked_processes(), 0);
    }

    #[tokio::test]
    async fn watched_event_wait_releases_registration_on_delivery() {
        let (registry, hub) = watched_parts(watch_process_registry(memory_registry().await));
        let record = registry
            .register_process(registration())
            .await
            .expect("register");
        let awaiter = ProcessRegistryAwaiter::new(registry.clone(), hub.clone());
        let mut waiting =
            Box::pin(awaiter.await_event(&record.id, crate::ProcessEventKind::CancelRequested, 0));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                assert!(futures_util::poll!(&mut waiting).is_pending());
                if hub.tracked_processes() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("event wait subscribes");
        let appended = registry
            .request_process_cancel(
                &record.id,
                crate::CancelOrigin::OperatorRequested,
                "actor:fixture:watched_event_wait_releases_registration_on_delivery".to_string(),
                None,
            )
            .await
            .expect("append");
        let event = waiting.await.expect("await event");
        assert_eq!(event.sequence, appended.last_event_sequence);
        assert_eq!(hub.tracked_processes(), 0);
    }

    #[tokio::test]
    async fn sink_receives_complete_process_terminal_append() {
        let raw = memory_registry().await;
        let sink = CollectingSink::default();
        let watched = watch_process_registry(raw);
        let _sink = watched.add_event_sink(Arc::new(sink.clone()));
        let (registry, _hub) = watched_parts(watched);
        let proc_record = registry
            .register_process(registration())
            .await
            .expect("register");
        registry
            .set_external_ref(
                &proc_record.id,
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await
            .expect("explicit append");
        registry
            .complete_process(
                &proc_record.id,
                success(serde_json::json!("done")),
                crate::ProcessCompletionAuthority::workflow_key(&proc_record.id),
            )
            .await
            .expect("complete");

        let collected = sink.collected();
        assert_eq!(
            collected
                .iter()
                .map(|(event_type, _)| event_type.as_str())
                .collect::<Vec<_>>(),
            vec!["process.external_ref_set", "process.completed"],
            "the sink must observe terminal events appended through completion verbs"
        );
        assert!(
            collected[0].1 < collected[1].1,
            "terminal event sequences must follow preceding appends"
        );
    }

    #[tokio::test]
    async fn sink_receives_runtime_lifecycle_events_in_order() {
        let raw = memory_registry().await;
        let sink = CollectingSink::default();
        let watched = watch_process_registry(raw);
        let _sink = watched.add_event_sink(Arc::new(sink.clone()));
        let (registry, _hub) = watched_parts(watched);
        let mut lifecycle_registration = ProcessRegistration::new(
            ProcessInput::Engine {
                kind: "test".to_string(),
                payload: serde_json::json!({}),
            },
            ProcessProvenance::host(),
            crate::Lifetime::Detached,
        );
        lifecycle_registration.env_ref = Some(crate::testing::process_execution_env_fixture_ref());
        let lifecycle_id = registry
            .register_process(lifecycle_registration)
            .await
            .expect("register")
            .id;
        registry
            .record_first_started(
                &lifecycle_id,
                ProcessStarted {
                    owner: crate::LeaseOwnerIdentity::engine_process_execution(
                        &lifecycle_id,
                        "lifecycle-test",
                    ),
                    attempt: 1,
                    started_at_ms: 1,
                    generation: None,
                    plugins: None,
                },
            )
            .await
            .expect("record first start");
        let wait = WaitState {
            kind: crate::WaitKind::Call {
                call_id: crate::ToolCallId::fixture("lifecycle-call"),
                tool_id: crate::ToolId::from("lifecycle-tool"),
            },
            since_ms: 2,
        };
        registry
            .set_process_wait(&lifecycle_id, wait)
            .await
            .expect("enter wait");
        registry
            .clear_process_wait(&lifecycle_id)
            .await
            .expect("clear wait");
        registry
            .set_external_ref(
                &lifecycle_id,
                ProcessExternalRef {
                    backend: "test".to_string(),
                    id: "external".to_string(),
                    metadata: None,
                    segment_ordinal: None,
                },
            )
            .await
            .expect("set external ref");

        let process_id = registry
            .require_process_id(&lifecycle_id)
            .await
            .expect("retained lifecycle target");
        let before_cancel = registry
            .get_process(&process_id)
            .await
            .expect("read before cancel")
            .expect("retained target");
        assert!(!before_cancel.is_terminal());
        assert!(before_cancel.cancel_request.is_none());
        let cancelled = registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::OperatorRequested,
                "actor:lifecycle-sink".to_string(),
                None,
            )
            .await
            .expect("request cancellation through watched registry");
        assert_eq!(
            cancelled
                .cancel_request
                .as_ref()
                .expect("folded cancellation")
                .origin,
            crate::CancelOrigin::OperatorRequested
        );
        let repeated = registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::OperatorRequested,
                "actor:lifecycle-sink".to_string(),
                None,
            )
            .await
            .expect("replay cancellation through watched registry");
        assert_eq!(
            repeated, cancelled,
            "watch notification preserves the first durable request"
        );

        let collected = sink.collected();
        assert_eq!(
            collected
                .iter()
                .map(|(event_type, _)| event_type.as_str())
                .collect::<Vec<_>>(),
            vec![
                "process.first_started",
                "process.waiting",
                "process.resumed",
                "process.external_ref_set",
                "process.cancel_requested",
            ],
            "the sink must observe every runtime lifecycle append"
        );
        assert!(
            collected.windows(2).all(|events| events[0].1 < events[1].1),
            "runtime lifecycle event sequences must be strictly ordered"
        );
    }

    #[tokio::test]
    async fn native_awaiter_returns_an_already_terminal_process() {
        let raw = memory_registry().await;
        let (registry, hub) = watched_parts(watch_process_registry(raw));
        let awaiter = ProcessRegistryAwaiter::new(Arc::clone(&registry), hub);
        let proc_record = registry
            .register_process(registration())
            .await
            .expect("register");
        registry
            .complete_process(
                &proc_record.id,
                success(serde_json::json!("ready")),
                crate::ProcessCompletionAuthority::workflow_key(&proc_record.id),
            )
            .await
            .expect("complete");

        let output = awaiter
            .await_terminal(&proc_record.id)
            .await
            .expect("await terminal");
        assert_eq!(output, success(serde_json::json!("ready")));
    }

    /// Sim-style race: many waiters attach to one process and completion fires
    /// while they are mid-flight between their subscribe and their first read.
    /// The change hub must resolve every one with identical output — no lost
    /// wakeups, no divergent results (ADR 0016).
    #[tokio::test]
    async fn concurrent_waiters_all_resolve_with_identical_output_on_completion() {
        let raw = memory_registry().await;
        let (registry, hub) = watched_parts(watch_process_registry(raw));
        let proc_record = registry
            .register_process(registration())
            .await
            .expect("register");

        const WAITERS: usize = 16;
        let barrier = Arc::new(tokio::sync::Barrier::new(WAITERS + 1));
        let mut waiters = Vec::with_capacity(WAITERS);
        for _ in 0..WAITERS {
            let awaiter = ProcessRegistryAwaiter::new(Arc::clone(&registry), hub.clone());
            let barrier = Arc::clone(&barrier);
            let process_id = proc_record.id.clone();
            waiters.push(crate::task::spawn(async move {
                barrier.wait().await;
                awaiter.await_terminal(&process_id).await
            }));
        }
        // Release every waiter, then complete at once so completion races their
        // first read and subscribe.
        barrier.wait().await;
        let output = success(serde_json::json!({ "raced": true }));
        registry
            .complete_process(
                &proc_record.id,
                output.clone(),
                crate::ProcessCompletionAuthority::workflow_key(&proc_record.id),
            )
            .await
            .expect("complete");

        for waiter in waiters {
            let resolved = tokio::time::timeout(Duration::from_secs(2), waiter)
                .await
                .expect("each racing waiter resolves under 2s")
                .expect("join waiter")
                .expect("await terminal");
            assert_eq!(
                resolved, output,
                "every concurrent waiter resolves with identical terminal output"
            );
        }
    }
}
