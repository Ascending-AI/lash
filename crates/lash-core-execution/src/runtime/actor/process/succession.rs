//! The owner's half of a successor pair (FIG-6121, D2): it records the
//! host's request from its mail, and carries the process onto the pair once
//! the run is at rest.
//!
//! Both happen in owner commits under the claim's fence. The pass that
//! reads `process.successor` mail moves the request into the driver and
//! acknowledges the mail in one `process.advance`. From then on the request
//! is the owner's state, which no release, park or drain drops. A process
//! whose engine has not started has no run to carry, so that pass applies
//! the request at once.
//!
//! A pass applies a recorded request once nothing of the activation runs
//! and the engine answers the run parked. The apply is one commit: the
//! carried engine state, the driver without the request, the
//! `process.succeeded` event whose fold rewrites the record's input and
//! definition id, and the requirements of the new env. It releases the
//! actor `ready`, so the next claim builds the new env's composition. A
//! crash before it leaves the old pair with the request pending; a crash
//! after it, the new pair whole. A refusal at the apply records
//! `process.successor_refused`, and the process goes on as written.

use lash_durable::domain::{ExecRootRow, ProcessActorRow, ProcessWrite, RunSeq};
use lash_durable::runner::Owned;
use lash_durable::{
    ActorTx, CommitLabel, DomainWrite, DurableError, Release, StoreFailure, StoreFailureKind,
};

use super::activation::{Live, Pass, ProcessActivation, corrupt};
use super::driver::Driver;
use super::effects::append_event;
use crate::runtime::actor::round::RunFold;
use crate::runtime::process::engine_state::EngineState;
use crate::runtime::process::{
    ProcessEventAppendRequest, ProcessSuccessorRefusal, ProcessSuccessorRequest, SUCCESSOR_MAIL,
    SuccessorCarry, carry_refusal, relinked_step,
};
use crate::{ProcessEngine, ProcessId, ProcessInput, ProcessRecord};

/// What a successor step did with the pass's transaction.
pub(super) enum Succession {
    /// It committed: the pass is over.
    Ended(Pass),
    /// Nothing to do now: the pass goes on.
    AsWritten(Box<ActorTx>),
}

/// What the owner reads a recorded request against.
pub(super) struct Current<'a> {
    pub(super) process: &'a ProcessId,
    pub(super) row: &'a ProcessActorRow,
    pub(super) record: &'a ProcessRecord,
    pub(super) engine: &'a dyn ProcessEngine,
    pub(super) root: Option<&'a ExecRootRow>,
}

fn unavailable(message: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: format!("carrying a process onto its successor: {message}"),
    })
}

