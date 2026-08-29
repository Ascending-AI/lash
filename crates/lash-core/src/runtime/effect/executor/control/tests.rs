use super::*;

struct ToolIntentGateSink {
    gate: Arc<tokio::sync::Mutex<()>>,
    admissions: std::sync::atomic::AtomicUsize,
}

impl Default for ToolIntentGateSink {
    fn default() -> Self {
        Self {
            gate: Arc::new(tokio::sync::Mutex::new(())),
            admissions: std::sync::atomic::AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ToolIntentOutcomeSink for ToolIntentGateSink {
    async fn lock_submission_gate(&self, _replay_key: &str) -> ToolIntentSubmissionGuard {
        ToolIntentSubmissionGuard::from_owned_mutex_guard(Arc::clone(&self.gate).lock_owned().await)
    }

    async fn admit(
        &self,
        _record: crate::ToolIntentSubmissionRecord,
    ) -> Result<crate::ToolIntentSubmissionAdmission, RuntimeError> {
        self.admissions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(crate::ToolIntentSubmissionAdmission::Admitted)
    }

    async fn complete_submission(
        &self,
        _identity: &crate::ToolIntentIdentity,
        _outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn retain_in_journal(
        &self,
        _identity: &crate::ToolIntentIdentity,
        _submitted: crate::ToolIntent,
        _outcome: crate::ToolIntentExecutionOutcome,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

fn test_tool_intent() -> (crate::ToolIntentIdentity, crate::ToolIntent) {
    let identity = crate::derive_tool_intent_identity(
        "tool-intent-gate-session",
        "tool-intent-gate-turn",
        Some("tool-intent-gate-call"),
        0,
    )
    .expect("tool-intent gate identity");
    let intent = crate::ToolIntent::CancelProcess(crate::CancelProcessIntent {
        session_id: "tool-intent-gate-session".to_string(),
        process_id: "tool-intent-gate-process".to_string(),
        reason: None,
    });
    (identity, intent)
}

#[tokio::test]
async fn runtime_tool_intent_preparation_holds_the_gate_until_dropped() {
    let host = crate::NativeEffectHost::default();
    let sink = ToolIntentGateSink::default();
    let (identity, intent) = test_tool_intent();
    let first = host
        .prepare_tool_intent(&sink, &identity, intent.clone())
        .await
        .expect("first tool-intent preparation");

    let blocked = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        host.prepare_tool_intent(&sink, &identity, intent.clone()),
    )
    .await;
    assert!(
        blocked.is_err(),
        "a duplicate preparation must remain blocked while the first preparation is held"
    );

    drop(first);
    let second = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        host.prepare_tool_intent(&sink, &identity, intent),
    )
    .await
    .expect("duplicate preparation unblocks after release")
    .expect("second tool-intent preparation");
    assert!(matches!(second, ToolIntentPreparation::RuntimeOwned { .. }));
    assert_eq!(sink.admissions.load(std::sync::atomic::Ordering::SeqCst), 2);
}

struct CompletionKeyProbe {
    issue_calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl AwaitEventResolver for CompletionKeyProbe {
    async fn await_event_key(
        &self,
        _scope: &ExecutionScope,
        _wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.issue_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(RuntimeError::new(
            RuntimeErrorCode::ToolCompletionKeyProcessLifetime,
            "probe must not issue a key",
        ))
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for CompletionKeyProbe {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("completion-key preparation test does not execute effects")
    }
}

#[tokio::test]
async fn completion_key_preparation_issues_nothing_when_deferral_is_impossible() {
    let probe = CompletionKeyProbe {
        issue_calls: std::sync::atomic::AtomicUsize::new(0),
    };
    let preparation = probe
        .prepare_completion_key(
            &ExecutionScope::turn("completion-key-session", "completion-key-turn"),
            AwaitEventWaitIdentity::tool_completion("completion-key-call"),
            false,
        )
        .await
        .expect("completion-key preparation");

    assert!(
        matches!(preparation, CompletionKeyPreparation::NotNeeded),
        "non-deferring work must select NotNeeded"
    );
    assert_eq!(
        probe.issue_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "NotNeeded must not issue an await-event key"
    );
}

struct TestResolver;

impl AwaitEventResolver for TestResolver {}

#[async_trait::async_trait]
impl RuntimeEffectController for TestResolver {
    async fn execute_effect(
        &self,
        _envelope: RuntimeEffectEnvelope,
        _local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        unreachable!("queued-lane controller tests do not execute effects")
    }
}

struct FakeQueuedLaneProbe {
    attempts: std::sync::Mutex<std::collections::VecDeque<QueuedLaneAttempt>>,
    try_calls: std::sync::atomic::AtomicUsize,
    pause_calls: std::sync::atomic::AtomicUsize,
}

impl FakeQueuedLaneProbe {
    fn new(attempts: impl IntoIterator<Item = QueuedLaneAttempt>) -> Self {
        Self {
            attempts: std::sync::Mutex::new(attempts.into_iter().collect()),
            try_calls: std::sync::atomic::AtomicUsize::new(0),
            pause_calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn try_calls(&self) -> usize {
        self.try_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn pause_calls(&self) -> usize {
        self.pause_calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl QueuedLaneProbe for FakeQueuedLaneProbe {
    async fn try_acquire(&self) -> Result<QueuedLaneAttempt, RuntimeError> {
        self.try_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self
            .attempts
            .lock()
            .expect("fake queued-lane attempts")
            .pop_front()
            .expect("fake queued-lane attempt available"))
    }

    async fn pause(&self, _slice: std::time::Duration) {
        self.pause_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

fn queued_lane_holder(expires_at_epoch_ms: u64) -> QueuedLaneHolder {
    QueuedLaneHolder(crate::store::SessionExecutionLease {
        session_id: "queued-lane-test".to_string(),
        owner: crate::LeaseOwnerIdentity::opaque("holder", "holder:incarnation"),
        executor_id: "holder-executor".to_string(),
        lease_token: "holder-token".to_string(),
        fencing_token: 7,
        claimed_at_epoch_ms: 1_000,
        lease_term_ms: 6_400,
        expires_at_epoch_ms,
    })
}

async fn queued_lane_guard() -> QueuedLaneGuard {
    let clock: Arc<dyn crate::Clock> = Arc::new(crate::testing::TestClock::new(1_000));
    let store = Arc::new(crate::runtime::InMemorySessionStore::with_clock(
        Arc::clone(&clock),
    ));
    let guard = crate::runtime::session_execution_lease::SessionExecutionLeaseGuard::try_acquire(
        store as Arc<dyn crate::store::RuntimePersistence>,
        "queued-lane-test",
        &crate::LeaseOwnerIdentity::opaque("owner", "owner:incarnation"),
        "queued-lane-test-executor",
        crate::LeaseTimings::default(),
        clock,
    )
    .await
    .expect("queued-lane test claim")
    .expect("queued-lane test guard");
    QueuedLaneGuard(guard)
}

#[tokio::test]
async fn default_queued_lane_acquisition_stops_after_one_busy_attempt() {
    let probe = Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
        queued_lane_holder(7_400),
    )]));

    let result = TestResolver
        .acquire_queued_lane(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await
        .expect("default queued-lane acquisition");

    assert!(matches!(result, QueuedLaneAcquisition::NotAcquired));
    assert_eq!(probe.try_calls(), 1);
    assert_eq!(probe.pause_calls(), 0);
}

#[tokio::test]
async fn provided_wait_retries_a_crashed_looking_holder_until_acquired() {
    let probe = Arc::new(FakeQueuedLaneProbe::new([
        QueuedLaneAttempt::Busy(queued_lane_holder(7_400)),
        QueuedLaneAttempt::Acquired(queued_lane_guard().await),
    ]));

    let result = TestResolver
        .wait_out_crashed_lane_holder(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await
        .expect("provided queued-lane wait");

    assert!(
        matches!(result, QueuedLaneAcquisition::Acquired(_)),
        "expected acquisition after one crashed-looking holder; try_calls={}, pause_calls={}",
        probe.try_calls(),
        probe.pause_calls(),
    );
    assert_eq!(probe.try_calls(), 2);
    assert_eq!(probe.pause_calls(), 1);
}

#[tokio::test]
async fn provided_wait_reports_a_renewing_holder_as_typed_retryable_busy() {
    #[cfg(feature = "otel-trace")]
    let metrics = crate::operational_metrics::TestMetrics::install();
    let probe = Arc::new(FakeQueuedLaneProbe::new([
        QueuedLaneAttempt::Busy(queued_lane_holder(7_400)),
        QueuedLaneAttempt::Busy(queued_lane_holder(7_401)),
    ]));

    let result = TestResolver
        .wait_out_crashed_lane_holder(
            Arc::clone(&probe) as Arc<dyn QueuedLaneProbe>,
            CancellationToken::new(),
        )
        .await;

    let Err(error) = result else {
        panic!("a renewing holder must hand pacing back to the engine")
    };
    assert_eq!(error.code, RuntimeErrorCode::SessionExecutionLaneBusy);
    assert!(error.is_retryable());
    assert_eq!(probe.try_calls(), 2);
    assert_eq!(probe.pause_calls(), 1);
    #[cfg(feature = "otel-trace")]
    {
        assert_eq!(
            metrics.histogram_count("lash.session_execution_lane.contention_wait.duration"),
            1
        );
        assert_eq!(
            metrics.counter_value("lash.session_execution_lane.give_ups"),
            1
        );
    }
}

async fn acquire_through_task_controller(
    controller: &TestResolver,
    probe: Arc<dyn QueuedLaneProbe>,
) -> Result<QueuedLaneAcquisition, RuntimeError> {
    let (scoped, mut requests) = EffectTaskController::scoped(
        controller,
        ExecutionScope::queue_drain("queued-lane-test", "drain"),
    )?;
    let acquire = scoped
        .controller()
        .acquire_queued_lane(probe, CancellationToken::new());
    let drive = async {
        requests
            .recv()
            .await
            .expect("queued-lane task request")
            .into_future(controller)
            .await;
    };
    let (result, ()) = tokio::join!(acquire, drive);
    result
}

#[tokio::test]
async fn queued_lane_acquisition_round_trips_through_the_task_controller() {
    let controller = TestResolver;
    let busy = acquire_through_task_controller(
        &controller,
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
            queued_lane_holder(7_400),
        )])),
    )
    .await
    .expect("busy queued-lane proxy response");
    assert!(matches!(busy, QueuedLaneAcquisition::NotAcquired));

    let acquired = acquire_through_task_controller(
        &controller,
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Acquired(
            queued_lane_guard().await,
        )])),
    )
    .await
    .expect("acquired queued-lane proxy response");
    assert!(matches!(acquired, QueuedLaneAcquisition::Acquired(_)));
}

#[tokio::test]
async fn closed_task_controller_returns_a_typed_queued_lane_error() {
    let controller = TestResolver;
    let (scoped, requests) = EffectTaskController::scoped(
        &controller,
        ExecutionScope::queue_drain("queued-lane-test", "closed"),
    )
    .expect("queued-lane task proxy");
    drop(requests);

    let result = scoped
        .controller()
        .acquire_queued_lane(
            Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
                queued_lane_holder(7_400),
            )])),
            CancellationToken::new(),
        )
        .await;

    let Err(error) = result else {
        panic!("a closed queued-lane task must return a typed error")
    };
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

