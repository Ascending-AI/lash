//! A plugin fault in a turn's after-turn finalize, on the Restate engine
//! (FIG-3897).
//!
//! The fault settles by its cause (FIG-3575). A live fault — an opaque
//! plugin-session failure, a store blip behind a plugin service — records
//! nothing, so the turn handler fails its attempt retryably and the engine
//! retries it: the retry replays the journaled model call and commits the
//! root once. A refusal over the turn is an outcome: the root ends terminal
//! with its typed failure after one attempt, and a redrive of the same turn
//! id answers that failure without running the turn again. A failure the
//! journal already holds is an outcome on every attempt, whatever its code.
//!
//! Every law runs on lash-restate's engine over the Restate server double.

use super::*;

const SEED: u64 = 0x3897_f1a1;

/// A core over the double's engine, and the double, which must outlive it:
/// a core built over `double.lash_backend()` does not hold it (FIG-3723).
struct Fixture {
    core: LashCore,
    double: lash_restate_test::RestateTestBackend,
    provider_calls: Arc<AtomicUsize>,
}

impl Fixture {
    async fn with_plugins(plugins: Vec<Arc<dyn PluginFactory>>) -> Result<Self> {
        let double = restate_double(SEED).await;
        let provider_calls = Arc::new(AtomicUsize::new(0));
        let builder =
            LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded);
        let builder = plugins
            .into_iter()
            .fold(builder, |builder, plugin| builder.plugin(plugin));
        let core = explicit_ephemeral_facets(builder)
            .provider(counting_provider(Arc::clone(&provider_calls)))
            .model(mock_model_spec())
            .build(crate::testing::runtime_lease_owner())?;
        Ok(Self {
            core,
            double,
            provider_calls,
        })
    }

    /// The attempts the engine made of `turn`'s `LashTurn` run.
    fn turn_attempts(&self, session: &str, turn: &str) -> u32 {
        let key = lash_restate::turn_workflow_key(
            &lash_core::SessionId::from(session),
            &lash_core::TurnId::from(turn),
        );
        self.double
            .server()
            .invocations()
            .into_iter()
            .filter(|view| {
                view.target.starts_with("LashTurn") && view.target.ends_with(&format!("/{key}/run"))
            })
            .map(|view| view.attempts)
            .sum()
    }
}

fn counting_provider(calls: Arc<AtomicUsize>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("finalize-fault")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

fn plugin(id: &'static str, spec: lash_core::facade_support::PluginSpec) -> Arc<dyn PluginFactory> {
    Arc::new(StaticPluginFactory::new(id, spec))
}

/// An `after_turn` hook that fails with `fault()` the first `failures` times
/// it runs, and counts every run in `calls`.
fn failing_after_turn(
    calls: Arc<AtomicUsize>,
    failures: usize,
    fault: fn() -> lash_core::PluginError,
) -> Arc<dyn PluginFactory> {
    plugin(
        "finalize-fault-after-turn",
        lash_core::facade_support::PluginSpec::new().with_after_turn(Arc::new(move |_| {
            let calls = Arc::clone(&calls);
            Box::pin(async move {
                if calls.fetch_add(1, Ordering::SeqCst) < failures {
                    return Err(fault());
                }
                Ok(Vec::new())
            })
        })),
    )
}

/// An opaque plugin-session failure: a store blip behind a plugin service.
fn session_blip() -> lash_core::PluginError {
    lash_core::PluginError::Session("plugin session store unavailable".to_string())
}

/// A live fault under a plugin-minted code: the plugin classes it
/// [`TurnFailureCause::LiveFault`](lash_core::TurnFailureCause::LiveFault).
fn minted_live_fault() -> lash_core::PluginError {
    lash_core::PluginError::Runtime(lash_core::RuntimeError::foreign(
        "finalize-plugin.store_blip",
        lash_core::TurnFailureCause::LiveFault,
        "plugin session store unavailable",
    ))
}

/// A plugin's refusal over the turn it finalizes.
fn finalize_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Invoke("the finalize hook refuses this turn".to_string())
}

/// A refusal under a plugin-minted code: the plugin classes it
/// [`TurnFailureCause::Outcome`](lash_core::TurnFailureCause::Outcome).
fn minted_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Runtime(lash_core::RuntimeError::foreign(
        "finalize-plugin.refused",
        lash_core::TurnFailureCause::Outcome,
        "the finalize hook refuses this turn",
    ))
}

/// F1: a live fault in the finalize hook fails the turn handler's attempt
/// retryably. The engine retries it, the retry replays the journaled model
/// call and runs the hook again, and the root commits exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_finalize_fault_is_retried_by_the_engine_to_one_committed_root() -> Result<()> {
    retried_to_one_committed_root("finalize-live-fault", session_blip).await
}

/// F1 for a fault the plugin classes itself: a plugin-minted code it marks
/// a live fault is retried the same way.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_minted_live_finalize_fault_is_retried_by_the_engine_to_one_committed_root() -> Result<()>
{
    retried_to_one_committed_root("finalize-minted-live-fault", minted_live_fault).await
}

