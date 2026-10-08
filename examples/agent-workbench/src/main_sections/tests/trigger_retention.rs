use super::*;
use lash::runtime::ClockWallTime as _;
use lash::triggers::{TriggerOccurrenceFilter, TriggerOccurrenceRequest};

const RECLAIM: &str = "/api/admin/trigger-occurrences/reclaim";
const FORGET: &str = "/api/admin/trigger-occurrences/forget-tombstones";
const WRITTEN_AT: u64 = 4_000_000_000_000;

/// A wall clock a law sets and advances, ticking in real time from where it
/// was last set; monotonic reads and sleeps are the system's.
#[derive(Debug)]
struct SettableClock {
    base_ms: std::sync::atomic::AtomicI64,
    set_at: Mutex<std::time::Instant>,
}

impl SettableClock {
    fn new() -> Self {
        Self {
            base_ms: std::sync::atomic::AtomicI64::new(chrono::Utc::now().timestamp_millis()),
            set_at: Mutex::new(std::time::Instant::now()),
        }
    }

    fn set(&self, epoch_ms: u64) {
        let mut set_at = self.set_at.lock_recover();
        self.base_ms.store(
            i64::try_from(epoch_ms).expect("an epoch in range"),
            std::sync::atomic::Ordering::SeqCst,
        );
        *set_at = std::time::Instant::now();
    }

    fn advance(&self, by_ms: u64) {
        self.set(self.timestamp_ms() + by_ms);
    }
}

#[async_trait]
impl lash::runtime::Clock for SettableClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let set_at = self.set_at.lock_recover();
        let elapsed = i64::try_from(set_at.elapsed().as_millis()).expect("elapsed in range");
        chrono::DateTime::from_timestamp_millis(
            self.base_ms.load(std::sync::atomic::Ordering::SeqCst) + elapsed,
        )
        .expect("a representable instant")
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: std::time::Instant) {
        tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
    }
}

