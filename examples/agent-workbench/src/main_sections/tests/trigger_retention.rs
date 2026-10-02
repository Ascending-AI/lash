use super::*;
use lash::runtime::ClockWallTime as _;
use lash::triggers::{TriggerOccurrenceFilter, TriggerOccurrenceRequest, TriggerStore as _};

const RECLAIM: &str = "/api/admin/trigger-occurrences/reclaim";
const FORGET: &str = "/api/admin/trigger-occurrences/forget-tombstones";
const WRITTEN_AT: u64 = 4_000_000_000_000;

struct RetentionFixture {
    state: AppState,
    double: lash_restate_test::RestateTestBackend,
    server: tokio::task::JoinHandle<()>,
    url: String,
}

impl Drop for RetentionFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl RetentionFixture {
    async fn new() -> Self {
        Self::with_authorization(WorkbenchAuthorization::allow_all()).await
    }

    async fn with_authorization(authorization: WorkbenchAuthorization) -> Self {
        let double = test_double_backend(0).await;
        let core = test_workbench_core(double.lash_backend());
        install_test_process_worker(&double, &core);
        let state = AppState {
            session_defaults: crate::tests::test_session_defaults(),
            process_observer: core.processes().observer().expect("process observer"),
            core,
            attachment_store: double.stores().attachment_store(),
            session_store_factory: double.stores().session_store_factory(),
            // Maintenance must use the core's configured store, even when a host
            // keeps a different handle for another purpose.
            trigger_store: detached_trigger_store(),
            sessions: WorkbenchSessions::fresh(),
            messages: Arc::new(Mutex::new(Vec::new())),
            selected_model: Arc::new(Mutex::new(ModelSelection {
                model: "test-model".into(),
                model_variant: None,
            })),
            trace_sink: None,
            lashlang_execution: Arc::new(TraceLashlangGraphStore::default()),
            event_tx: SessionEventRegistry::new(16),
            restate_ingress_url: "http://127.0.0.1:8080".into(),
            restate_admin_url: "http://127.0.0.1:9070".into(),
            restate_http: reqwest::Client::new(),
            restate_cron_job_keys: Arc::new(Mutex::new(BTreeMap::new())),
            mail_world: mail::MailWorld::new(),
            active_turns: ActiveTurns::default(),
            unknown_turn_terminals: UnknownTurnTerminals::default(),
            authorization,
            approvals: approvals::WorkbenchApprovals::in_memory().expect("approvals"),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind operator routes");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("operator address")
        );
        let app = trigger_occurrence_admin_routes().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve operator routes");
        });
        Self {
            state,
            double,
            server,
            url,
        }
    }

    async fn post(&self, route: &str, request: Value) -> Value {
        let response = self
            .state
            .restate_http
            .post(format!("{}{route}", self.url))
            .json(&request)
            .send()
            .await
            .expect("operator request");
        let status = response.status();
        let body = response.text().await.expect("operator response");
        println!("POST {route} {request}: {status} {body}");
        assert_eq!(status, StatusCode::OK, "operator route: {body}");
        serde_json::from_str(&body).expect("operator JSON")
    }

    async fn reclaim(&self) -> lash::triggers::TriggerOccurrenceReclamationReport {
        serde_json::from_value(
            self.post(RECLAIM, json!({"cutoff_epoch_ms": u64::MAX}))
                .await,
        )
        .expect("typed reclamation report")
    }

    async fn forget(&self, cutoff: u64) -> usize {
        let body = self
            .post(FORGET, json!({"written_before_epoch_ms": cutoff}))
            .await;
        assert_eq!(body["written_before_epoch_ms"], cutoff);
        serde_json::from_value(body["forgotten"].clone()).expect("exact forget count")
    }

    async fn ingest(&self, request: &TriggerOccurrenceRequest) {
        self.double
            .stores()
            .trigger_store()
            .ingest_occurrence(request.clone())
            .await
            .expect("ingest occurrence");
    }

    async fn refused(&self, request: &TriggerOccurrenceRequest) {
        let triggers = self.double.stores().trigger_store();
        let before = triggers
            .list_occurrences(TriggerOccurrenceFilter::default())
            .await
            .expect("occurrences");
        let deliveries = triggers.list_deliveries().await.expect("deliveries");
        let processes = self
            .state
            .core
            .processes()
            .list(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("process baseline");
        let error = self
            .emit(request.clone())
            .await
            .expect_err("reclaimed redelivery must be refused");
        let code = match &error {
            lash::EmbedError::Plugin(PluginError::Runtime(error)) => &error.code,
            lash::EmbedError::Plugin(PluginError::RuntimeEffectController(error)) => &error.code,
            error => panic!("untyped redelivery refusal: {error:?}"),
        };
        assert_eq!(
            *code,
            lash::runtime::RuntimeErrorCode::TriggerOccurrenceReclaimed
        );
        assert_eq!(
            triggers
                .list_occurrences(TriggerOccurrenceFilter::default())
                .await
                .expect("occurrences"),
            before
        );
        assert_eq!(
            triggers.list_deliveries().await.expect("deliveries"),
            deliveries
        );
        assert_eq!(
            self.state
                .core
                .processes()
                .list(&lash::process::ProcessListFilter {
                    status: lash::process::ProcessStatusFilter::Any,
                    ..Default::default()
                })
                .await
                .expect("process count")
                .len(),
            processes.len()
        );
        println!(
            "redelivery {}: TriggerOccurrenceReclaimed; 0 new occurrences, 0 new deliveries, 0 new processes",
            request.idempotency_key
        );
    }

    async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> lash::Result<lash::triggers::TriggerEmitReport> {
        let core = self.state.core.clone();
        // A new handler journal is essential: replaying the first handler would
        // answer its recorded receipt without testing the retained tombstone.
        run_in_test_handler(
            &self.double,
            lash::runtime::AdmittedScope::runtime_operation(format!(
                "retention-redelivery:{}",
                uuid::Uuid::new_v4()
            )),
            Arc::new(move |scoped| {
                let core = core.clone();
                let request = request.clone();
                Box::pin(async move { core.triggers().emit(request, scoped).await })
            }),
        )
        .await
    }
}