#[tokio::test]
async fn dropped_queued_lane_response_returns_a_typed_error() {
    let controller = TestResolver;
    let (scoped, mut requests) = EffectTaskController::scoped(
        &controller,
        ExecutionScope::queue_drain("queued-lane-test", "dropped-response"),
    )
    .expect("queued-lane task proxy");

    let acquire = scoped.controller().acquire_queued_lane(
        Arc::new(FakeQueuedLaneProbe::new([QueuedLaneAttempt::Busy(
            queued_lane_holder(7_400),
        )])),
        CancellationToken::new(),
    );
    let accept_then_drop = async {
        match requests.recv().await.expect("queued-lane task request") {
            EffectControllerTaskRequest::AcquireQueuedLane { response, .. } => drop(response),
            _ => panic!("expected a queued-lane request"),
        }
    };
    let (result, ()) = tokio::join!(acquire, accept_then_drop);

    let Err(error) = result else {
        panic!("a dropped queued-lane response must return a typed error")
    };
    assert_eq!(
        error.code,
        RuntimeErrorCode::RuntimeEffectControllerTaskClosed
    );
}

#[test]
fn queued_lane_holder_debug_redacts_the_lease_token() {
    let holder = queued_lane_holder(9_000);
    let rendered = format!("{holder:?}");
    assert!(
        !rendered.contains("holder-token"),
        "Debug output must not leak the lease token: {rendered}"
    );
    assert!(rendered.contains("holder-executor"));
}

