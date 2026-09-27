//! A cancelled turn never completes work it withheld from its terminal
//! checkpoint (FIG-3531, FIG-3543).
//!
//! Input claimed at `BeforeCompletion` is withheld from that checkpoint's
//! delivery for a follow-on turn (FIG-3157). An Immediate cancel means that
//! follow-on never runs and the model never saw the input, so the cancelled
//! turn settles it through the cancellation's undelivered disposition exactly
//! as it settles an unclaimed active-turn row: deferred by default, dropped
//! when the host asks, with the same `TurnCancelAffectedInput` record in
//! enqueue order, and — when deferred — delivered once by the next turn.
//!
//! A process wake withheld the same way is never completed and never dropped
//! by the cancel: it is deferred whatever the disposition, recorded as a
//! `TurnCancelAffectedWake`, and delivered once by the next turn (FIG-3543,
//! ADR 0101 §10).
//!
//! The law drives a real runtime turn over the supplied durable store and
//! reads the outcome back only through surfaces every backend already owes.

use super::direct_turn_acceptance::{acceptance_runtime_for_session, direct_input, text_response};
use crate::admit;
use lash_core::testing::conformance_support::ActiveTurnControl;
use lash_sansio::SessionId;
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::Mutex;
use tokio_util::sync::CancellationToken;

/// The session every store in this suite is exercised under. Not `root`:
/// commit admission is process-wide and keyed by session, and a cancelled
/// turn's commit does not wait behind other laws' commits in the same binary.
pub const CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID: &str = "cancelled-turn-withheld-input";
const SESSION_ID: &str = CANCELLED_TURN_WITHHELD_INPUT_SESSION_ID;

/// How the Stop reaches the running turn.
enum Stop {
    /// The caller's own cancellation token: Immediate, `Defer`.
    Local(CancellationToken),
    /// A durable Immediate request naming the undelivered disposition.
    Durable(Box<crate::TurnCancelRequest>),
}

/// Stops the running turn the moment its terminal checkpoint has claimed
/// active-turn input: the claim is taken and withheld, and the Stop lands
/// before the turn commits.
struct StopAfterTerminalClaim {
    inner: Arc<dyn crate::RuntimePersistence>,
    effect_host: Arc<dyn crate::EffectHost>,
    armed: Mutex<Option<Stop>>,
    withheld_inputs: Mutex<Vec<crate::InputId>>,
    withheld_batches: Mutex<Vec<crate::BatchId>>,
    /// A wake to accept just before the running turn's terminal checkpoint
    /// claims: accepted after the turn's input, so the turn lane admits the
    /// input first (ADR 0101 §5) and the wake reaches the turn only there.
    arriving_wake: Mutex<Option<crate::QueuedWorkBatchDraft>>,
    arrived_wake: Mutex<Option<crate::QueuedWorkBatch>>,
}