struct DenyMaintenance;

impl WorkbenchAuthorizer for DenyMaintenance {
    fn authorize(&self, action: &WorkbenchAuthorizationAction) -> Result<(), AppError> {
        assert!(matches!(
            action,
            WorkbenchAuthorizationAction::RunStoreMaintenance
        ));
        Err(AppError::forbidden("operator maintenance denied"))
    }
}

#[test]
fn operator_forget_requires_an_explicit_cutoff_and_operator_authorization() {
    run_async_test_on_stack_budget("operator-forget-authorization", || async {
        let fixture = RetentionFixture::new().await;
        for request in [
            json!({}),
            json!({"written_before_epoch_ms": 1, "typo": true}),
        ] {
            let response = fixture
                .state
                .restate_http
                .post(format!("{}{FORGET}", fixture.url))
                .json(&request)
                .send()
                .await
                .expect("invalid operator request");
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }
        let denied = RetentionFixture::with_authorization(WorkbenchAuthorization::with_authorizer(
            Arc::new(DenyMaintenance),
        ))
        .await;
        denied.ingest(&unmatched("denied-forget")).await;
        assert_eq!(
            denied
                .double
                .stores()
                .trigger_store()
                .reclaim_trigger_occurrences(u64::MAX)
                .await
                .expect("seed fence")
                .reclaimed_occurrence_count,
            1
        );
        let response = denied
            .state
            .restate_http
            .post(format!("{}{FORGET}", denied.url))
            .json(&json!({"written_before_epoch_ms": u64::MAX}))
            .send()
            .await
            .expect("denied operator request");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            denied
                .state
                .core
                .processes()
                .forget_trigger_tombstones(u64::MAX)
                .await
                .expect("denial changed nothing"),
            1
        );
        println!(
            "invalid cutoff: 422; unknown field: 422; unauthorized forget: 403; 1 tombstone retained"
        );
    });
}

fn unmatched(key: &str) -> TriggerOccurrenceRequest {
    TriggerOccurrenceRequest::new("retention.probe", "operator", json!({}), key)
}

#[test]
fn operator_reclaim_preserves_redelivery_fences() {
    run_async_test_on_stack_budget("operator-reclaim-retention", || async {
        let fixture = RetentionFixture::new().await;
        let request = unmatched("survive-reclaim");
        fixture.ingest(&request).await;
        let report = fixture.reclaim().await;
        assert_eq!(report.inspected_occurrence_count, 1);
        assert_eq!(report.reclaimed_occurrence_count, 1);
        fixture
            .double
            .test_clock()
            .advance(100 * 365 * 24 * 60 * 60 * 1000);
        assert_eq!(fixture.reclaim().await.reclaimed_occurrence_count, 0);
        fixture.refused(&request).await;
    });
}

#[test]
fn operator_forget_reports_exact_exclusive_write_time_count() {
    run_async_test_on_stack_budget("operator-forget-count", || async {
        let fixture = RetentionFixture::new().await;
        let older = [unmatched("older-one"), unmatched("older-two")];
        fixture.double.test_clock().set(WRITTEN_AT - 10);
        for request in &older {
            fixture.ingest(request).await;
        }
        fixture.double.test_clock().set(WRITTEN_AT);
        // Seed tombstones through the configured store so the missing forget
        // route is independently red before reclaim's facade fix lands.
        assert_eq!(
            fixture
                .double
                .stores()
                .trigger_store()
                .reclaim_trigger_occurrences(u64::MAX)
                .await
                .expect("older tombstones")
                .reclaimed_occurrence_count,
            2
        );
        let retained = unmatched("at-cutoff");
        fixture.ingest(&retained).await;
        fixture.double.test_clock().set(WRITTEN_AT + 10);
        assert_eq!(
            fixture
                .double
                .stores()
                .trigger_store()
                .reclaim_trigger_occurrences(u64::MAX)
                .await
                .expect("newer tombstone")
                .reclaimed_occurrence_count,
            1
        );
        assert_eq!(fixture.forget(WRITTEN_AT).await, 0);
        assert_eq!(fixture.forget(WRITTEN_AT + 10).await, 2);
        assert_eq!(fixture.forget(WRITTEN_AT + 10).await, 0);
        for request in &older {
            fixture.ingest(request).await;
        }
        fixture.refused(&retained).await;
        assert_eq!(fixture.forget(u64::MAX).await, 1);
    });
}