#[test]
fn journal_identity_is_typed_and_session_qualified() {
    let scopes = [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::queue_drain("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process("shared"),
        ExecutionScope::runtime_operation("shared"),
    ];
    let identities = scopes
        .iter()
        .map(|scope| scope.journal_identity().expect("durable identity"))
        .collect::<Vec<_>>();
    let keys = identities
        .iter()
        .map(EffectJournalIdentity::key)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(keys.len(), scopes.len());
    for identity in &identities[..3] {
        assert_eq!(identity.session_id(), Some("session"));
    }
    for identity in &identities[3..] {
        assert_eq!(identity.session_id(), None);
    }
}

/// Every variant survives the round trip, not just the one the drain
/// happens to exercise.
///
/// `from_journal_key` is the drain's only way back from a journal row to a
/// scope, and it re-derives each variant from a `kind` string by hand. A
/// variant whose forward and backward spellings drift apart makes every
/// row written under it undrainable — refused as a scope no version of this
/// runtime writes, which is exactly the wrong answer for a scope this
/// version writes constantly. Only the whole set proves the mapping; one
/// variant proves the plumbing.
#[test]
fn every_scope_variant_round_trips_through_its_journal_key() {
    for scope in [
        ExecutionScope::turn("session", "shared"),
        ExecutionScope::queue_drain("session", "shared"),
        ExecutionScope::session_delete("session"),
        ExecutionScope::process("shared"),
        ExecutionScope::runtime_operation("shared"),
    ] {
        let key = scope
            .journal_identity()
            .expect("durable identity")
            .key()
            .to_string();
        assert_eq!(
            ExecutionScope::from_journal_key(&key),
            Some(scope.clone()),
            "scope {scope:?} did not come back from its own journal key `{key}`"
        );
    }
}

/// A key this build cannot read is refused rather than guessed at, which is
/// what lets the drain treat `None` as corruption instead of as a default.
#[test]
fn a_journal_key_this_build_cannot_read_is_refused() {
    for key in [
        "",
        "not json",
        r#"{"version":1,"kind":"turn","session_id":"s","execution_id":"t"}"#,
        r#"{"version":2,"kind":"nonsense","execution_id":"t"}"#,
        // Right kind, missing the field that kind requires.
        r#"{"version":2,"kind":"turn","session_id":"s"}"#,
        // Decodes, but to a scope the forward direction would refuse.
        r#"{"version":2,"kind":"process","execution_id":""}"#,
    ] {
        assert_eq!(
            ExecutionScope::from_journal_key(key),
            None,
            "`{key}` is not a scope this build wrote"
        );
    }
}