impl ProcessActivation {
    /// Record the newest `process.successor` mail of `tx` in the driver, or
    /// apply it at once to a process whose engine has not started.
    pub(super) async fn record_successor(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        current: &Current<'_>,
        driver: &Driver,
    ) -> Result<Succession, DurableError> {
        let Some(mail) = tx
            .mail()
            .iter()
            .filter(|mail| mail.kind.as_str() == SUCCESSOR_MAIL)
            .max_by_key(|mail| mail.seq)
            .cloned()
        else {
            return Ok(Succession::AsWritten(Box::new(tx)));
        };
        tx.ack_through(mail.seq);
        let request = match serde_json::from_str::<ProcessSuccessorRequest>(&mail.body) {
            Ok(request) if !request.is_current(current.record) => request,
            // A resend of the pair the process already runs, or mail that
            // is no request: acknowledged, and nothing changes.
            Ok(_) => {
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Succession::Ended(Pass::Again));
            }
            Err(error) => {
                tracing::warn!(process_id = %current.process, %error, "successor mail does not decode; dropped");
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Succession::Ended(Pass::Again));
            }
        };
        if current.row.state_rev == 0 {
            return self
                .apply_successor(owned, tx, current, driver.clone(), request, None, None)
                .await;
        }
        let mut driver = driver.clone();
        driver.successor = Some(request);
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: current.process.clone(),
            expected_rev: current.row.state_rev,
            driver_json: driver.encode(),
        }));
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        Ok(Succession::Ended(Pass::Again))
    }

    /// Carry the process onto the request its driver holds, when the run is
    /// at rest: nothing of `live` runs and the engine answers it parked.
    #[expect(
        clippy::too_many_arguments,
        reason = "one step of the pass, over what the pass has read"
    )]
    pub(super) async fn apply_recorded_successor(
        &self,
        owned: &Owned,
        tx: ActorTx,
        current: &Current<'_>,
        driver: &Driver,
        state: &EngineState,
        fold: &RunFold,
        live: &Live,
    ) -> Result<Succession, DurableError> {
        let Some(request) = driver.successor.clone() else {
            return Ok(Succession::AsWritten(Box::new(tx)));
        };
        if live.busy() {
            return Ok(Succession::AsWritten(Box::new(tx)));
        }
        let mut driver = driver.clone();
        driver.successor = None;
        self.apply_successor(owned, tx, current, driver, request, Some(state), Some(fold))
            .await
    }

    /// Apply `request` to the process as `driver` (without the request)
    /// and `state` stand, or record why it is refused.
    #[expect(
        clippy::too_many_arguments,
        reason = "one step of the pass, over what the pass has read"
    )]
    async fn apply_successor(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        current: &Current<'_>,
        driver: Driver,
        request: ProcessSuccessorRequest,
        state: Option<&EngineState>,
        fold: Option<&RunFold>,
    ) -> Result<Succession, DurableError> {
        let ProcessInput::Engine {
            kind,
            payload,
            launch,
        } = current.record.input.as_ref()
        else {
            return Err(corrupt("a successor request", "the process runs no engine"));
        };
        let refusal = if current.record.cancel_request.is_some() {
            Some(ProcessSuccessorRefusal::Cancelling)
        } else {
            let envs = self.backend.process_env_store();
            let load = |env_ref| {
                let envs = std::sync::Arc::clone(&envs);
                async move {
                    crate::runtime::process::load_process_env(envs.as_ref(), &env_ref)
                        .await
                        .map_err(unavailable)
                }
            };
            let old = load(launch.env.clone()).await?;
            let new = load(request.env.clone()).await?;
            relinked_step(driver.in_flight_effects(), &old, &new)
        };
        let carried = match refusal {
            Some(refusal) => Err(refusal),
            None => match self.steps.succession(current.record) {
                None => Err(ProcessSuccessorRefusal::Unsupported { kind: kind.clone() }),
                Some(succession) => match succession
                    .carry(current.process, payload, &request.payload, state)
                    .await
                {
                    Ok(SuccessorCarry::Carried { state, own_formats }) => Ok((state, own_formats)),
                    // A run in flight is carried at its next park.
                    Ok(SuccessorCarry::InFlight) => {
                        return Ok(Succession::AsWritten(Box::new(tx)));
                    }
                    Err(error) => Err(carry_refusal(error).map_err(|error| unavailable(&error))?),
                },
            },
        };
        if current.row.state_rev > 0 {
            tx.write(DomainWrite::Process(ProcessWrite::Advance {
                process: current.process.clone(),
                expected_rev: current.row.state_rev,
                driver_json: driver.encode(),
            }));
        }
        let (carried, own_formats) = match carried {
            Ok(carried) => carried,
            Err(refusal) => {
                tracing::info!(process_id = %current.process, %refusal, "successor pair refused");
                append_event(
                    &mut tx,
                    current.process,
                    ProcessEventAppendRequest::successor_refused(
                        current.process,
                        &request,
                        &refusal,
                    ),
                );
                owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
                return Ok(Succession::Ended(Pass::Again));
            }
        };
        if own_formats && let Some(formats) = self.backend.formats().process(kind) {
            tx.stamp_formats(formats.clone());
        }
        if let Some(carried) = &carried {
            super::state_graph::write_engine_state(
                &mut tx,
                current.process,
                current.engine.kind(),
                carried,
                current.root,
            )?;
        }
        append_event(
            &mut tx,
            current.process,
            ProcessEventAppendRequest::succeeded(current.process, &request),
        );
        // The new env's components are what the process requires from now
        // on, beside the callbacks its unsettled steps were admitted under.
        let mut next = current.record.clone();
        crate::runtime::process::apply_successor_input(&mut next, &request)
            .map_err(|error| corrupt("a successor request", error))?;
        let components = self
            .steps
            .requirements(&next, &super::step_admission::step_requests(&driver))
            .await?
            .unwrap_or_default();
        let held = driver.steps.values().filter_map(|step| {
            fold?
                .round(RunSeq(step.run))?
                .members()
                .get(usize::try_from(step.member).ok()?)?
                .draft()
                .binding()
        });
        tx.set_requires(crate::runtime::actor::claim_obligations::process_requires(
            components, held,
        ));
        // The activation built its tools from the old env: the next claim
        // builds the new one's.
        tx.give_up(Release::Ready);
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        tracing::info!(process_id = %current.process, env = %request.env, "process carried onto its successor pair");
        Ok(Succession::Ended(Pass::Released))
    }
}