#[test]
fn operator_redelivery_after_forget_runs_and_retained_tombstone_suppresses() {
    run_async_test_on_stack_budget("operator-forget-redelivery", || async {
        let fixture = RetentionFixture::new().await;
        let session_id = fixture.state.current_session_id();
        let session = crate::created_session(&fixture.state.core, session_id.clone())
            .await
            .open()
            .await
            .expect("session");
        register_test_trigger(&session).await;
        let _hold = fixture.double.hold_session_drive(&session_id).await;
        let request = TriggerOccurrenceRequest::new(BUTTON_TRIGGER_SOURCE_TYPE,
            lash::triggers::empty_trigger_source_key(BUTTON_TRIGGER_SOURCE_TYPE).expect("source key"),
            json!({"button":"Blue", "message":"retention probe", "pressed_at":"2026-10-02T00:00:00Z"}),
            "forget-matched").with_source(json!({})).for_session(&session_id);
        let original = fixture.emit(request.clone()).await.expect("first fire");
        assert_eq!(original.started_process_ids().len(), 1);
        let original_id = original.started_process_ids()[0].clone();
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.state.core.processes().await_output(&original_id),
        )
        .await
        .expect("settle deadline")
        .expect("settle process");
        assert_eq!(
            output.terminal_status(),
            Some(lash::process::TerminalProcessStatus::Completed)
        );
        println!("original process {original_id}: terminal success");
        let prune = fixture
            .state
            .core
            .processes()
            .prune(
                u64::MAX,
                None,
                lash::process::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("terminal retention");
        assert_eq!(prune.pruned_processes, 1);
        assert_eq!(prune.pruned_trigger_deliveries, 1);
        println!("terminal retention: 1 process, 1 delivery pruned");
        fixture.refused(&request).await;
        let cutoff = fixture.double.test_clock().timestamp_ms() + 10;
        assert_eq!(fixture.forget(cutoff).await, 1);
        fixture.double.test_clock().set(cutoff + 10);
        let mut retained = request.clone();
        retained.idempotency_key = "retained-matched".into();
        let retained_fire = fixture
            .emit(retained.clone())
            .await
            .expect("retained first fire");
        assert_eq!(retained_fire.started_process_ids().len(), 1);
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            fixture
                .state
                .core
                .processes()
                .await_output(&retained_fire.started_process_ids()[0]),
        )
        .await
        .expect("retained deadline")
        .expect("retained settles");
        assert_eq!(
            output.terminal_status(),
            Some(lash::process::TerminalProcessStatus::Completed)
        );
        let prune = fixture
            .state
            .core
            .processes()
            .prune(
                u64::MAX,
                None,
                lash::process::ProjectionWatermark::NoProjector,
            )
            .await
            .expect("retain later fence");
        assert_eq!(prune.pruned_processes, 1);
        assert_eq!(prune.pruned_trigger_deliveries, 1);
        assert_eq!(fixture.forget(cutoff).await, 0);
        fixture.refused(&retained).await;
        let redelivery = fixture
            .emit(request.clone())
            .await
            .expect("forgotten redelivery");
        assert_eq!(redelivery.started_process_ids().len(), 1);
        assert_eq!(
            fixture
                .double
                .stores()
                .trigger_store()
                .list_occurrences(TriggerOccurrenceFilter::default())
                .await
                .expect("one new occurrence")
                .len(),
            1
        );
        assert_eq!(
            fixture
                .double
                .stores()
                .trigger_store()
                .list_deliveries()
                .await
                .expect("one new delivery")
                .len(),
            1
        );
        let new_id = redelivery.started_process_ids()[0].clone();
        assert_ne!(new_id, original_id);
        let output = tokio::time::timeout(
            Duration::from_secs(10),
            fixture.state.core.processes().await_output(&new_id),
        )
        .await
        .expect("redelivery deadline")
        .expect("redelivery settles");
        assert_eq!(
            output.terminal_status(),
            Some(lash::process::TerminalProcessStatus::Completed)
        );
        println!(
            "forgotten redelivery: 1 new occurrence, 1 new delivery, 1 new process {new_id}, terminal success"
        );
        fixture.refused(&retained).await;
    });
}
