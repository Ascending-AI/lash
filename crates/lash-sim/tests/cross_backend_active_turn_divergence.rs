//! DISCRIMINATOR (scratch) for the cross-backend SQLite active-turn divergence.
//!
//! Builds TWO real `lash::LashCore`s identical except the session store
//! factory (in-memory vs lash-sqlite-store) and drives the SAME operation
//! sequence on both over the production transport seam with an UN-GATED
//! `ScriptedLlmHttpTransport` (no `ScriptedTransportSchedule` / no per-event
//! Notify gating — that gating is the harness artifact under test). Compares
//! committed assistant message + cumulative provider exchange count per turn.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use lash::persistence::{
    AttachmentStore, FileAttachmentStore, InMemoryAttachmentStore,
    InMemoryProcessExecutionEnvStore, InMemorySessionStoreFactory, ProcessExecutionEnvStore,
};
use lash::{LashCore, PendingTurnInputCancelOutcome, TurnInput};
use lash_core::{
    AwaitEventKey, AwaitEventResolver, AwaitEventWaitIdentity, EffectHost, ExecutionScope,
    InlineRuntimeEffectController, Resolution, ResolveOutcome, RuntimeEffectController,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectLocalExecutor,
    RuntimeEffectOutcome, RuntimeError, ScopedEffectController, SessionStoreFactory,
    TurnInputCheckpointBoundary, TurnInputIngress,
};
use lash_sim::ProviderWireScript;
use lash_sim::ScriptedLlmHttpTransport;
use lash_sim::runtime_providers::{
    OPENAI_COMPATIBLE, runtime_provider_components, runtime_scripts_for_texts,
};

const PROVIDER_KIND: &str = OPENAI_COMPATIBLE;

fn scripts(n: usize) -> Vec<ProviderWireScript> {
    let texts: Vec<String> = (1..=n).map(|i| format!("answer {i}")).collect();
    runtime_scripts_for_texts(PROVIDER_KIND, &texts).expect("scripts")
}

async fn build_core(
    store_factory: Arc<dyn SessionStoreFactory>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    scripts: Vec<ProviderWireScript>,
) -> (LashCore, Arc<ScriptedLlmHttpTransport>) {
    build_core_with_effect_host(
        store_factory,
        process_env_store,
        attachment_store,
        scripts,
        Arc::new(lash::durability::InlineEffectHost::default()),
    )
    .await
}

async fn build_core_with_effect_host(
    store_factory: Arc<dyn SessionStoreFactory>,
    process_env_store: Arc<dyn ProcessExecutionEnvStore>,
    attachment_store: Arc<dyn AttachmentStore>,
    scripts: Vec<ProviderWireScript>,
    effect_host: Arc<dyn EffectHost>,
) -> (LashCore, Arc<ScriptedLlmHttpTransport>) {
    // UN-GATED transport: deliver each scripted response complete/synchronously.
    let transport = Arc::new(ScriptedLlmHttpTransport::from_scripts(scripts));
    let (provider_handle, model, _kind) =
        runtime_provider_components(PROVIDER_KIND, &transport).expect("provider components");
    let core = LashCore::standard_builder()
        .effect_host(effect_host)
        .attachment_store(attachment_store)
        .process_env_store(process_env_store)
        .store_factory(store_factory)
        .provider(provider_handle)
        .model(model)
        .build()
        .expect("core");
    (core, transport)
}

#[derive(Clone)]
struct YieldBeforeCancelWatchController {
    inner: InlineRuntimeEffectController,
}

#[async_trait]
impl AwaitEventResolver for YieldBeforeCancelWatchController {
    async fn await_event_key(
        &self,
        scope: &ExecutionScope,
        wait: AwaitEventWaitIdentity,
    ) -> Result<AwaitEventKey, RuntimeError> {
        self.inner.await_event_key(scope, wait).await
    }

    async fn resolve_await_event(
        &self,
        key: &AwaitEventKey,
        resolution: Resolution,
    ) -> Result<ResolveOutcome, RuntimeError> {
        self.inner.resolve_await_event(key, resolution).await
    }