/// A workbench whose store set keeps a settable wall clock, with its
/// operator routes served over HTTP.
struct RetentionFixture {
    workbench: Workbench,
    clock: Arc<SettableClock>,
    http: reqwest::Client,
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
        Self::with_authorization(WorkbenchAuthorization::allow_all(), silent_provider()).await
    }

    async fn with_authorization(
        authorization: WorkbenchAuthorization,
        provider: ProviderHandle,
    ) -> Self {
        let clock = Arc::new(SettableClock::new());
        let stores: Arc<dyn lash::StoreSet> = Arc::new(
            lash::sqlite::SqliteStoreSet::memory_with_clock(
                Arc::clone(&clock) as Arc<dyn lash::runtime::Clock>
            )
            .await
            .expect("open a SQLite memory store set on the settable clock"),
        );
        let mut workbench = Workbench::builder(provider).stores(stores).build().await;
        workbench.state.authorization = authorization;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind operator routes");
        let url = format!(
            "http://{}",
            listener.local_addr().expect("operator address")
        );
        let app = trigger_occurrence_admin_routes().with_state(workbench.state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve operator routes");
        });
        Self {
            workbench,
            clock,
            http: reqwest::Client::new(),
            server,
            url,
        }
    }

    fn state(&self) -> &AppState {
        &self.workbench.state
    }

    fn triggers(&self) -> Arc<dyn lash::triggers::TriggerStore> {
        self.workbench.stores.trigger_store()
    }

    async fn post(&self, route: &str, request: Value) -> Value {
        let response = self
            .http
            .post(format!("{}{route}", self.url))
            .json(&request)
            .send()
            .await
            .expect("operator request");
        let status = response.status();
        let body = response.text().await.expect("operator response");
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

    /// Record `request`'s occurrence: an emission records it in the same
    /// transaction that starts its deliveries (FIG-5175).
    async fn ingest(&self, request: &TriggerOccurrenceRequest) {
        self.emit(request.clone())
            .await
            .expect("record the occurrence");
    }

    async fn process_count(&self) -> usize {
        self.state()
            .core
            .processes()
            .list(&lash::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            })
            .await
            .expect("process listing")
            .len()
    }

    async fn refused(&self, request: &TriggerOccurrenceRequest) {
        let triggers = self.triggers();
        let before = triggers
            .list_occurrences(TriggerOccurrenceFilter::default())
            .await
            .expect("occurrences");
        let deliveries = triggers.list_deliveries().await.expect("deliveries");
        let processes = self.process_count().await;
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
        assert_eq!(self.process_count().await, processes);
    }

    /// Emit `request` under a fresh runtime operation of this host, so a
    /// redelivery is a new act rather than a replay of an earlier one.
    async fn emit(
        &self,
        request: TriggerOccurrenceRequest,
    ) -> lash::Result<lash::triggers::TriggerEmitReport> {
        let operation = self
            .state()
            .core
            .session_administration()
            .await
            .effect_host()
            .scoped(lash::runtime::AdmittedScope::runtime_operation(format!(
                "retention-redelivery:{}",
                uuid::Uuid::new_v4()
            )))
            .expect("a runtime-operation scope");
        self.state().core.triggers().emit(request, operation).await
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_forget_requires_an_explicit_cutoff_and_operator_authorization() {
    let fixture = RetentionFixture::new().await;
    for request in [
        json!({}),
        json!({"written_before_epoch_ms": 1, "typo": true}),
    ] {
        let response = fixture
            .http
            .post(format!("{}{FORGET}", fixture.url))
            .json(&request)
            .send()
            .await
            .expect("invalid operator request");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
    let denied = RetentionFixture::with_authorization(
        WorkbenchAuthorization::with_authorizer(Arc::new(DenyMaintenance)),
        silent_provider(),
    )
    .await;
    denied.ingest(&unmatched("denied-forget")).await;
    assert_eq!(
        denied
            .triggers()
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("seed fence")
            .reclaimed_occurrence_count,
        1
    );
    let response = denied
        .http
        .post(format!("{}{FORGET}", denied.url))
        .json(&json!({"written_before_epoch_ms": u64::MAX}))
        .send()
        .await
        .expect("denied operator request");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        denied
            .state()
            .core
            .processes()
            .forget_trigger_tombstones(u64::MAX)
            .await
            .expect("denial changed nothing"),
        1
    );
}

fn unmatched(key: &str) -> TriggerOccurrenceRequest {
    TriggerOccurrenceRequest::new("retention.probe", "operator", json!({}), key)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_reclaim_preserves_redelivery_fences() {
    let fixture = RetentionFixture::new().await;
    let request = unmatched("survive-reclaim");
    fixture.ingest(&request).await;
    let report = fixture.reclaim().await;
    assert_eq!(report.inspected_occurrence_count, 1);
    assert_eq!(report.reclaimed_occurrence_count, 1);
    fixture.clock.advance(100 * 365 * 24 * 60 * 60 * 1000);
    assert_eq!(fixture.reclaim().await.reclaimed_occurrence_count, 0);
    fixture.refused(&request).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_forget_reports_exact_exclusive_write_time_count() {
    let fixture = RetentionFixture::new().await;
    let older = [unmatched("older-one"), unmatched("older-two")];
    fixture.clock.set(WRITTEN_AT - 10);
    for request in &older {
        fixture.ingest(request).await;
    }
    fixture.clock.set(WRITTEN_AT);
    assert_eq!(
        fixture
            .triggers()
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("older tombstones")
            .reclaimed_occurrence_count,
        2
    );
    let retained = unmatched("at-cutoff");
    fixture.ingest(&retained).await;
    fixture.clock.set(WRITTEN_AT + 10);
    assert_eq!(
        fixture
            .triggers()
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operator_redelivery_after_forget_runs_and_retained_tombstone_suppresses() {
    let fixture = RetentionFixture::with_authorization(
        WorkbenchAuthorization::allow_all(),
        replying_provider(reset_chat_tests::BUTTON_TRIGGER_REGISTRATION),
    )
    .await;
    let session_id = fixture.state().current_session_id();
    reset_chat_tests::register_button_trigger(fixture.state()).await;
    let request = TriggerOccurrenceRequest::new(
        BUTTON_TRIGGER_SOURCE_TYPE,
        lash::triggers::empty_trigger_source_key(BUTTON_TRIGGER_SOURCE_TYPE).expect("source key"),
        json!({"button":"Blue", "message":"retention probe", "pressed_at":"2026-10-02T00:00:00Z"}),
        "forget-matched",
    )
    .with_source(json!({}))
    .for_session(&session_id);
    let original = fixture.emit(request.clone()).await.expect("first fire");
    assert_eq!(original.started_process_ids().len(), 1);
    let original_id = original.started_process_ids()[0].clone();
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        fixture.state().core.processes().await_output(&original_id),
    )
    .await
    .expect("settle deadline")
    .expect("settle process");
    assert_eq!(
        output.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed),
        "process output: {output:?}"
    );
    let prune = fixture
        .state()
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
    fixture.refused(&request).await;
    let cutoff = fixture.clock.timestamp_ms() + 10;
    assert_eq!(fixture.forget(cutoff).await, 1);
    fixture.clock.set(cutoff + 10);
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
            .state()
            .core
            .processes()
            .await_output(&retained_fire.started_process_ids()[0]),
    )
    .await
    .expect("retained deadline")
    .expect("retained settles");
    assert_eq!(
        output.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed),
        "process output: {output:?}"
    );
    let prune = fixture
        .state()
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
            .triggers()
            .list_occurrences(TriggerOccurrenceFilter::default())
            .await
            .expect("one new occurrence")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .triggers()
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
        fixture.state().core.processes().await_output(&new_id),
    )
    .await
    .expect("redelivery deadline")
    .expect("redelivery settles");
    assert_eq!(
        output.terminal_status(),
        Some(lash::process::TerminalProcessStatus::Completed),
        "process output: {output:?}"
    );
    fixture.refused(&retained).await;
}