async fn retried_to_one_committed_root(
    session_id: &'static str,
    fault: fn() -> lash_core::PluginError,
) -> Result<()> {
    const TURN: &str = "finalize-blip";
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::with_plugins(vec![failing_after_turn(
        Arc::clone(&finalize_calls),
        1,
        fault,
    )])
    .await?;
    let session = fixture
        .core
        .session(session_id)
        .created()
        .await
        .open()
        .await?;

    let output = session
        .send(TurnInput::text("finalize blips once"))
        .id(TURN)
        .output()
        .await
        .expect("the engine retries the live finalize fault to completion");

    assert!(output.is_success(), "{:?}", output.result.outcome);
    assert_eq!(
        finalize_calls.load(Ordering::SeqCst),
        2,
        "the hook failed once and ran again on the retry"
    );
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "the retry replays the journaled model call"
    );
    assert_eq!(
        fixture.turn_attempts(session_id, TURN),
        2,
        "the live fault ended exactly one attempt of the turn's run"
    );
    let applications = session.durable().turn_input_applications().await?;
    assert_eq!(
        applications.len(),
        1,
        "exactly one root committed: {applications:?}"
    );
    assert!(session.durable().pending_turn_inputs().await?.is_empty());

    let settled = session
        .send(TurnInput::text("finalize blips once"))
        .id(TURN)
        .await?
        .outcome()
        .await?;
    assert_eq!(settled.status, crate::TurnStatus::Answered);
    assert_eq!(finalize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    Ok(())
}

/// A live fault that recurs on every attempt spends the turn handler's
/// retry policy: after `TURN_HANDLER_MAX_ATTEMPTS` the root's run pauses with
/// the fault as its last failure, and the root is stalled. No attempt
/// committed it or recorded a failure, and every retry replayed the journaled
/// model call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finalize_fault_on_every_attempt_pauses_the_root_run() -> Result<()> {
    const SESSION: &str = "finalize-fault-pauses";
    const TURN: &str = "finalize-always-fails";
    let attempts = u32::try_from(lash_restate::TURN_HANDLER_MAX_ATTEMPTS)
        .expect("the attempt budget fits the double's counter");
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::with_plugins(vec![failing_after_turn(
        Arc::clone(&finalize_calls),
        usize::MAX,
        session_blip,
    )])
    .await?;
    let session = fixture.core.session(SESSION).created().await.open().await?;
    let key = lash_restate::turn_workflow_key(
        &lash_core::SessionId::from(SESSION),
        &lash_core::TurnId::from(TURN),
    );

    let _handle = session
        .send(TurnInput::text("finalize always fails"))
        .id(TURN)
        .await?;
    let paused = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            if let Some(run) = fixture
                .double
                .server()
                .invocations()
                .into_iter()
                .find(|view| {
                    view.target.starts_with("LashTurn")
                        && view.target.ends_with(&format!("/{key}/run"))
                        && view.status == "paused"
                })
            {
                return run;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the root's run pauses once its attempts are spent");

    assert_eq!(paused.attempts, attempts);
    let (_, last_failure) = paused
        .last_failure
        .expect("the pause keeps its last failure");
    assert!(
        last_failure.contains("plugin session store unavailable"),
        "the pause names the finalize fault: {last_failure}"
    );
    assert_eq!(finalize_calls.load(Ordering::SeqCst), attempts as usize);
    assert_eq!(
        fixture.provider_calls.load(Ordering::SeqCst),
        1,
        "every retry replayed the journaled model call"
    );
    assert!(
        session
            .durable()
            .turn_input_applications()
            .await?
            .is_empty(),
        "no attempt committed the root"
    );
    Ok(())
}

/// A refusal in the finalize hook is an outcome: the root ends terminal
/// after one attempt with the typed finalize failure, and a redrive of the
/// same turn id answers that failure without running the turn again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finalize_refusal_ends_the_root_terminal_with_its_typed_failure() -> Result<()> {
    refused_terminal("finalize-refusal", finalize_refusal).await
}

/// The same for a refusal the plugin classes itself: a plugin-minted code it
/// marks an outcome ends the root terminal, never retried.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_minted_finalize_refusal_ends_the_root_terminal_with_its_typed_failure() -> Result<()> {
    refused_terminal("finalize-minted-refusal", minted_refusal).await
}

async fn refused_terminal(
    session_id: &'static str,
    fault: fn() -> lash_core::PluginError,
) -> Result<()> {
    const TURN: &str = "finalize-refused";
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::with_plugins(vec![failing_after_turn(
        Arc::clone(&finalize_calls),
        usize::MAX,
        fault,
    )])
    .await?;
    let session = fixture
        .core
        .session(session_id)
        .created()
        .await
        .open()
        .await?;

    let refused = session
        .send(TurnInput::text("finalize refuses"))
        .id(TURN)
        .output()
        .await
        .expect_err("a finalize refusal ends the root terminal");
    assert_finalize_refusal(&refused);
    assert_eq!(finalize_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture.turn_attempts(session_id, TURN),
        1,
        "a refusal is never retried"
    );

    let redriven = session
        .send(TurnInput::text("finalize refuses"))
        .id(TURN)
        .output()
        .await
        .expect_err("the redrive answers the recorded refusal");
    assert_finalize_refusal(&redriven);
    assert_eq!(
        finalize_calls.load(Ordering::SeqCst),
        1,
        "the redrive never runs the turn again"
    );
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.turn_attempts(session_id, TURN), 1);
    Ok(())
}