    async fn peek_await_event(
        &self,
        key: &AwaitEventKey,
    ) -> Result<Option<Resolution>, RuntimeError> {
        self.inner.peek_await_event(key).await
    }

    async fn await_await_event(
        &self,
        key: &AwaitEventKey,
        cancel: lash::CancellationToken,
        deadline: Option<Instant>,
    ) -> Result<Resolution, RuntimeError> {
        if matches!(key.wait, AwaitEventWaitIdentity::TurnCancelGate) {
            for _ in 0..256 {
                tokio::task::yield_now().await;
            }
        }
        self.inner.await_await_event(key, cancel, deadline).await
    }

    async fn revoke_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.revoke_await_events_for_session(session_id).await
    }

    async fn cancel_await_events_for_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        self.inner.cancel_await_events_for_session(session_id).await
    }
}

#[async_trait]
impl RuntimeEffectController for YieldBeforeCancelWatchController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.inner.execute_effect(envelope, local_executor).await
    }
}

impl EffectHost for YieldBeforeCancelWatchController {
    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        ScopedEffectController::shared(Arc::new(self.clone()), scope)
    }

    fn scoped_static(
        &self,
        scope: ExecutionScope,
    ) -> Result<Option<ScopedEffectController<'static>>, RuntimeError> {
        Ok(Some(ScopedEffectController::shared(
            Arc::new(self.clone()),
            scope,
        )?))
    }
}

async fn build_in_memory(n_scripts: usize) -> (LashCore, Arc<ScriptedLlmHttpTransport>) {
    build_core(
        Arc::new(InMemorySessionStoreFactory::new()),
        Arc::new(InMemoryProcessExecutionEnvStore::new()),
        Arc::new(InMemoryAttachmentStore::new()),
        scripts(n_scripts),
    )
    .await
}

async fn build_sqlite(dir: &Path, n_scripts: usize) -> (LashCore, Arc<ScriptedLlmHttpTransport>) {
    std::fs::create_dir_all(dir).expect("create sqlite dir");
    let process_env_store: Arc<dyn ProcessExecutionEnvStore> = Arc::new(
        lash_sqlite_store::Store::open(&dir.join("process-env.sqlite"))
            .await
            .expect("process env store"),
    );
    build_core(
        Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            dir.to_path_buf(),
        )),
        process_env_store,
        Arc::new(FileAttachmentStore::new(dir.join("attachments"))),
        scripts(n_scripts),
    )
    .await
}

async fn build_sqlite_with_effect_host(
    dir: &Path,
    n_scripts: usize,
    effect_host: Arc<dyn EffectHost>,
) -> (LashCore, Arc<ScriptedLlmHttpTransport>) {
    std::fs::create_dir_all(dir).expect("create sqlite dir");
    let process_env_store: Arc<dyn ProcessExecutionEnvStore> = Arc::new(
        lash_sqlite_store::Store::open(&dir.join("process-env.sqlite"))
            .await
            .expect("process env store"),
    );
    build_core_with_effect_host(
        Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
            dir.to_path_buf(),
        )),
        process_env_store,
        Arc::new(FileAttachmentStore::new(dir.join("attachments"))),
        scripts(n_scripts),
        effect_host,
    )
    .await
}

#[derive(Debug, PartialEq, Eq)]
struct TurnObs {
    label: String,
    assistant_message: Option<String>,
    is_success: bool,
    cumulative_exchanges: usize,
}

