#[cfg(test)]
mod process_work_tests {
    use super::tests::{
        explicit_durable_test_facets, in_memory_trigger_store, run_async_test_on_stack_budget,
        spawn_restate_ingress_capture,
    };
    use super::*;

    #[test]
    fn workbench_work_rail_exposes_process_cancellation() {
        assert!(ui::INDEX_HTML.contains("className = \"work-cancel\""));
        assert!(ui::INDEX_HTML.contains("/cancel\""));
        assert!(ui::INDEX_HTML.contains("Request cooperative process cancellation"));
        assert!(ui::INDEX_HTML.contains("error: \" + process.error"));
        assert!(
            ui::INDEX_HTML.contains("row.error ? \" error\""),
            "failed work must receive the work rail's visible error treatment"
        );
    }

    #[test]
    fn await_work_route_returns_terminal_outcome_and_reconciled_events() {
        run_async_test_on_stack_budget("workbench-await-work-test", || {
            await_work_route_returns_terminal_outcome_and_reconciled_events_inner()
        });
    }

    async fn await_work_route_returns_terminal_outcome_and_reconciled_events_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-await-work-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let session_store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            data_dir.join("lash-sessions"),
        ));
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> =
            session_store_factory;
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("await-work route test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::builder("test-model")
                .context_window_tokens(4096)
                .build().expect("model spec");
        let event_tx = SessionEventRegistry::new(16);
        // The app sink, wired exactly as bootstrap wires it — through the
        // driver's watched decorator, feeding an mpsc channel.
        let (sink_tx, mut sink_rx) = mpsc::channel::<lash::process::ProcessEvent>(16);
        let driver = lash::process::ProcessWorkDriver::new_with_sink(
            Arc::clone(&process_registry),
            Arc::new(NoopProcessRunHandle),
            Some(Arc::new(ChannelProcessEventSink::new(sink_tx))),
        );
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(Arc::clone(&core_store_factory))
            .process_registry(Arc::clone(&process_registry))
            .build(crate::test_core_owner())
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: driver.clone(),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx,
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url: "http://127.0.0.1:8080".to_string(),
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };

        // Register, append one non-terminal event, and complete — all through
        // the driver's decorated registry handle, the same one the sink watches.
        let watched = driver.process_registry();
        watched
            .register_process(
                lash::process::ProcessRegistration::new(
                    "await-route-proc",
                    lash::process::ProcessInput::External {
                        metadata: Value::Null,
                    },
                    lash::process::RecoveryDisposition::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                )
                .with_extra_event_types([lash::process::ProcessEventType {
                    name: "progress".to_string(),
                    payload_schema: lash::triggers::LashSchema::any(),
                    semantics: Default::default(),
                }]),
            )
            .await
            .expect("register process");
        watched
            .append_event(
                "await-route-proc",
                lash::process::ProcessEventAppendRequest::new("progress", json!({ "step": 1 })),
            )
            .await
            .expect("append progress event");
        watched
            .complete_process(
                "await-route-proc",
                lash::process::ProcessAwaitOutput::Success {
                    value: json!("done"),
                    control: None,
                },
                lash::process::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("complete process");

        let Json(result) = await_work(
            AxumPath("await-route-proc".to_string()),
            State(state.clone()),
        )
        .await
        .expect("await work route");

        // Terminal outcome rides the await seam (ADR 0016)...
        assert!(matches!(
            &result.outcome,
            lash::process::ProcessAwaitOutput::Success { value, .. } if value == &json!("done")
        ));
        // ...and the event log reconciled from the durable store is complete.
        assert!(
            result
                .events
                .iter()
                .any(|event| event.event_type == "progress"),
            "reconciled events missing the appended progress event: {:?}",
            result.events
        );

        // The sink saw both appends (best-effort freshness)...
        let mut sunk = Vec::new();
        while let Ok(event) = sink_rx.try_recv() {
            sunk.push(event);
        }
        assert!(
            sunk.iter().any(|event| event.event_type == "progress"),
            "sink missed the non-terminal append: {sunk:?}"
        );
        assert!(
            sunk.iter()
                .any(|event| event.event_type == "process.completed"),
            "sink missed the terminal append: {sunk:?}"
        );

        watched
            .register_process(lash::process::ProcessRegistration::new(
                "failed-work-rail-proc",
                lash::process::ProcessInput::External {
                    metadata: Value::Null,
                },
                lash::process::RecoveryDisposition::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            ))
            .await
            .expect("register failed process");
        watched
            .complete_process(
                "failed-work-rail-proc",
                lash::process::ProcessAwaitOutput::Failure {
                    class: lash::tools::ToolFailureClass::External,
                    code: "deterministic_failure".to_string(),
                    message: "deterministic durable process failure".to_string(),
                    raw: None,
                    control: None,
                },
                lash::process::ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("fail process");
        let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list failed work");
        let failed = work
            .iter()
            .find(|item| item.process.process_id == "failed-work-rail-proc")
            .expect("failed process in work API");
        assert_eq!(failed.process.status_label, "failed");
        assert!(failed.process.terminal);
        assert_eq!(
            failed.process.error.as_deref(),
            Some("deterministic durable process failure")
        );

        // An unknown process id errors instead of hanging.
        let missing = await_work(AxumPath("no-such-process".to_string()), State(state)).await;
        assert!(missing.is_err(), "unknown process id must error");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally() {
        run_async_test_on_stack_budget("workbench-orphaned-process-api-test", || {
            work_api_keeps_orphaned_process_visible_and_routes_cancel_globally_inner()
        });
    }

    async fn work_api_keeps_orphaned_process_visible_and_routes_cancel_globally_inner() {
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-orphaned-process-api-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(&data_dir.join("processes.db"), data_dir.join("lash-sessions"))
                .await
                .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("orphaned process API test should not call the provider")
            .build()
            .into_handle();
        let model = lash::ModelSpec::builder("test-model")
            .context_window_tokens(4096)
            .build()
        .expect("model spec");
        let (restate_ingress_url, mut restate_requests) = spawn_restate_ingress_capture().await;
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(core_store_factory)
            .process_registry(Arc::clone(&process_registry))
            .build(crate::test_core_owner())
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url,
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let session_id = state.current_session_id();
        let process_id = "process-survives-session-delete";
        process_registry
            .register_process(lash::process::ProcessRegistration::new(
                process_id,
                lash::process::ProcessInput::External {
                    metadata: json!({ "test": true }),
                },
                lash::process::RecoveryDisposition::ExternallyOwned,
                lash::process::ProcessProvenance::session(lash::process::SessionScope::new(
                    &session_id,
                )),
            ))
            .await
            .expect("register process");
        process_registry
            .add_observer(
                &session_id,
                process_id,
                lash::process::ProcessObserverBy::host("workbench-session-delete"),
            )
            .await
            .expect("observe process");
        let deletion = process_registry
            .delete_session_process_state(&session_id)
            .await
            .expect("delete session process edges");
        assert_eq!(deletion.removed_observer_count, 1);
        let (_, current_session_id) = state.session_ids.rotate();
        assert_ne!(current_session_id, session_id);

        let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list runtime-wide work");
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].process.process_id, process_id);

        let Json(receipt) = cancel_work(
            AxumPath(process_id.to_string()),
            State(state.clone()),
        )
        .await
        .expect("submit process cancellation");
        assert!(receipt.accepted);
        assert_eq!(receipt.process_id, process_id);
        let request = tokio::time::timeout(Duration::from_secs(2), restate_requests.recv())
            .await
            .expect("Restate request timeout")
            .expect("Restate request");
        assert!(
            request
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| path.starts_with("WorkbenchProcessCancelWorkflow/")),
            "unexpected Restate request: {request:#}"
        );
        assert_eq!(
            request.pointer("/body/process_id").and_then(Value::as_str),
            Some(process_id)
        );
        assert_eq!(
            request.pointer("/body/session_id").and_then(Value::as_str),
            Some(session_id.as_str()),
            "process cancellation must retain its originating trace session"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn durable_process_registry_preserves_identity_lifecycle_and_fencing() {
        run_async_test_on_stack_budget("workbench-process-registry-lifecycle-test", || {
            durable_process_registry_preserves_identity_lifecycle_and_fencing_inner()
        });
    }

    async fn durable_process_registry_preserves_identity_lifecycle_and_fencing_inner() {
        use lash::process::{
            CausalRef, ProcessAwaitOutput, ProcessChangeCursor, ProcessCompletionAuthority,
            ProcessEventAppendRequest, ProcessEventType, ProcessExecutionEnvRef,
            ProcessExecutionEnvSpec, ProcessExternalRef, ProcessHandleSummary, ProcessIdentity, ProcessInput,
            ProcessLeaseClaimOutcome, ProcessListFilter, ProcessListMode, ProcessObserverBy,
            ProcessOriginator, ProcessProvenance, ProcessRegistration, ProcessRegistry,
            ProcessStarted, ProcessStatus, ProcessStatusFilter, ProcessWorklistCursor,
            ProjectionWatermark, RecoveryDisposition, SessionScope,
        };
        let registry = lash::testing::TestLocalProcessRegistry::default();
        let process_id = "invoice-export";
        let scope = SessionScope::for_agent_frame("session-finance", "frame-review");
        assert_eq!(scope.id().as_str(), "session:session-finance/frame:frame-review");
        assert!(!scope.is_empty());
        assert_eq!(scope.session_id, "session-finance");
        assert_eq!(scope.agent_frame_id.as_deref(), Some("frame-review"));

        let cause = CausalRef::TriggerOccurrence {
            occurrence_id: "occurrence-42".to_string(),
            subscription_id: Some("subscription-nightly".to_string()),
            subscription_revision: Some(7),
            subscription_incarnation: Some("incarnation-blue".to_string()),
        };
        let provenance = ProcessProvenance::session(scope.clone()).with_caused_by(Some(cause));
        let ProcessOriginator::Session {
            session_id,
            agent_frame_id,
        } = &provenance.originator
        else {
            panic!("session work must retain a session originator");
        };
        assert_eq!(session_id, "session-finance");
        assert_eq!(agent_frame_id.as_deref(), Some("frame-review"));
        let Some(CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_revision,
            subscription_incarnation,
        }) = &provenance.caused_by
        else {
            panic!("trigger-started work must retain its occurrence provenance");
        };
        assert_eq!(occurrence_id, "occurrence-42");
        assert_eq!(subscription_id.as_deref(), Some("subscription-nightly"));
        assert_eq!(*subscription_revision, Some(7));
        assert_eq!(subscription_incarnation.as_deref(), Some("incarnation-blue"));

        let input = ProcessInput::Engine {
            kind: "report-export".to_string(),
            payload: json!({ "format": "csv", "rows": 12 }),
        };
        assert_eq!(input.engine_kind(), "engine");
        assert_eq!(input.engine_specific_kind(), Some("report-export"));
        let identity = ProcessIdentity::from_process_input(&input)
            .with_label(Some("Nightly invoice export"))
            .with_definition(Some(json!({ "workflow": "invoice-export", "revision": 7 })));
        assert_eq!(identity.kind, "report-export");
        assert_eq!(identity.label.as_deref(), Some("Nightly invoice export"));
        assert_eq!(identity.definition.as_ref().unwrap()["revision"], 7);
        let execution_env_ref = ProcessExecutionEnvSpec::new(
            Default::default(),
            lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
        )
            .stable_ref()
            .expect("derive process execution environment identity");
        let execution_env_digest = execution_env_ref
            .as_str()
            .strip_prefix("process-env:v3:sha256:")
            .expect("process execution environment uses the v3 identity family");
        assert_eq!(execution_env_digest.len(), 64);
        assert!(execution_env_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));

        let registration = ProcessRegistration::new(
            process_id,
            input,
            RecoveryDisposition::Rerunnable,
            ProcessProvenance::host(),
        )
        .with_process_provenance(provenance)
        .with_identity(identity)
        .with_max_attempts(Some(3))
        .with_execution_env_ref(Some(execution_env_ref.clone()))
        .with_extra_event_types([ProcessEventType {
            name: "progress".to_string(),
            payload_schema: lash::triggers::LashSchema::any(),
            semantics: Default::default(),
        }])
        .with_wake_session_id(Some("session-finance".to_string()));
        assert_eq!(registration.id, process_id);
        assert_eq!(registration.disposition, RecoveryDisposition::Rerunnable);
        assert_eq!(registration.max_attempts, Some(3));
        assert_eq!(
            registration.env_ref.as_ref().map(ProcessExecutionEnvRef::as_str),
            Some(execution_env_ref.as_str())
        );
        assert_eq!(registration.wake_session_id.as_deref(), Some("session-finance"));
        assert_eq!(registration.input.engine_specific_kind(), Some("report-export"));
        assert!(registration.event_types.iter().any(|event| {
            event.name == "process.completed" && event.semantics.terminal.is_some()
        }));

        let initial_cursor = ProcessChangeCursor::initial();
        assert_eq!(initial_cursor.store_sequence(), 0);
        assert_eq!(ProcessChangeCursor::from_store_sequence(9).store_sequence(), 9);
        let replay_registration = registration.clone();
        let initial_observers = ["session-finance".to_string(), "session-ops".to_string()];
        let record = registry
            .register_process_with_observers(registration, &initial_observers)
            .await
            .expect("register process and initial observers");
        assert_eq!(record.id, process_id);
        assert_eq!(record.status, ProcessStatus::Running);
        assert!(!record.is_terminal());
        assert_eq!(record.originator_id(), "session-finance");
        assert_eq!(record.identity.label.as_deref(), Some("Nightly invoice export"));
        assert_eq!(record.max_attempts, Some(3));
        assert_eq!(record.disposition, RecoveryDisposition::Rerunnable);
        assert_eq!(record.input.engine_specific_kind(), Some("report-export"));
        assert_eq!(record.provenance.originator, ProcessOriginator::Session {
            session_id: "session-finance".to_string(),
            agent_frame_id: Some("frame-review".to_string()),
        });
        assert_eq!(
            record.env_ref.as_ref().map(ProcessExecutionEnvRef::as_str),
            Some(execution_env_ref.as_str())
        );
        assert!(record.event_types.iter().any(|event| event.name == "progress"));
        assert!(record.updated_at_ms >= record.created_at_ms);
        assert!(record.external_ref.is_none());
        assert!(record.first_started.is_none());
        assert!(record.abandon_request.is_none());
        assert!(record.wait.is_none());
        assert!(record.outcome.is_none());
        let registration_digest = record
            .registration_fingerprint
            .strip_prefix("process-registration-definition:v2:sha256:")
            .expect("process registration uses the v2 definition-fingerprint family");
        assert_eq!(registration_digest.len(), 64);
        assert!(registration_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        let replay = registry
            .register_process_with_observers(
                replay_registration,
                &["session-ops".to_string(), "session-finance".to_string()],
            )
            .await
            .expect("replay process registration by lookup id");
        assert_eq!(replay.id, process_id);
        assert_eq!(replay.registration_fingerprint, record.registration_fingerprint);

        let observers = registry
            .observers_for_process(process_id)
            .await
            .expect("list initial observers");
        assert_eq!(observers, ["session-finance", "session-ops"]);
        assert!(registry
            .is_observer("session-finance", process_id)
            .await
            .expect("read observer edge"));
        registry
            .transfer_observers(
                "session-ops",
                "session-audit",
                &[process_id.to_string()],
                ProcessObserverBy::host("audit-handoff"),
            )
            .await
            .expect("transfer observer");
        assert_eq!(
            registry
                .observers_for_process(process_id)
                .await
                .expect("list transferred observers"),
            ["session-audit", "session-finance"]
        );
        registry
            .remove_observer(
                "session-audit",
                process_id,
                ProcessObserverBy::ForkInheritance,
            )
            .await
            .expect("remove inherited observer");
        assert_eq!(ProcessObserverBy::ForkInheritance.replay_component(), "fork_inheritance");

        let external_ref = ProcessExternalRef {
            backend: "restate".to_string(),
            id: "invocation-778".to_string(),
            metadata: Some(json!({ "region": "eu-central-1" })),
        };
        let record = registry
            .set_external_ref(process_id, external_ref)
            .await
            .expect("bind backend work");
        assert_eq!(record.external_ref.as_ref().unwrap().backend, "restate");
        assert_eq!(record.external_ref.as_ref().unwrap().id, "invocation-778");
        assert_eq!(
            record.external_ref.as_ref().unwrap().metadata.as_ref().unwrap()["region"],
            "eu-central-1"
        );

        let progress = ProcessEventAppendRequest::new(
            "progress",
            json!({ "completed_rows": 8, "total_rows": 12 }),
        )
        .with_replay_key("invoice-export:progress:8");
        assert_eq!(progress.event_type, "progress");
        assert_eq!(progress.payload["completed_rows"], 8);
        assert_eq!(progress.replay.as_ref().unwrap().key, "invoice-export:progress:8");
        let first_progress = registry
            .append_event(process_id, progress.clone())
            .await
            .expect("append progress");
        let replayed_progress = registry
            .append_event(
                process_id,
                progress.with_optional_replay(first_progress.event.invocation.replay.clone()),
            )
            .await
            .expect("replay progress append");
        assert_eq!(replayed_progress.event.sequence, first_progress.event.sequence);
        assert_eq!(replayed_progress.event.process_id, process_id);
        assert_eq!(replayed_progress.event.event_type, "progress");
        assert_eq!(replayed_progress.event.payload["total_rows"], 12);
        assert!(replayed_progress.wake_delivery.is_none());
        assert_eq!(
            registry
                .count_events_through(process_id, "progress", first_progress.event.sequence)
                .await
                .expect("count progress events"),
            1
        );
        assert_eq!(
            registry
                .recent_events(process_id, 1)
                .await
                .expect("read event tail")[0]
                .event_type,
            "progress"
        );

        let owner = lash::persistence::LeaseOwnerIdentity::opaque("worker-berlin", "boot-9");
        let lease = match registry
            .claim_process_lease(process_id, &owner, 60_000)
            .await
            .expect("claim process lease")
        {
            ProcessLeaseClaimOutcome::Acquired(lease) => lease,
            ProcessLeaseClaimOutcome::Busy { holder } => {
                panic!("fresh process unexpectedly held by {}", holder.owner.owner_id)
            }
        };
        assert_eq!(lease.process_id, process_id);
        assert!(lease.schema_version > 0);
        assert_eq!(lease.owner.owner_id, "worker-berlin");
        assert_eq!(lease.owner.incarnation_id, "boot-9");
        assert_eq!(lease.fencing_token, 1);
        assert!(lease.expires_at_epoch_ms > lease.claimed_at_epoch_ms);
        assert_eq!(
            registry
                .get_process_lease(process_id)
                .await
                .expect("read lease")
                .as_ref()
                .map(|held| held.lease_token.as_str()),
            Some(lease.lease_token.as_str())
        );
        let renewed = registry
            .renew_process_lease(&lease, 120_000)
            .await
            .expect("renew lease");
        assert!(renewed.expires_at_epoch_ms >= lease.expires_at_epoch_ms);
        let started = ProcessStarted {
            owner: renewed.owner.clone(),
            fencing_token: renewed.fencing_token,
            attempt: 1,
            started_at_ms: renewed.claimed_at_epoch_ms,
        };
        assert!(started.same_execution(&ProcessStarted {
            owner: renewed.owner.clone(),
            ..started.clone()
        }));
        assert!(!started.same_execution(&ProcessStarted {
            fencing_token: renewed.fencing_token + 1,
            ..started.clone()
        }));

        let running = registry
            .get_process(process_id)
            .await
            .expect("read running process")
            .expect("registered process remains visible");
        assert_eq!(running.status, ProcessStatus::Running);

        let filter = ProcessListFilter::decode(&json!({
            "status": "running",
            "originator_id": "session-finance",
            "identity_kind": "report-export",
            "identity_label": "Nightly invoice export",
            "caused_by_occurrence_id": "occurrence-42",
            "caused_by_subscription_id": "subscription-nightly",
            "created_at_start_ms": record.created_at_ms,
            "created_at_end_ms": record.created_at_ms.saturating_add(1),
        }))
        .expect("decode process filters");
        assert_eq!(filter.status, ProcessStatusFilter::Running);
        assert_eq!(filter.status.label(), Some("running"));
        assert_eq!(ProcessStatus::Failed.label(), "failed");
        assert!(ProcessStatus::Failed.is_terminal());
        assert_eq!(filter.list_mode(), ProcessListMode::Live);
        assert_eq!(filter.list_mode().as_str(), "live");
        assert!(filter.matches_record(&running));
        assert_eq!(
            registry
                .list_processes(&filter)
                .await
                .expect("filter live process")
                .len(),
            1
        );
        assert!(ProcessStatusFilter::Any.matches(ProcessStatus::Running));
        assert_eq!(ProcessStatusFilter::Any.list_mode(), ProcessListMode::All);
        assert_eq!(ProcessStatusFilter::decode(Some("completed")), Ok(ProcessStatusFilter::Completed));

        let live_refs = registry
            .live_reference_summary()
            .await
            .expect("summarize live references");
        assert_eq!(live_refs.len(), 1);
        assert_eq!(live_refs[0].process_count, 1);
        assert_eq!(live_refs[0].definition.as_ref().unwrap()["revision"], 7);
        assert_eq!(
            live_refs[0]
                .env_ref
                .as_ref()
                .map(ProcessExecutionEnvRef::as_str),
            Some(execution_env_ref.as_str())
        );
        assert_eq!(
            registry
                .filter_unregistered_process_ids(&[
                    process_id.to_string(),
                    "never-registered".to_string(),
                ])
                .await
                .expect("filter recovery candidates"),
            ["never-registered"]
        );

        let success = ProcessAwaitOutput::from_tool_output(lash::tools::ToolCallOutput::success(
            json!({ "artifact": "invoices.csv", "rows": 12 }),
        ));
        assert_eq!(success.terminal_status(), Some(ProcessStatus::Completed));
        assert_eq!(
            success.clone().into_tool_output().value_for_projection()["artifact"],
            "invoices.csv"
        );
        let completion = registry
            .complete_process_with_lease(&renewed, success.clone())
            .await
            .expect("complete process under lease");
        assert_eq!(completion.status, ProcessStatus::Completed);
        assert!(completion.is_terminal());
        assert_eq!(completion.outcome.as_ref(), Some(&success));
        let completed = (*completion).clone();
        assert!(registry
            .get_process_lease(process_id)
            .await
            .expect("read released lease")
            .is_none());

        let replay = registry
            .complete_process_with_lease(&renewed, success.clone())
            .await
            .expect("replay terminal completion");
        assert_eq!(replay.status, ProcessStatus::Completed);
        assert_eq!(replay.outcome.as_ref(), Some(&success));
        let cancellation = ProcessAwaitOutput::Cancelled {
            message: "operator cancelled".to_string(),
            raw: None,
            control: None,
        };
        assert_eq!(cancellation.terminal_status(), Some(ProcessStatus::Cancelled));
        assert_eq!(
            cancellation.into_tool_output().value_for_projection()["message"],
            "operator cancelled"
        );

        let handle = ProcessHandleSummary::from_record(completed.clone())
            .with_definition(Some(json!({ "workflow": "invoice-export", "revision": 7 })));
        assert_eq!(handle.handle_type, "process");
        assert_eq!(handle.id, process_id);
        assert_eq!(handle.process_id, process_id);
        assert_eq!(handle.kind, "report-export");
        assert_eq!(handle.label.as_deref(), Some("Nightly invoice export"));
        assert_eq!(handle.definition.as_ref().unwrap()["revision"], 7);
        assert_eq!(handle.status, ProcessStatus::Completed);
        let cancel_summary = lash::process::ProcessCancelSummary::from_record(completed.clone());
        assert_eq!(cancel_summary.process_id, process_id);
        assert_eq!(cancel_summary.status, ProcessStatus::Completed);

        let worklist_cursor = ProcessWorklistCursor::new("example", "invoice-a", "invoice-z");
        assert_eq!(worklist_cursor.backend(), "example");
        assert_eq!(worklist_cursor.after_process_id(), "invoice-a");
        assert_eq!(worklist_cursor.through_process_id(), "invoice-z");
        let worklist_page = registry
            .list_non_terminal_page(
                std::num::NonZeroUsize::new(16).expect("non-zero test page size"),
                None,
            )
            .await
            .expect("list recovery work");
        assert!(worklist_page.records.is_empty());
        assert!(worklist_page.continuation.is_none());
        assert_eq!(
            registry
                .list_processes(&ProcessListFilter {
                    status: ProcessStatusFilter::Completed,
                    ..Default::default()
                })
                .await
                .expect("list completed process")
                .len(),
            1
        );

        let external_id = "externally-owned-export";
        registry
            .register_process(ProcessRegistration::new(
                external_id,
                ProcessInput::External {
                    metadata: json!({ "backend": "batch-service" }),
                },
                RecoveryDisposition::ExternallyOwned,
                ProcessProvenance::new(ProcessOriginator::host_scoped("batch-service")),
            ))
            .await
            .expect("register externally-owned work");
        let external_completion = registry
            .complete_process(
                external_id,
                ProcessAwaitOutput::Failure {
                    class: lash::tools::ToolFailureClass::External,
                    code: "batch_rejected".to_string(),
                    message: "batch service rejected the export".to_string(),
                    raw: Some(json!({ "retryable": false })),
                    control: None,
                },
                ProcessCompletionAuthority::external_owner(),
            )
            .await
            .expect("external owner closes its work");
        assert_eq!(external_completion.status, ProcessStatus::Failed);
        assert!(matches!(
            external_completion.outcome.as_ref(),
            Some(ProcessAwaitOutput::Failure {
                class: lash::tools::ToolFailureClass::External,
                code,
                message,
                raw: Some(raw),
                control: None,
            }) if code == "batch_rejected"
                && message == "batch service rejected the export"
                && raw["retryable"] == false
        ));
        assert_eq!(ProcessCompletionAuthority::workflow_key("wf-1").label(), "workflow-key");

        let report = registry
            .prune_terminal_processes(u64::MAX, None, ProjectionWatermark::NoProjector)
            .await
            .expect("prune projected terminal processes");
        assert_eq!(report.pruned_processes, 2);
        assert!(report.pruned_events >= 2);
        assert_eq!(report.pruned_trigger_deliveries, 0);
        assert_eq!(
            registry
                .filter_tombstoned_process_ids(&[
                    process_id.to_string(),
                    external_id.to_string(),
                    "never-registered".to_string(),
                ])
                .await
                .expect("filter pruned process ids"),
            [process_id, external_id]
        );
        let compacted = registry
            .compact_process_tombstones(
                u64::MAX,
                ProjectionWatermark::UpTo(initial_cursor),
                None,
            )
            .await
            .expect("compact projected tombstones");
        assert_eq!(compacted, 0, "unprojected deletions must retain their tombstones");
        assert_eq!(
            registry
                .compact_process_tombstones(
                    u64::MAX,
                    ProjectionWatermark::NoProjector,
                    None,
                )
                .await
                .expect("compact tombstones without a projector"),
            2
        );
    }

    #[test]
    fn session_delete_reclaims_the_deleted_sessions_terminal_work() {
        run_async_test_on_stack_budget("workbench-session-delete-retention-test", || {
            session_delete_reclaims_the_deleted_sessions_terminal_work_inner()
        });
    }

    /// The work rail reads the runtime-wide process registry, and deleting a
    /// session deliberately detaches rather than deletes the globally-owned rows
    /// it originated. Without the retention half of the delete, every reset left
    /// the dead session's finished work — trigger deliveries above all — on the
    /// rail forever (FIG-989).
    async fn session_delete_reclaims_the_deleted_sessions_terminal_work_inner() {
        use lash::process::{
            ProcessCompletionAuthority, ProcessInput, ProcessProvenance, ProcessRegistration,
            RecoveryDisposition, SessionScope,
        };
        let data_dir = std::env::temp_dir().join(format!(
            "agent-workbench-session-delete-retention-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&data_dir).expect("create temp workbench dir");
        let process_registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::open(
                &data_dir.join("processes.db"),
                data_dir.join("lash-sessions"),
            )
            .await
            .expect("open registry"),
        ) as Arc<dyn lash::process::ProcessRegistry>;
        let core_store_factory: Arc<dyn lash::persistence::SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(data_dir.join("lash-sessions")),
        );
        let provider = lash::testing::TestProvider::builder()
            .kind("workbench-test")
            .complete_error("session delete retention test should not call the provider")
            .build()
            .into_handle();
        let model =
            lash::ModelSpec::builder("test-model")
                .context_window_tokens(4096)
                .build()
                .expect("model spec");
        let (restate_ingress_url, _restate_requests) = spawn_restate_ingress_capture().await;
        let core = explicit_durable_test_facets(&data_dir)
            .provider(provider)
            .model(model)
            .store_factory(core_store_factory)
            .process_registry(Arc::clone(&process_registry))
            .build(crate::test_core_owner())
            .expect("build core");
        let process_observer = core
            .processes()
            .observer()
            .expect("process observer configured");
        let state = AppState {
            core,
            attachment_store: test_attachment_store(),
            trigger_store: in_memory_trigger_store(),
            process_observer,
            process_work_driver: inert_process_work_driver(Arc::clone(&process_registry)),
            session_ids: WorkbenchSessionIds::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".to_string(),
                model_variant: Default::default(),
            })),
            web_configured: false,
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            queued_work_driver: inert_queued_work_driver(),
            restate_ingress_url,
            restate_admin_url: "http://127.0.0.1:9070".to_string(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            authorization: WorkbenchAuthorization::allow_all(),
        };
        let deleted_session_id = state.current_session_id();
        let surviving_session_id = format!("{deleted_session_id}-survivor");
        let reclaimed = "trigger-delivery-of-deleted-session".to_string();

        // Four rows the work rail can render: the deleted session's finished
        // trigger delivery and its still-running work, plus work another session
        // and the host itself own.
        for (process_id, originator) in [
            (
                "trigger-delivery-of-deleted-session",
                Some(deleted_session_id.clone()),
            ),
            ("live-work-of-deleted-session", Some(deleted_session_id.clone())),
            (
                "trigger-delivery-of-surviving-session",
                Some(surviving_session_id.clone()),
            ),
            ("host-owned-work", None),
        ] {
            process_registry
                .register_process(ProcessRegistration::new(
                    process_id,
                    ProcessInput::External {
                        metadata: json!({ "trigger_delivery": originator.is_some() }),
                    },
                    RecoveryDisposition::ExternallyOwned,
                    match &originator {
                        Some(session_id) => {
                            ProcessProvenance::session(SessionScope::new(session_id))
                        }
                        None => ProcessProvenance::host(),
                    },
                ))
                .await
                .expect("register work-rail process");
        }
        for process_id in [
            "trigger-delivery-of-deleted-session",
            "trigger-delivery-of-surviving-session",
            "host-owned-work",
        ] {
            process_registry
                .complete_process(
                    process_id,
                    lash::process::ProcessAwaitOutput::Success {
                        value: json!({ "delivered": true }),
                        control: None,
                    },
                    ProcessCompletionAuthority::external_owner(),
                )
                .await
                .expect("complete work-rail process");
        }
        let session = state
            .core
            .session(deleted_session_id.clone())
            .open()
            .await
            .expect("open the session under deletion");
        drop(session);

        assert_eq!(
            work_rail_process_ids(&state).await,
            vec![
                "host-owned-work".to_string(),
                "live-work-of-deleted-session".to_string(),
                "trigger-delivery-of-deleted-session".to_string(),
                "trigger-delivery-of-surviving-session".to_string(),
            ],
            "every registered row is on the runtime-wide rail before the delete"
        );

        let scope = state
            .core
            .session_delete_scope(&deleted_session_id)
            .await
            .expect("build session delete scope");
        let effect_host = state.core.effect_host();
        let scoped = effect_host
            .scoped(scope)
            .expect("scope inline session deletion");
        let retention = state
            .delete_session_and_reclaim_processes(&deleted_session_id, scoped)
            .await
            .expect("delete the session and reclaim its finished work");
        assert_eq!(retention.pruned_processes, 1, "one finished row reclaimed");
        assert_eq!(retention.pruned_events, 1, "its terminal event went with it");
        assert_eq!(retention.pruned_trigger_deliveries, 0, "no delivery reserved");

        let rail = work_rail_process_ids(&state).await;
        assert!(!rail.contains(&reclaimed), "reclaimed work leaves the rail");
        assert_eq!(
            rail,
            vec![
                "host-owned-work".to_string(),
                "live-work-of-deleted-session".to_string(),
                "trigger-delivery-of-surviving-session".to_string(),
            ],
            "live work and other owners' finished work stay on the rail"
        );
        assert!(
            matches!(
                process_registry.get_process(&reclaimed).await,
                Err(lash::plugins::PluginError::ProcessNoLongerRetained { .. })
            ),
            "the reclaimed row must read as a payload-free tombstone"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// The process ids the work rail renders with no session selected: the
    /// runtime-wide registry snapshot `/api/work` serves.
    async fn work_rail_process_ids(state: &AppState) -> Vec<String> {
        let Json(work) = list_work(State(state.clone()), Query(SessionQuery::default()))
            .await
            .expect("list runtime-wide work");
        let mut ids = work
            .into_iter()
            .map(|item| item.process.process_id)
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }
}
