//! PANICKING-ACTIVATION-PARKS-AT-BUDGET (FIG-6096): an activation that
//! panics before any advance commits is a failed activation, though the
//! runner's hand-back of its actor commits.

use lash_durable::ActorState;
use serde_json::json;

use super::blockers::observed;
use super::{
    LOOP_BUDGET, LawBroken, LawOutcome, SETTLE, SHORT, actor_state, eventually, law_backend,
    noted_calls, payload, root, serve, tag, terminal,
};
use lash_execution_contracts::backend::Backend;
use lash_execution_contracts::park::ProcessParkReason;
use lash_execution_contracts::process::ProcessParkState;

/// A process whose every activation panics before any advance commits
/// parks with `ActivationLoop` after exactly the activation-loop budget's
/// claims ran its engine, on the production runner. A process whose
/// panicking claims are interrupted by a committed advance never parks,
/// however many panicked in total, and ends.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_panicking_activation_parks_at_its_budget_and_progress_resets_the_count(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let panicking_tag = tag("panicking");
        let panicking = root(&backend, payload(&panicking_tag, "panic")).await?;
        eventually(SETTLE, "the panicking process parked", || async {
            Ok(actor_state(&backend, &panicking).await? == Some(ActorState::Parked))
        })
        .await?;
        let seen = observed(&backend, &panicking).await?;
        ensure!(
            seen.park
                == ProcessParkState::Parked(ProcessParkReason::ActivationLoop {
                    failed_activations: LOOP_BUDGET,
                }),
            "a panicking process parked {:?}, not at its budget of {LOOP_BUDGET}",
            seen.park
        );
        let ran = noted_calls(&panicking_tag);
        ensure!(
            ran == LOOP_BUDGET,
            "a panicking process ran its engine in {ran} claims, not {LOOP_BUDGET}"
        );

        let flapping_tag = tag("flapping");
        let mut script = payload(&flapping_tag, "flap");
        script["until_ms"] = json!(
            backend
                .durable()
                .now()
                .await?
                .after_millis(i64::try_from(SHORT.as_millis()).unwrap_or(i64::MAX))
                .0
        );
        let flapping = root(&backend, script).await?;
        eventually(SETTLE, "the flapping process ended", || async {
            Ok(terminal(&backend, &flapping).await?.is_some())
        })
        .await?;
        let ran = noted_calls(&flapping_tag);
        ensure!(
            ran == 2 * LOOP_BUDGET,
            "a flapping process ran its engine in {ran} claims, not {}",
            2 * LOOP_BUDGET
        );
        Ok::<_, LawBroken>(())
    }
    .await;
    serving.stop().await;
    result
}

/// The session-turn mail service panics until repaired; its child turn's
/// completion and cancellation calls are counted independently.
#[derive(Default)]
struct PanickingTurns {
    panics: std::sync::atomic::AtomicBool,
    mails: std::sync::atomic::AtomicU32,
    outcomes: std::sync::atomic::AtomicU32,
    cancels: std::sync::atomic::AtomicU32,
}

#[async_trait::async_trait]
impl super::super::process::SessionTurns for PanickingTurns {
    async fn mail(
        &self,
        _: &lash_execution_contracts::process::ProcessRecord,
        _: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<
        super::super::process::SessionTurnMail,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.mails.fetch_add(1, super::Ordering::SeqCst);
        assert!(
            !self.panics.load(super::Ordering::SeqCst),
            "the child-turn mail service panics"
        );
        Ok(super::super::process::SessionTurnMail::Mailed)
    }

    async fn cancel(
        &self,
        _: &lash_execution_contracts::process::ProcessRecord,
        _: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<
        super::super::process::SessionTurnCancel,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.cancels.fetch_add(1, super::Ordering::SeqCst);
        Ok(super::super::process::SessionTurnCancel::Withdrawn)
    }

    async fn outcome(
        &self,
        _: &lash_execution_contracts::process::ProcessRecord,
        _: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<
        lash_execution_contracts::process::ProcessOutcome,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.outcomes.fetch_add(1, super::Ordering::SeqCst);
        Ok(
            lash_execution_contracts::process::ProcessOutcome::from_tool_output(
                crate::ToolCallOutput::success(json!({"child_answer": "done"})),
            ),
        )
    }
}