/// Drive the trace-faithful sequence: active-turn input targeting the
/// currently-/about-to-run turn 1 is enqueued, CANCELLED, then turn 1 runs
/// (its AfterWork checkpoint sees the already-cancelled row), then subsequent
/// turns. `t1` is both the active_turn_id and turn 1's turn_id (as in the
/// trace, where active_turn_id == the running turn's boundary id).
async fn drive_cancel_before_turn(
    core: &LashCore,
    transport: &Arc<ScriptedLlmHttpTransport>,
    session_id: &str,
) -> (Vec<TurnObs>, String) {
    let session = core
        .session(session_id.to_string())
        .open_fresh()
        .await
        .expect("open fresh");
    let t1 = format!("{session_id}:provider:001");
    let t2 = format!("{session_id}:provider:002");
    let t3 = format!("{session_id}:provider:003");

    let pending = session
        .enqueue(TurnInput::text("queued follow-up"))
        .id("follow-up-001")
        .ingress(TurnInputIngress::active_turn(
            t1.clone(),
            TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await
        .expect("enqueue active-turn input");
    let cancel = session
        .cancel_pending_turn_input(&pending.input_id)
        .await
        .expect("cancel");
    let cancel_outcome = cancel_label(&cancel);

    let mut obs = Vec::new();
    for (label, turn_id) in [("turn-1", &t1), ("turn-2", &t2), ("turn-3", &t3)] {
        obs.push(run_turn(&session, transport, label, turn_id).await);
    }
    (obs, cancel_outcome)
}

/// Task-literal ordering: run turn 1, THEN enqueue + cancel an active-turn
/// input targeting turn 1 (already finished), then subsequent turns.
async fn drive_cancel_after_turn(
    core: &LashCore,
    transport: &Arc<ScriptedLlmHttpTransport>,
    session_id: &str,
) -> (Vec<TurnObs>, String) {
    let session = core
        .session(session_id.to_string())
        .open_fresh()
        .await
        .expect("open fresh");
    let t1 = format!("{session_id}:provider:001");
    let t2 = format!("{session_id}:provider:002");
    let t3 = format!("{session_id}:provider:003");

    let mut obs = Vec::new();
    obs.push(run_turn(&session, transport, "turn-1", &t1).await);

    let pending = session
        .enqueue(TurnInput::text("queued follow-up"))
        .id("follow-up-001")
        .ingress(TurnInputIngress::active_turn(
            t1.clone(),
            TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await
        .expect("enqueue active-turn input");
    let cancel = session
        .cancel_pending_turn_input(&pending.input_id)
        .await
        .expect("cancel");
    let cancel_outcome = cancel_label(&cancel);

    obs.push(run_turn(&session, transport, "turn-2", &t2).await);
    obs.push(run_turn(&session, transport, "turn-3", &t3).await);
    (obs, cancel_outcome)
}

/// Control: enqueue an active-turn input targeting turn 1 and DO NOT cancel,
/// then run turn 1 (whose AfterWork checkpoint should claim and inject it,
/// driving an extra exchange), then turn 2.
async fn drive_no_cancel_control(
    core: &LashCore,
    transport: &Arc<ScriptedLlmHttpTransport>,
    session_id: &str,
) -> Vec<TurnObs> {
    let session = core
        .session(session_id.to_string())
        .open_fresh()
        .await
        .expect("open fresh");
    let t1 = format!("{session_id}:provider:001");
    let t2 = format!("{session_id}:provider:002");

    session
        .enqueue(TurnInput::text("queued follow-up"))
        .id("follow-up-001")
        .ingress(TurnInputIngress::active_turn(
            t1.clone(),
            TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await
        .expect("enqueue active-turn input");

    let mut obs = Vec::new();
    obs.push(run_turn(&session, transport, "turn-1", &t1).await);
    obs.push(run_turn(&session, transport, "turn-2", &t2).await);
    obs
}

/// Claim-then-cancel: enqueue active-turn input targeting turn 1, run turn 1
/// (its AfterWork checkpoint CLAIMS and completes the input -> extra exchange),
/// THEN cancel (must observe the post-claim terminal state), then run turn 2.
async fn drive_claim_then_cancel(
    core: &LashCore,
    transport: &Arc<ScriptedLlmHttpTransport>,
    session_id: &str,
) -> (Vec<TurnObs>, String) {
    let session = core
        .session(session_id.to_string())
        .open_fresh()
        .await
        .expect("open fresh");
    let t1 = format!("{session_id}:provider:001");
    let t2 = format!("{session_id}:provider:002");

    let pending = session
        .enqueue(TurnInput::text("queued follow-up"))
        .id("follow-up-001")
        .ingress(TurnInputIngress::active_turn(
            t1.clone(),
            TurnInputCheckpointBoundary::AfterWork,
        ))
        .send()
        .await
        .expect("enqueue active-turn input");

    let mut obs = Vec::new();
    obs.push(run_turn(&session, transport, "turn-1", &t1).await);
    let cancel = session
        .cancel_pending_turn_input(&pending.input_id)
        .await
        .expect("cancel");
    let cancel_outcome = cancel_label(&cancel);
    obs.push(run_turn(&session, transport, "turn-2", &t2).await);
    (obs, cancel_outcome)
}

fn cancel_label(outcome: &PendingTurnInputCancelOutcome) -> String {
    match outcome {
        PendingTurnInputCancelOutcome::Cancelled(_) => "cancelled",
        PendingTurnInputCancelOutcome::AlreadyClaimed { .. } => "already_claimed",
        PendingTurnInputCancelOutcome::AlreadyCompleted(_) => "already_completed",
        PendingTurnInputCancelOutcome::AlreadyCancelled(_) => "already_cancelled",
        PendingTurnInputCancelOutcome::NotFound => "not_found",
    }
    .to_string()
}

async fn run_turn(
    session: &lash::LashSession,
    transport: &Arc<ScriptedLlmHttpTransport>,
    label: &str,
    turn_id: &str,
) -> TurnObs {
    let result = session
        .turn(TurnInput::text(format!("user prompt for {label}")))
        .turn_id(turn_id.to_string())
        .run()
        .await;
    let cumulative_exchanges = transport.exchanges().map(|e| e.len()).unwrap_or(usize::MAX);
    match result {
        Ok(output) => TurnObs {
            label: label.to_string(),
            assistant_message: output.assistant_message().map(str::to_string),
            is_success: output.is_success(),
            cumulative_exchanges,
        },
        Err(err) => TurnObs {
            label: format!("{label} (ERR: {err})"),
            assistant_message: None,
            is_success: false,
            cumulative_exchanges,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_backend_active_turn_cancel_then_turn_agrees() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Variant B: cancel BEFORE turn 1 runs (closest to the recorded trace
    // timing: cancel precedes turn 1's AfterWork checkpoint).
    let (mem_core, mem_tx) = build_in_memory(8).await;
    let (b_mem, b_mem_cancel) = drive_cancel_before_turn(&mem_core, &mem_tx, "mem-B").await;
    let (sq_core, sq_tx) = build_sqlite(&tmp.path().join("sqlite-B"), 8).await;
    let (b_sq, b_sq_cancel) = drive_cancel_before_turn(&sq_core, &sq_tx, "sql-B").await;

    // Variant A: cancel AFTER turn 1 (task-literal ordering).
    let (mem_core_a, mem_tx_a) = build_in_memory(8).await;
    let (a_mem, a_mem_cancel) = drive_cancel_after_turn(&mem_core_a, &mem_tx_a, "mem-A").await;
    let (sq_core_a, sq_tx_a) = build_sqlite(&tmp.path().join("sqlite-A"), 8).await;
    let (a_sq, a_sq_cancel) = drive_cancel_after_turn(&sq_core_a, &sq_tx_a, "sql-A").await;

    // Variant C: no cancel control (active-turn input should be claimed).
    let (mem_core_c, mem_tx_c) = build_in_memory(8).await;
    let c_mem = drive_no_cancel_control(&mem_core_c, &mem_tx_c, "mem-C").await;
    let (sq_core_c, sq_tx_c) = build_sqlite(&tmp.path().join("sqlite-C"), 8).await;
    let c_sq = drive_no_cancel_control(&sq_core_c, &sq_tx_c, "sql-C").await;

    // Normalize session-id-derived labels so the per-backend Vecs are
    // comparable (labels are "turn-1/2/3" already, independent of session id).
    println!("=== Variant B (cancel BEFORE turn 1) ===");
    println!("  in-memory cancel: {b_mem_cancel}");
    println!("  sqlite    cancel: {b_sq_cancel}");
    println!("  in-memory turns:  {b_mem:#?}");
    println!("  sqlite    turns:  {b_sq:#?}");

    println!("=== Variant A (cancel AFTER turn 1) ===");
    println!("  in-memory cancel: {a_mem_cancel}");
    println!("  sqlite    cancel: {a_sq_cancel}");
    println!("  in-memory turns:  {a_mem:#?}");
    println!("  sqlite    turns:  {a_sq:#?}");

    println!("=== Variant C (no cancel control) ===");
    println!("  in-memory turns:  {c_mem:#?}");
    println!("  sqlite    turns:  {c_sq:#?}");

    // Variant D: claim-then-cancel (cancel observes post-claim terminal state).
    let (mem_core_d, mem_tx_d) = build_in_memory(8).await;
    let (d_mem, d_mem_cancel) = drive_claim_then_cancel(&mem_core_d, &mem_tx_d, "mem-D").await;
    let (sq_core_d, sq_tx_d) = build_sqlite(&tmp.path().join("sqlite-D"), 8).await;
    let (d_sq, d_sq_cancel) = drive_claim_then_cancel(&sq_core_d, &sq_tx_d, "sql-D").await;
    println!("=== Variant D (claim then cancel) ===");
    println!("  in-memory cancel: {d_mem_cancel}");
    println!("  sqlite    cancel: {d_sq_cancel}");
    println!("  in-memory turns:  {d_mem:#?}");
    println!("  sqlite    turns:  {d_sq:#?}");

    assert_eq!(
        b_mem_cancel, b_sq_cancel,
        "variant B cancel outcome diverged"
    );
    assert_eq!(b_mem, b_sq, "variant B per-turn output diverged");
    assert_eq!(
        a_mem_cancel, a_sq_cancel,
        "variant A cancel outcome diverged"
    );
    assert_eq!(a_mem, a_sq, "variant A per-turn output diverged");
    assert_eq!(c_mem, c_sq, "variant C (control) per-turn output diverged");
    assert_eq!(
        d_mem_cancel, d_sq_cancel,
        "variant D cancel outcome diverged"
    );
    assert_eq!(d_mem, d_sq, "variant D per-turn output diverged");
}

#[derive(Debug, PartialEq, Eq)]
struct FirstPartyCancelObs {
    cancelled: bool,
    request_id: Option<String>,
    origin: Option<String>,
    duplicate_preserved_original: bool,
    next_turn_succeeded: bool,
}

async fn drive_first_party_cancel_before_start(
    core: &LashCore,
    session_id: &str,
) -> FirstPartyCancelObs {
    let turn_id = "cancelled-turn";
    let address = lash::TurnAddress::new(session_id, turn_id);
    let driver = core.turn_work_driver();
    let first = driver
        .request_cancel(lash::TurnCancelRequest::new(
            address.clone(),
            "cross-backend-cancel",
            Some("sim-user".to_string()),
        ))
        .await
        .expect("cancel before start");
    assert!(matches!(
        first.outcome,
        lash::TurnCancelOutcome::Requested(_)
    ));

    let duplicate = driver
        .request_cancel(lash::TurnCancelRequest::new(
            address,
            "cross-backend-duplicate",
            Some("sim-duplicate".to_string()),
        ))
        .await
        .expect("duplicate cancel");
    let duplicate_preserved_original = matches!(
        duplicate.outcome,
        lash::TurnCancelOutcome::AlreadyRequested(lash::TurnCancellationEvidence {
            ref request_id,
            origin: Some(ref origin),
            ..
        }) if request_id == "cross-backend-cancel" && origin == "sim-user"
    );

    let session = core
        .session(session_id)
        .open_fresh()
        .await
        .expect("open session");
    let cancelled = session
        .turn(TurnInput::text("this turn is already cancelled"))
        .turn_id(turn_id)
        .run()
        .await
        .expect("cancelled turn commits");
    let next = session
        .turn(TurnInput::text("future turn remains isolated"))
        .turn_id("future-turn")
        .run()
        .await
        .expect("future turn");

    let cancellation = cancelled.result.cancellation;
    FirstPartyCancelObs {
        cancelled: matches!(
            cancelled.result.outcome,
            lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled)
        ),
        request_id: cancellation
            .as_ref()
            .map(|evidence| evidence.request_id.clone()),
        origin: cancellation.and_then(|evidence| evidence.origin),
        duplicate_preserved_original,
        next_turn_succeeded: next.result.is_success(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_backend_first_party_turn_cancel_agrees() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (mem_core, _) = build_in_memory(2).await;
    let memory = drive_first_party_cancel_before_start(&mem_core, "turn-cancel-memory").await;
    let (sqlite_core, _) = build_sqlite(&tmp.path().join("turn-cancel-sqlite"), 2).await;
    let sqlite = drive_first_party_cancel_before_start(&sqlite_core, "turn-cancel-sqlite").await;

    assert_eq!(memory, sqlite, "first-party turn cancellation diverged");
    assert_eq!(memory.request_id.as_deref(), Some("cross-backend-cancel"));
    assert_eq!(memory.origin.as_deref(), Some("sim-user"));
    assert!(memory.cancelled);
    assert!(memory.duplicate_preserved_original);
    assert!(memory.next_turn_succeeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_reopen_preserves_cancelled_turn_commit_and_allows_next_turn() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sqlite_dir = tmp.path().join("turn-cancel-reopen");
    let session_id = "turn-cancel-sqlite-reopen";
    let turn_id = "cancelled-before-reopen";

    let (first_core, first_transport) = build_sqlite_with_effect_host(
        &sqlite_dir,
        1,
        Arc::new(YieldBeforeCancelWatchController {
            inner: InlineRuntimeEffectController::default(),
        }),
    )
    .await;
    let first_driver = first_core.turn_work_driver();
    let outcome = first_driver
        .request_cancel(lash::TurnCancelRequest::new(
            lash::TurnAddress::new(session_id, turn_id),
            "sqlite-replay-cancel",
            Some("sqlite-replay".to_string()),
        ))
        .await
        .expect("request cancellation before SQLite turn");
    assert!(matches!(
        outcome.outcome,
        lash::TurnCancelOutcome::Requested(_)
    ));
    let first_session = first_core
        .session(session_id)
        .open_fresh()
        .await
        .expect("open first SQLite session");
    let cancelled = first_session
        .turn(TurnInput::text("cancel before provider work"))
        .turn_id(turn_id)
        .run()
        .await
        .expect("commit cancelled SQLite turn");
    assert!(matches!(
        cancelled.result.outcome,
        lash::TurnOutcome::Stopped(lash::TurnStop::Cancelled)
    ));
    assert_eq!(
        cancelled
            .result
            .cancellation
            .as_ref()
            .map(|evidence| evidence.request_id.as_str()),
        Some("sqlite-replay-cancel")
    );
    assert_eq!(
        cancelled
            .result
            .cancellation
            .as_ref()
            .and_then(|evidence| evidence.origin.as_deref()),
        Some("sqlite-replay")
    );
    assert_eq!(
        first_transport.exchanges().expect("first exchanges").len(),
        0,
        "cancel-before-start reached the provider"
    );
    let committed_turn_index = first_session
        .observe()
        .current_observation()
        .read_view
        .turn_index();
    drop(first_session);
    drop(first_core);

    let (reopened_core, reopened_transport) = build_sqlite(&sqlite_dir, 1).await;
    let reopened = reopened_core
        .session(session_id)
        .open()
        .await
        .expect("reopen SQLite session after cancellation");
    assert_eq!(
        reopened
            .observe()
            .current_observation()
            .read_view
            .turn_index(),
        committed_turn_index,
        "reopen lost the cancelled turn's committed session revision"
    );
    let next = reopened
        .turn(TurnInput::text("work after replayed cancellation"))
        .turn_id("turn-after-reopen")
        .run()
        .await
        .expect("run future turn after SQLite reopen");
    assert!(next.result.is_success());
    assert_eq!(
        next.result.assistant_message(),
        Some("answer 1"),
        "future turn did not consume the first provider response"
    );
    assert_eq!(
        reopened_transport
            .exchanges()
            .expect("reopened exchanges")
            .len(),
        1
    );
}