fn assert_finalize_refusal(error: &EmbedError) {
    let EmbedError::Runtime(runtime) = error else {
        panic!("the refusal is the typed runtime error: {error:?}");
    };
    assert_eq!(
        runtime.code,
        lash_core::RuntimeErrorCode::PluginFinalizeTurn,
        "the refusal carries the finalize failure: {runtime:?}"
    );
    assert!(
        runtime
            .message
            .contains("the finalize hook refuses this turn"),
        "{runtime:?}"
    );
}

/// F4: a failure the journal already holds is an outcome on every attempt,
/// whatever its code. The checkpoint hook fails with a live-coded
/// plugin-session fault, which the checkpoint's journaled outcome records;
/// the first attempt is then interrupted by a live finalize fault. The
/// engine's retry replays the recorded checkpoint failure and settles it as
/// the root's failed turn, and a redrive of the same turn id answers it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journaled_failure_settles_the_root_failed_after_a_live_finalize_fault() -> Result<()> {
    const SESSION: &str = "finalize-journaled-failure";
    const TURN: &str = "journaled-failure";
    let checkpoint_calls = Arc::new(AtomicUsize::new(0));
    let finalize_calls = Arc::new(AtomicUsize::new(0));
    let failing_checkpoint = plugin(
        "finalize-fault-checkpoint",
        lash_core::facade_support::PluginSpec::new().with_checkpoint(Arc::new({
            let checkpoint_calls = Arc::clone(&checkpoint_calls);
            move |_| {
                let checkpoint_calls = Arc::clone(&checkpoint_calls);
                Box::pin(async move {
                    checkpoint_calls.fetch_add(1, Ordering::SeqCst);
                    Err(lash_core::PluginError::Session(
                        "checkpoint store unavailable".to_string(),
                    ))
                })
            }
        })),
    );
    let fixture = Fixture::with_plugins(vec![
        failing_checkpoint,
        failing_after_turn(Arc::clone(&finalize_calls), 1, session_blip),
    ])
    .await?;
    let session = fixture.core.session(SESSION).created().await.open().await?;

    let settled = session
        .send(TurnInput::text("checkpoint fails and is journaled"))
        .id(TURN)
        .await?
        .outcome()
        .await?;
    assert_journaled_failure(&settled, true);
    let recorded_checkpoints = checkpoint_calls.load(Ordering::SeqCst);
    assert!(recorded_checkpoints > 0, "the checkpoint ran and failed");
    assert_eq!(
        finalize_calls.load(Ordering::SeqCst),
        2,
        "the live finalize fault ended the first attempt and the retry finalized"
    );
    assert_eq!(fixture.turn_attempts(SESSION, TURN), 2);

    let redriven = session
        .send(TurnInput::text("checkpoint fails and is journaled"))
        .id(TURN)
        .await?
        .outcome()
        .await?;
    assert_journaled_failure(&redriven, false);
    assert_eq!(
        redriven.root, settled.root,
        "the redrive answers the same root"
    );
    assert_eq!(
        checkpoint_calls.load(Ordering::SeqCst),
        recorded_checkpoints,
        "neither the retry nor the redrive re-ran the recorded checkpoint"
    );
    assert_eq!(finalize_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.provider_calls.load(Ordering::SeqCst), 1);
    assert!(session.durable().pending_turn_inputs().await?.is_empty());
    Ok(())
}

/// The root settled failed on the recorded checkpoint failure. The live
/// report names it; a report rebuilt from the store for a redrive is thin
/// (D1 §1.5 3b) and carries the typed stop alone.
#[track_caller]
fn assert_journaled_failure(settled: &crate::SendOutcome, live: bool) {
    assert_eq!(
        settled.status,
        crate::TurnStatus::Failed,
        "the journaled checkpoint failure settles the root failed: {settled:?}"
    );
    let report = &settled
        .output
        .as_ref()
        .expect("a failed root carries its settled turn")
        .result;
    assert!(
        matches!(
            report.outcome,
            TurnOutcome::Stopped(lash_core::facade_support::TurnStop::RuntimeError)
        ),
        "{:?}",
        report.outcome
    );
    if live {
        assert!(
            report.errors.iter().any(|issue| {
                issue.kind == lash_core::TurnFailureKind::RuntimeEffectController
                    && issue.message.contains("checkpoint store unavailable")
            }),
            "the failed turn names the recorded checkpoint failure: {:?}",
            report.errors
        );
    }
}
