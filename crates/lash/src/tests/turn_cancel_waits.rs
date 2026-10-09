//! The two turn-cancel modes over a turn that waits (FIG-635): a code
//! cell's durable sleep. An `AfterStep` request lets the wait finish on its
//! own terms and stops the turn at the step boundary after it; only an
//! `Immediate` request, fresh or escalating a deferred stop, cuts the wait.

use super::*;
use crate::{TurnCancelMode, TurnInput};
use lash_core::facade_support::TurnCancelOutcome;
use std::time::{Duration, Instant};

/// How long the cell sleeps when its timer is let finish: long enough for
/// a cancel to land inside it.
const SLEEP_MS: u64 = 2_000;
/// How long the cell sleeps when a stop cuts its timer: far past everything
/// the turn does around the sleep (lowering the cell in a worker, restoring
/// the turn to end it), so a turn that ends before it was cut, on any host.
const CUT_SLEEP_MS: u64 = 20_000;
/// When, after the cell was sent, a stop is requested: inside the sleep.
const STOP_AFTER: Duration = Duration::from_millis(500);
/// How long a cancelled turn may take to end.
const ENDS_WITHIN: Duration = Duration::from_secs(30);

/// An RLM core whose model answers the first call with a cell that sleeps
/// `sleep_ms` and then prints, and every later call with text; `started`
/// opens at the first call, and `calls` counts them.
async fn sleeping_cell_session(
    id: &str,
    sleep_ms: u64,
    calls: &Arc<AtomicUsize>,
    started: &Arc<tokio::sync::Notify>,
) -> Result<(LashCore, crate::DurableSession)> {
    let backend = sqlite_memory_store_backend().await;
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        lash_protocol_rlm::CellDialect::typescript(),
    )
    .with_worker_service(untimed_fixture_workers());
    let (calls, started) = (Arc::clone(calls), Arc::clone(started));
    let provider = crate::testing::TestProvider::builder()
        .kind("cancel-waits")
        .complete(move |_request| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                started.notify_one();
            }
            async move {
                Ok(text_response(&if call == 0 {
                    typescript_block(&format!("await sleep({sleep_ms});\nconsole.log(\"woke\");"))
                } else {
                    "after the sleep".to_string()
                }))
            }
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(backend, factory))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(
            crate::plugins::SessionToolAccess::ambient(),
            mock_session_spec(),
        ))
        .await?;
    Ok((core, session))
}

/// The open run's cancel outcome a receipt answers.
fn outcome(receipt: crate::CancelReceipt) -> TurnCancelOutcome {
    match receipt {
        crate::CancelReceipt::Cancelled { receipt, .. } => receipt.outcome,
        other => panic!("the cancel addressed the open run: {other:?}"),
    }
}

/// `handle`'s ended turn: its cancellation evidence and when it ended.
async fn ended(
    handle: crate::SendHandle,
) -> Result<(lash_core::facade_support::TurnCancellationEvidence, Instant)> {
    let output = tokio::time::timeout(ENDS_WITHIN, handle.output())
        .await
        .expect("the cancelled turn ends")?;
    let at = Instant::now();
    let evidence = output
        .result
        .cancellation()
        .cloned()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.result.outcome));
    Ok((evidence, at))
}

/// An after-step stop requested while a cell sleeps lets the timer finish:
/// the turn ends no sooner than the sleep's due time, with after-step
/// evidence, and no model call starts after the step.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_step_during_a_cell_sleep_lets_the_timer_finish() -> Result<()> {
    let (calls, started) = (Arc::default(), Arc::default());
    let (core, session) =
        sleeping_cell_session("after-step-sleep", SLEEP_MS, &calls, &started).await?;
    let sent = Instant::now();
    let handle = session.send(TurnInput::text("sleep")).await?;
    started.notified().await;
    tokio::time::sleep(STOP_AFTER).await;
    let requested = outcome(
        handle
            .cancel()
            .request_id("stop-in-sleep")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    let (evidence, at) = ended(handle).await?;
    assert_eq!(evidence.request_id, "stop-in-sleep");
    assert_eq!(evidence.mode, TurnCancelMode::AfterStep);
    assert!(
        at.duration_since(sent) >= Duration::from_millis(SLEEP_MS),
        "an after-step stop does not wake the timer early: ended after {:?}",
        at.duration_since(sent)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "no model call after the step"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// An immediate stop requested while a cell sleeps cuts the sleep: the turn
/// ends with immediate evidence well before the sleep's due time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn immediate_during_a_cell_sleep_aborts_it() -> Result<()> {
    let (calls, started) = (Arc::default(), Arc::default());
    let (core, session) =
        sleeping_cell_session("immediate-sleep", CUT_SLEEP_MS, &calls, &started).await?;
    let sent = Instant::now();
    let handle = session.send(TurnInput::text("sleep")).await?;
    started.notified().await;
    tokio::time::sleep(STOP_AFTER).await;
    let requested = outcome(
        handle
            .cancel()
            .request_id("abort-in-sleep")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    let (evidence, at) = ended(handle).await?;
    assert_eq!(evidence.request_id, "abort-in-sleep");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert!(
        at.duration_since(sent) < Duration::from_millis(CUT_SLEEP_MS),
        "an immediate stop cuts the sleep: ended after {:?}",
        at.duration_since(sent)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// An immediate request escalating an after-step stop while a cell sleeps
/// cuts the sleep the deferred stop let run: the turn ends with the
/// escalating request's evidence before the sleep's due time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn escalating_a_deferred_stop_aborts_the_cell_sleep() -> Result<()> {
    let (calls, started) = (Arc::default(), Arc::default());
    let (core, session) =
        sleeping_cell_session("escalated-sleep", CUT_SLEEP_MS, &calls, &started).await?;
    let sent = Instant::now();
    let handle = session.send(TurnInput::text("sleep")).await?;
    let run = handle.id().cloned().expect("the send names its run");
    started.notified().await;
    tokio::time::sleep(STOP_AFTER).await;
    let first = outcome(
        handle
            .cancel()
            .request_id("stop-first")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(first, TurnCancelOutcome::Requested(_)),
        "{first:?}"
    );
    tokio::time::sleep(STOP_AFTER).await;
    let escalated = outcome(
        session
            .cancel(crate::CancelTarget::Run(run))
            .request_id("abort-second")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(&escalated, TurnCancelOutcome::Escalated(evidence)
            if evidence.request_id == "abort-second"),
        "{escalated:?}"
    );
    let (evidence, at) = ended(handle).await?;
    assert_eq!(evidence.request_id, "abort-second");
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert!(
        at.duration_since(sent) < Duration::from_millis(CUT_SLEEP_MS),
        "the escalation cuts the sleep: ended after {:?}",
        at.duration_since(sent)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(session);
    core.shutdown().await?;
    Ok(())
}