/// SESSION-TURN-PANIC-PARKS-AT-BUDGET (FIG-6129): a process whose child
/// turn's mail panics on every activation parks at the claim budget, keeps
/// its committed driver and child wait, and runs no further service calls.
/// Repair and redrive finish it once; cancellation of a second parked
/// process ends it without entering the still-panicking service.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_panicking_session_turn_process_parks_at_its_budget_and_resumes_or_cancels(
    backend: &Backend,
) -> LawOutcome {
    use super::{
        NODE, Ordering, actor, cancel, cancellation, find, record, register, serve_activation,
    };
    use lash_durable::domain::{MailDomainWrite, WaitLifecycle, WaitPurpose, WaitResolution};
    use lash_durable::{CommitLabel, MailTx, NoProbe};
    use std::sync::Arc;

    let backend = law_backend(backend)?;
    let turns = Arc::new(PanickingTurns::default());
    let activation = super::ProcessActivation::new(
        backend.clone(),
        Arc::new(super::LawSteps),
        Arc::new(NoProbe),
    )
    .with_session_turns(Arc::clone(&turns) as _);
    let serving = serve_activation(
        &backend,
        NODE,
        Vec::new(),
        Arc::clone(backend.durable()),
        activation,
    );
    let result = async {
        for cancel_parked in [false, true] {
            turns.panics.store(true, Ordering::SeqCst);
            let mails_before = turns.mails.load(Ordering::SeqCst);
            let request = lash_execution_contracts::session_types::SessionCreateRequest::root(
                crate::PluginOptions::default(),
            );
            let process = register(
                &backend,
                lash_execution_contracts::process::ProcessRegistration::new(
                    lash_execution_contracts::process::ProcessInput::SessionTurn {
                        definition_key: tag("panicking-child-turn"),
                        create_request: Box::new(request),
                        turn_input: Box::new(crate::TurnInput::text("child work")),
                        result: lash_execution_contracts::process::SessionTurnOutcome::Turn,
                    },
                    lash_execution_contracts::process::ProcessProvenance::host(),
                    lash_execution_contracts::process::LifetimeDecision::Detached,
                ),
            )
            .await?;
            // The first claim commits the driver and wait, then panics.
            // Only the following budget's claims committed nothing.
            eventually(
                SETTLE,
                "the panicking session-turn process parked",
                || async {
                    // Bound the red-side witness without thousands of panic logs.
                    ensure!(
                        turns.mails.load(Ordering::SeqCst) - mails_before <= LOOP_BUDGET + 1,
                        "the session-turn mail service exceeded the claim budget"
                    );
                    Ok(actor_state(&backend, &process).await? == Some(ActorState::Parked))
                },
            )
            .await?;
            let seen = observed(&backend, &process).await?;
            ensure!(
                seen.park
                    == ProcessParkState::Parked(ProcessParkReason::ActivationLoop {
                        failed_activations: LOOP_BUDGET,
                    }),
                "the session-turn process parked {:?}, not at its budget",
                seen.park
            );
            let calls = turns.mails.load(Ordering::SeqCst);
            ensure!(
                calls - mails_before == LOOP_BUDGET + 1,
                "the mail service ran {} times, not the initial claim plus its budget",
                calls - mails_before
            );
            ensure!(
                record(&backend, &process).await?.terminal().is_none(),
                "the park wrote a terminal"
            );
            let key = actor(&process)?;
            let row = backend
                .durable()
                .process(&process)
                .await?
                .ok_or_else(|| LawBroken("the parked process row vanished".into()))?;
            let driver = row.driver_json.clone();
            let waits = backend.durable().pending_waits(&key).await?;
            ensure!(
                row.state_rev == 1
                    && waits.len() == 1
                    && waits[0].purpose
                        == WaitPurpose::ChildSession {
                            process: process.clone(),
                            deadline: None,
                        }
                    && waits[0].lifecycle == WaitLifecycle::Pending,
                "the park lost the committed driver or child wait: {row:?}, {waits:?}"
            );

            if cancel_parked {
                cancel(&backend, &process).await?;
                eventually(
                    SETTLE,
                    "the parked session-turn process ended cancelled",
                    || async { Ok(terminal(&backend, &process).await?.is_some()) },
                )
                .await?;
                let end = terminal(&backend, &process).await?.unwrap_or_default();
                ensure!(
                    cancellation(&end) == Some(("operator_requested".into(), false)),
                    "the parked session-turn process did not end cancelled: {end}"
                );
                ensure!(
                    turns.mails.load(Ordering::SeqCst) == calls
                        && turns.cancels.load(Ordering::SeqCst) == 0,
                    "the parked process's cancel entered the child service"
                );
            } else {
                ensure!(
                    backend
                        .durable()
                        .process(&process)
                        .await?
                        .is_some_and(|row| row.driver_json == driver),
                    "the parked driver changed"
                );
                turns.panics.store(false, Ordering::SeqCst);
                ensure!(
                    backend.redrive_process(&process, "process-laws").await?,
                    "the redrive found no parked process"
                );
                eventually(
                    SETTLE,
                    "the repaired process waits on its child",
                    || async {
                        Ok(actor_state(&backend, &process).await? == Some(ActorState::Waiting))
                    },
                )
                .await?;
                ensure!(
                    backend.durable().pending_waits(&key).await? == waits,
                    "the redrive replaced the committed child wait"
                );
                let mut tx = MailTx::new();
                use sha2::{Digest as _, Sha256};
                let resolution_ref =
                    serde_json::to_string(&super::Resolution::Ok(json!({"run": process.as_str()})))
                        .map_err(|error| LawBroken(error.to_string()))?;
                tx.write(MailDomainWrite::ResolveWait(WaitResolution {
                    id: waits[0].id,
                    by_host: false,
                    digest: format!("sha256:{:x}", Sha256::digest(&resolution_ref)),
                    resolution_ref,
                }));
                backend.commit_mail(tx, CommitLabel::WAIT_RESOLVE).await?;
                eventually(SETTLE, "the repaired process ended", || async {
                    Ok(terminal(&backend, &process).await?.is_some())
                })
                .await?;
                let end = terminal(&backend, &process).await?.unwrap_or_default();
                ensure!(
                    find(&end, "child_answer") == Some(&json!("done")),
                    "the repaired process did not answer its child: {end}"
                );
                ensure!(
                    turns.mails.load(Ordering::SeqCst) == calls + 1
                        && turns.outcomes.load(Ordering::SeqCst) == 1,
                    "redrive did not mail and finish the child once"
                );
            }
        }
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}