#[async_trait::async_trait]
impl crate::store::RuntimePersistenceDecorator for StopAfterTerminalClaim {
    fn inner(&self) -> &(dyn crate::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn claim_checkpoint_work(
        &self,
        session_id: &SessionId,
        session_execution_lease: &crate::SessionExecutionLeaseAuthority,
        owner: &crate::LeaseOwnerIdentity,
        turn_id: &TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::TurnInputClaim>,
            Option<crate::QueuedWorkClaim>,
        ),
        crate::StoreError,
    > {
        if matches!(checkpoint, crate::CheckpointKind::BeforeCompletion) {
            let arriving = self.arriving_wake.lock().expect("arriving wake").take();
            if let Some(draft) = arriving {
                let batch = self
                    .inner
                    .enqueue_queued_work(draft)
                    .await
                    .expect("accept the wake that arrives during the turn");
                *self.arrived_wake.lock().expect("arrived wake") = Some(batch);
            }
        }
        let claims = self
            .inner
            .claim_checkpoint_work(
                session_id,
                session_execution_lease,
                owner,
                turn_id,
                checkpoint,
                max_inputs,
                policy,
            )
            .await?;
        let claimed = claims
            .0
            .as_ref()
            .map(|claim| {
                claim
                    .inputs
                    .iter()
                    .map(|input| input.input_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let claimed_batches = claims
            .1
            .as_ref()
            .map(|claim| {
                claim
                    .batches
                    .iter()
                    .map(|batch| batch.batch_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if matches!(checkpoint, crate::CheckpointKind::BeforeCompletion)
            && (!claimed.is_empty() || !claimed_batches.is_empty())
        {
            self.withheld_inputs
                .lock()
                .expect("withheld log")
                .extend(claimed);
            self.withheld_batches
                .lock()
                .expect("withheld log")
                .extend(claimed_batches);
            let stop = self.armed.lock().expect("stop slot").take();
            match stop {
                // A token-fired stop reaches the turn's cancellation gate
                // through the drive's spawned forwarder, and the commit
                // settles that gate first-writer-wins against its own
                // completion seal: without a rendezvous the seal can win
                // and the stop lands on the follow-on turn instead. This
                // claim is still open, so the commit cannot run — hold it
                // until the gate carries the stop.
                Some(Stop::Local(token)) => {
                    token.cancel();
                    let control = ActiveTurnControl::new(
                        self.effect_host.as_ref(),
                        crate::TurnAddress::new(session_id, turn_id),
                    )
                    .await
                    .expect("the running turn's cancellation gate");
                    let delivered = control
                        .watch_immediate(self.effect_host.as_ref())
                        .await
                        .expect("watch the turn's cancellation gate");
                    assert!(
                        delivered.is_some(),
                        "the held checkpoint leaves the gate open for the stop it must carry"
                    );
                }
                Some(Stop::Durable(request)) => {
                    crate::TurnWorkDriver::for_session(
                        Arc::clone(&self.effect_host),
                        SESSION_ID,
                        Arc::clone(&self.inner),
                    )
                    .request_cancel(*request)
                    .await
                    .expect("record the durable Immediate stop");
                }
                None => {}
            }
        }
        Ok(claims)
    }
}

struct Harness {
    store: Arc<dyn crate::RuntimePersistence>,
    decorated: Arc<StopAfterTerminalClaim>,
    effect_host: Arc<dyn crate::EffectHost>,
    runtime: crate::LashRuntime,
    requests: Arc<Mutex<Vec<crate::LlmRequest>>>,
}

impl Harness {
    fn request_mentions(&self, text: &str) -> usize {
        self.requests
            .lock()
            .map(|requests| {
                requests
                    .iter()
                    .filter(|request| format!("{request:?}").contains(text))
                    .count()
            })
            .unwrap_or_default()
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn run(
        &mut self,
        turn_id: &TurnId,
        text: &str,
        cancel: CancellationToken,
    ) -> crate::AgentFrameRun {
        let scope = self
            .effect_host
            .scoped(admit(crate::ExecutionScope::turn(SESSION_ID, turn_id)))
            .expect("scope the turn");
        self.runtime
            .stream_turn_with_agent_frames(
                direct_input(turn_id, text),
                crate::TurnOptions::new(cancel, scope),
            )
            .await
            .unwrap_or_else(|error| panic!("turn `{turn_id}` commits: {error:?}"))
    }
}

/// One cancelled turn that withholds `texts`, then the next turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn withheld_cancel_case(
    harness: &mut Harness,
    case: &str,
    disposition: crate::TurnCancelDisposition,
    texts: &[&str],
) {
    let session_id = SessionId::from(SESSION_ID);
    let cancelled_turn_id = TurnId::from(format!("{case}-cancelled"));
    let next_turn_id = TurnId::from(format!("{case}-next"));

    // Inject-now input sent while the turn runs: the terminal checkpoint
    // claims and withholds it for a follow-on turn.
    let mut input_ids = Vec::new();
    for text in texts {
        let accepted = harness
            .store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                &session_id,
                crate::TurnInputIngress::active_turn(
                    &cancelled_turn_id,
                    crate::TurnInputCheckpointBoundary::BeforeCompletion,
                ),
                crate::TurnInput::text(*text),
            ))
            .await
            .expect("enqueue inject-now input for the running turn");
        input_ids.push(accepted.input_id);
    }

    let local = CancellationToken::new();
    let stop = match disposition {
        crate::TurnCancelDisposition::Defer => Stop::Local(local.clone()),
        crate::TurnCancelDisposition::Drop => Stop::Durable(Box::new(
            crate::TurnCancelRequest::new(
                crate::TurnAddress::new(&session_id, &cancelled_turn_id),
                format!("{case}-stop"),
                Some("conformance-user".to_string()),
            )
            .undelivered(crate::TurnCancelDisposition::Drop)
            .mode(crate::TurnCancelMode::Immediate),
        )),
    };
    *harness.decorated.armed.lock().expect("stop slot") = Some(stop);
    harness
        .decorated
        .withheld_inputs
        .lock()
        .expect("withheld log")
        .clear();
    let run = harness
        .run(&cancelled_turn_id, "summarise the repo", local)
        .await;

    assert_eq!(
        *harness
            .decorated
            .withheld_inputs
            .lock()
            .expect("withheld log"),
        input_ids,
        "{case}: the terminal checkpoint must claim and withhold the inject-now input"
    );
    let outcomes = run
        .turns
        .iter()
        .map(|turn| format!("{:?}", turn.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        run.turns.len(),
        1,
        "{case}: a cancelled turn starts no follow-on for its withheld input: {outcomes:?}"
    );
    let cancelled = run.turns.into_iter().next().expect("the cancelled turn");
    let evidence = match &cancelled.outcome {
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("{case}: expected an Immediate cancel, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.undelivered, disposition, "{case}");
    for text in texts {
        assert_eq!(
            harness.request_mentions(text),
            0,
            "{case}: the cancelled turn's model never saw `{text}`"
        );
    }

    // Never completed: no application settles it.
    let applications = harness
        .store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read applications");
    assert!(
        applications
            .iter()
            .all(|application| !input_ids.contains(&application.input_id)),
        "{case}: the cancelled turn must not settle withheld input as completed: {applications:?}"
    );
    // Deferred rows stay pending for the next turn; dropped rows leave.
    let pending = harness
        .store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("read pending inputs")
        .into_iter()
        .filter(|pending| input_ids.contains(&pending.input.input_id))
        .map(|pending| (pending.input.input_id, pending.input.state))
        .collect::<Vec<_>>();
    let expected_pending = match disposition {
        crate::TurnCancelDisposition::Defer => input_ids
            .iter()
            .map(|input_id| (input_id.clone(), crate::TurnInputState::DeferredNextTurn))
            .collect(),
        crate::TurnCancelDisposition::Drop => Vec::new(),
    };
    assert_eq!(pending, expected_pending, "{case}: rows after the cancel");

    // The same record an unclaimed row gets, in enqueue order, on the turn
    // result and on the durable cancellation.
    let expected = input_ids
        .iter()
        .zip(texts)
        .map(|(input_id, text)| crate::TurnCancelAffectedInput {
            input_id: input_id.clone(),
            payload: crate::TurnInput::text(*text),
            disposition,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        cancelled.turn_cancel_input_outcome.affected_inputs, expected,
        "{case}: the turn result records the undelivered input"
    );
    let record = harness
        .store
        .turn_cancel_request(&crate::TurnAddress::new(&session_id, &cancelled_turn_id))
        .await
        .expect("read the cancellation record")
        .expect("the Immediate cancel is durable");
    assert_eq!(
        record.outcome.map(|outcome| outcome.affected_inputs),
        Some(expected),
        "{case}: the durable cancellation records the undelivered input"
    );

    // Deferred input is driven exactly once, in order, ahead of the next
    // turn's own input: the next drive admits the deferred head first, under
    // a root named by it (FIG-3600). Dropped input never reaches a turn.
    let run = harness
        .run(&next_turn_id, "carry on", CancellationToken::new())
        .await;
    assert_eq!(run.turns.len(), 1, "{case}: the next turn runs alone");
    let delivered = harness
        .store
        .list_turn_input_applications(&session_id)
        .await
        .expect("read applications after the next turn")
        .into_iter()
        .filter(|application| input_ids.contains(&application.input_id))
        .map(|application| (application.input_id, application.turn_id))
        .collect::<Vec<_>>();
    let expected_delivered = match disposition {
        crate::TurnCancelDisposition::Defer => input_ids
            .iter()
            .map(|input_id| (input_id.clone(), crate::TurnId::from(input_ids[0].as_str())))
            .collect(),
        crate::TurnCancelDisposition::Drop => Vec::new(),
    };
    assert_eq!(
        delivered, expected_delivered,
        "{case}: what the next turn delivers"
    );
    for text in texts {
        let expected_mentions = usize::from(disposition == crate::TurnCancelDisposition::Defer);
        assert_eq!(
            harness.request_mentions(text),
            expected_mentions,
            "{case}: the next turn's model sees `{text}` exactly as the disposition says"
        );
    }
    assert!(
        harness
            .store
            .list_pending_turn_inputs(&session_id)
            .await
            .expect("read pending inputs after the next turn")
            .iter()
            .all(|pending| !input_ids.contains(&pending.input.input_id)),
        "{case}: nothing withheld is left pending once the next turn commits"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn harness(backend: crate::Backend, store: Arc<dyn crate::RuntimePersistence>) -> Harness {
    let effect_host = backend.effect_host();
    let decorated = Arc::new(StopAfterTerminalClaim {
        inner: Arc::clone(&store),
        effect_host: Arc::clone(&effect_host),
        armed: Mutex::new(None),
        withheld_inputs: Mutex::new(Vec::new()),
        withheld_batches: Mutex::new(Vec::new()),
        arriving_wake: Mutex::new(None),
        arrived_wake: Mutex::new(None),
    });
    let runtime_store: Arc<dyn crate::RuntimePersistence> = decorated.clone();
    let requests = Arc::new(Mutex::new(Vec::<crate::LlmRequest>::new()));
    let provider = {
        let requests = Arc::clone(&requests);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let requests = Arc::clone(&requests);
                async move {
                    requests.lock().expect("request log").push(request);
                    Ok(text_response("summary of the repo"))
                }
            })
            .build()
            .into_handle()
    };
    let runtime = acceptance_runtime_for_session(
        SESSION_ID,
        &runtime_store,
        &backend,
        provider,
        Vec::new(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    Harness {
        store,
        decorated,
        effect_host,
        runtime,
        requests,
    }
}

/// An Immediate cancel that lands after the terminal checkpoint claimed and
/// withheld inject-now input settles that input through the undelivered
/// disposition — never as completed — in enqueue order, and records it on the
/// cancellation exactly as it records an unclaimed row. Covers the default
/// `Defer` for one and for two inputs, and a host-selected `Drop`.
pub async fn immediate_cancel_defers_withheld_inject_now_input(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let mut harness = harness(backend, store).await;

    withheld_cancel_case(
        &mut harness,
        &format!("{prefix}-withheld-defer"),
        crate::TurnCancelDisposition::Defer,
        &["also check the tests"],
    )
    .await;
    withheld_cancel_case(
        &mut harness,
        &format!("{prefix}-withheld-ordered"),
        crate::TurnCancelDisposition::Defer,
        &["first follow-up", "second follow-up"],
    )
    .await;
    withheld_cancel_case(
        &mut harness,
        &format!("{prefix}-withheld-drop"),
        crate::TurnCancelDisposition::Drop,
        &["dropped follow-up"],
    )
    .await;
}

/// One process wake for the session, as the process wake sender copies it
/// into the queue.
fn wake_delivery(process: &str, sequence: u64, text: &str) -> crate::ProcessWakeDelivery {
    crate::ProcessWakeDelivery {
        version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
            lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION
        )),
        wake_id: format!("{process}-wake-{sequence}"),
        target_session_id: SessionId::from(SESSION_ID),
        process_id: crate::ProcessId::fixture(process),
        sequence,
        event_type: "process.wake".to_string(),
        event_invocation: crate::RuntimeInvocation {
            attribution: crate::RuntimeAttribution::for_session(SESSION_ID),
            subject: crate::RuntimeSubject::ProcessEvent {
                process_id: crate::ProcessId::fixture(process),
                sequence,
                event_type: "process.wake".to_string(),
            },
            caused_by: None,
            replay: None,
        },
        process_caused_by: None,
        authority: crate::QueuedWorkAuthority::default(),
        input: text.to_string(),
        created_at_ms: 1,
    }
}

/// One cancelled turn whose terminal checkpoint withholds the wake `text`,
/// stopped with `disposition`, then the next turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn withheld_wake_case(
    harness: &mut Harness,
    case: &str,
    disposition: crate::TurnCancelDisposition,
    sequence: u64,
    text: &str,
) {
    let session_id = SessionId::from(SESSION_ID);
    let cancelled_turn_id = TurnId::from(format!("{case}-cancelled"));
    let next_turn_id = TurnId::from(format!("{case}-next"));

    // A wake that arrives while the turn runs: accepted after the turn's
    // input, its terminal checkpoint claims and withholds it for a follow-on
    // turn (FIG-3157).
    let wake = wake_delivery(&format!("{case}-process"), sequence, text);
    *harness
        .decorated
        .arriving_wake
        .lock()
        .expect("arriving wake") = Some(crate::runtime::process_wake_batch_draft(wake.clone()));

    let local = CancellationToken::new();
    let stop = match disposition {
        crate::TurnCancelDisposition::Defer => Stop::Local(local.clone()),
        crate::TurnCancelDisposition::Drop => Stop::Durable(Box::new(
            crate::TurnCancelRequest::new(
                crate::TurnAddress::new(&session_id, &cancelled_turn_id),
                format!("{case}-stop"),
                Some("conformance-user".to_string()),
            )
            .undelivered(crate::TurnCancelDisposition::Drop)
            .mode(crate::TurnCancelMode::Immediate),
        )),
    };
    *harness.decorated.armed.lock().expect("stop slot") = Some(stop);
    harness
        .decorated
        .withheld_batches
        .lock()
        .expect("withheld log")
        .clear();
    let run = harness
        .run(&cancelled_turn_id, "summarise the repo", local)
        .await;
    let batch = harness
        .decorated
        .arrived_wake
        .lock()
        .expect("arrived wake")
        .take()
        .expect("the wake arrived before the terminal checkpoint claimed");

    assert_eq!(
        *harness
            .decorated
            .withheld_batches
            .lock()
            .expect("withheld log"),
        vec![batch.batch_id.clone()],
        "{case}: the terminal checkpoint must claim and withhold the wake"
    );
    let outcomes = run
        .turns
        .iter()
        .map(|turn| format!("{:?}", turn.outcome))
        .collect::<Vec<_>>();
    assert_eq!(
        run.turns.len(),
        1,
        "{case}: a cancelled turn starts no follow-on for its withheld wake: {outcomes:?}"
    );
    let cancelled = run.turns.into_iter().next().expect("the cancelled turn");
    let evidence = match &cancelled.outcome {
        crate::TurnOutcome::Stopped(crate::TurnStop::Cancelled { evidence }) => evidence.clone(),
        other => panic!("{case}: expected an Immediate cancel, got {other:?}"),
    };
    assert_eq!(evidence.mode, crate::TurnCancelMode::Immediate);
    assert_eq!(evidence.undelivered, disposition, "{case}");
    assert_eq!(
        harness.request_mentions(text),
        0,
        "{case}: the cancelled turn's model never saw the wake"
    );

    // The affected-item record names the wake as deferred, on the turn
    // result and on the durable cancellation; no host input was affected.
    let expected = vec![crate::TurnCancelAffectedWake::deferred(
        batch.batch_id.clone(),
        batch.items[0].item_id.clone(),
        wake.clone(),
    )];
    assert_eq!(
        cancelled.turn_cancel_input_outcome.affected_wakes, expected,
        "{case}: the turn result records the deferred wake"
    );
    assert!(
        cancelled
            .turn_cancel_input_outcome
            .affected_inputs
            .is_empty(),
        "{case}: no host input was affected"
    );
    let record = harness
        .store
        .turn_cancel_request(&crate::TurnAddress::new(&session_id, &cancelled_turn_id))
        .await
        .expect("read the cancellation record")
        .expect("the Immediate cancel is durable");
    assert_eq!(
        record
            .outcome
            .map(|outcome| (outcome.affected_inputs, outcome.affected_wakes)),
        Some((Vec::new(), expected)),
        "{case}: the durable cancellation records the deferred wake"
    );

    // Deferred whatever the host's disposition, which governs host-authored
    // items only: never completed, never dropped. The row stays queued at
    // its own position, unclaimed.
    let queued = harness
        .store
        .list_queued_work(&session_id)
        .await
        .expect("read queued work after the cancel")
        .into_iter()
        .map(|batch| (batch.batch_id, batch.enqueue_seq))
        .collect::<Vec<_>>();
    assert_eq!(
        queued,
        vec![(batch.batch_id.clone(), batch.enqueue_seq)],
        "{case}: the withheld wake is deferred at its own position, never settled"
    );

    // The deferred wake is delivered exactly once by the next run: the next
    // turn's terminal checkpoint claims it and its follow-on turn drives it.
    let run = harness
        .run(&next_turn_id, "carry on", CancellationToken::new())
        .await;
    assert!(
        run.turns
            .iter()
            .all(|turn| !matches!(turn.outcome, crate::TurnOutcome::Stopped(_))),
        "{case}: the next run commits"
    );
    assert_eq!(
        harness.request_mentions(text),
        1,
        "{case}: the next run's model sees the deferred wake exactly once"
    );
    assert!(
        harness
            .store
            .list_queued_work(&session_id)
            .await
            .expect("read queued work after the next run")
            .is_empty(),
        "{case}: the delivered wake settles with the turn that rendered it"
    );
}

/// An Immediate cancel that lands after the terminal checkpoint claimed and
/// withheld a process wake never settles that wake as completed and never
/// drops it: whatever the host's undelivered disposition, a held wake is
/// deferred — its claim released in the cancel commit, its position kept,
/// its redelivery floor untouched — recorded on the cancellation, and
/// delivered exactly once by the next turn (FIG-3543, ADR 0101 §10). Covers
/// the default `Defer` and a host-selected `Drop`.
pub async fn immediate_cancel_defers_withheld_process_wakes(
    prefix: &str,
    backend: crate::Backend,
    store: Arc<dyn crate::RuntimePersistence>,
) {
    let mut harness = harness(backend, store).await;

    withheld_wake_case(
        &mut harness,
        &format!("{prefix}-wake-drop"),
        crate::TurnCancelDisposition::Drop,
        1,
        "the build finished",
    )
    .await;
    withheld_wake_case(
        &mut harness,
        &format!("{prefix}-wake-defer"),
        crate::TurnCancelDisposition::Defer,
        1,
        "the deploy finished",
    )
    .await;
}
