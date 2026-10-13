//! How a process's steps are admitted (ADR 0132 §10): an engine's batch
//! held against the process's `max_tool_calls`, each step pinned by its
//! host's admission and recorded on the driver; and the handoff of a
//! `call_dynamic` call (P6), whose committed resolution is consumed by the
//! commit that admits the call of its matched tool, without the engine.

use std::collections::BTreeSet;

use lash_core_store::tool_run::{MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole};
use lash_durable::domain::{OwnerKey, ProcessActorRow, ProcessWrite, RunSeq, ScopeKey};
use lash_durable::runner::Owned;
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant};
use lash_sansio::{ToolCallAdmission, ToolCallPosition};

use super::activation::{Live, Pass, corrupt, millis, refused, unavailable};
use super::driver::{Driver, InFlight};
use super::effects::record_omitted_effects;
use super::terminal::{ProcessParkReason, record_terminal};
use crate::runtime::actor::round::{self, AdmittedExecution, ExecutionDraft, RoundDraft, RunFold};
use crate::runtime::process::engine_state::StepRequest;
use crate::runtime::process::steps::StepRefusal;
use crate::{ProcessId, ProcessRecord};

use super::ProcessActivation;

/// The rows a pass read, which the commit that consumes a settled step is
/// written against.
#[derive(Clone, Copy)]
pub(super) struct Consumed<'a> {
    pub(super) process: &'a ProcessId,
    pub(super) row: &'a ProcessActorRow,
    pub(super) record: &'a ProcessRecord,
    pub(super) fold: &'a RunFold,
    pub(super) now: DurableInstant,
}

/// A `call_dynamic` call's committed resolution, and the call of the tool
/// its match names, which lash admits in its place (P6).
pub(super) struct Handoff {
    pub(super) resolution: InFlight,
    pub(super) target: StepRequest,
}

/// How a batch of steps is held against the process's `max_tool_calls`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Hold {
    /// The engine asked for them: their tool steps count, and a batch past
    /// the limit ends the process.
    Counted,
    /// A call already held moved to them: the hold it had is theirs.
    Moved,
}

/// How a batch of steps' admission ended.
pub(super) enum Admission {
    /// Every step is admitted on the transaction.
    Admitted,
    /// The process ends with this outcome instead.
    Ended(crate::ProcessOutcome),
    /// A step was refused before its admission.
    Refused(StepRefusal),
}

impl ProcessActivation {
    /// Rewrite on `tx` the requirements of the process `record` once it
    /// holds `driver`'s steps (S7): its components' config tokens, and the
    /// callbacks its unsettled steps were admitted under, those held from
    /// earlier rounds of `fold` and those `fresh` admits. The requirements
    /// written, or `None` when they are unchanged.
    pub(super) async fn rewrite_requires(
        &self,
        tx: &mut ActorTx,
        record: &ProcessRecord,
        driver: &Driver,
        fold: &RunFold,
        fresh: &[AdmittedExecution],
        live: &Live,
    ) -> Result<Option<Vec<String>>, DurableError> {
        let components = self
            .steps
            .requirements(record, &step_requests(driver))
            .await?
            .unwrap_or_default();
        let held = driver.steps.values().filter_map(|step| {
            fold.round(RunSeq(step.run))?
                .members()
                .get(usize::try_from(step.member).ok()?)?
                .draft()
                .binding()
        });
        let admitted = fresh
            .iter()
            .filter_map(|execution| execution.draft().binding());
        let requires = crate::runtime::actor::claim_obligations::process_requires(
            components,
            held.chain(admitted),
        );
        if requires == live.requires {
            return Ok(None);
        }
        tx.set_requires(requires.clone());
        Ok(Some(requires))
    }

    /// Consume `handoff`'s resolution, a `call_dynamic` call's resolution
    /// step whose committed outcome resolved its path, by admitting its
    /// target, the call of the matched tool, in its place (P6): one commit removes the
    /// resolution from the driver, admits the target under the same step
    /// name at the driver's next run, moves the call's hold against
    /// `max_tool_calls` to it, and replaces the resolver's claim obligation
    /// with the target's. The engine is not called and its state does not
    /// move: it is answered once, by the target. A crash after this commit
    /// resumes the same target under the same call id, and nothing resolves
    /// the path again; one before it is recomputed from the same rows.
    pub(super) async fn hand_off(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        consumed: &Consumed<'_>,
        mut driver: Driver,
        live: &mut Live,
        handoff: Handoff,
    ) -> Result<Pass, DurableError> {
        let Handoff { resolution, target } = handoff;
        let Consumed {
            process,
            row,
            record,
            fold,
            now,
        } = *consumed;
        if let Some(held) = driver.held.get_mut(&resolution.run) {
            *held = held.saturating_sub(1);
            if *held == 0 {
                driver.held.remove(&resolution.run);
            }
        }
        let mut fresh = Vec::new();
        match self
            .admit_steps(
                &mut tx,
                process,
                record,
                &mut driver,
                vec![target],
                now,
                &mut fresh,
                Hold::Moved,
            )
            .await?
        {
            Admission::Admitted => {}
            Admission::Refused(StepRefusal::CompositionUnavailable { component, reason }) => {
                let reason = ProcessParkReason::CompositionUnavailable {
                    component,
                    message: reason,
                };
                return self.park(owned, tx, &reason).await;
            }
            Admission::Refused(refusal) => {
                return self
                    .end_with(
                        owned,
                        tx,
                        process,
                        record,
                        &driver,
                        refused(refusal.to_string()),
                    )
                    .await;
            }
            Admission::Ended(outcome) => {
                return self
                    .end_with(owned, tx, process, record, &driver, outcome)
                    .await;
            }
        }
        super::waiting::settle(&mut tx, process, record, &driver);
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: row.state_rev,
            driver_json: driver.encode(),
        }));
        self.record_published(&mut tx, process, row);
        let requires = self
            .rewrite_requires(&mut tx, record, &driver, fold, &fresh, live)
            .await?;
        owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        if let Some(requires) = requires {
            live.requires = requires;
        }
        live.lifecycle.admitted(&fresh);
        Ok(Pass::Again)
    }

    /// End `process` with `outcome`, which lash decided for it.
    pub(super) async fn end_with(
        &self,
        owned: &Owned,
        mut tx: ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &Driver,
        outcome: crate::ProcessOutcome,
    ) -> Result<Pass, DurableError> {
        super::waiting::end(&mut tx, process, record);
        record_omitted_effects(&mut tx, process, driver, self.fleet());
        record_terminal(&mut tx, process, &outcome)?;
        tx.ack_seen();
        self.commit_terminal(owned, tx, process).await?;
        Ok(Pass::Again)
    }

    /// Admit `requests` on `tx` as the process's next run of steps, each
    /// under what its host's admission pins, held against the process's
    /// `max_tool_calls` as `hold` says, and record them on `driver`. A step
    /// its admission settled (D3) commits its outcome with the admission;
    /// every other one is pushed to `fresh`, to run once this commits.
    #[expect(
        clippy::too_many_arguments,
        reason = "the transition and the rows its admission is recorded against"
    )]
    pub(super) async fn admit_steps(
        &self,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &mut Driver,
        requests: Vec<StepRequest>,
        now: DurableInstant,
        fresh: &mut Vec<AdmittedExecution>,
        hold: Hold,
    ) -> Result<Admission, DurableError> {
        let tools = requests
            .iter()
            .filter(|request| matches!(request, StepRequest::Tool { .. }))
            .count();
        if tools > 0 && hold == Hold::Counted {
            let limit = match self.steps.max_tool_calls(record).await {
                Ok(limit) => limit,
                Err(StepRefusal::Unavailable { reason, .. }) => {
                    return Err(unavailable(reason));
                }
                Err(refusal) => return Ok(Admission::Refused(refusal)),
            };
            // A run is held whole while any of its steps is in
            // flight; a consumed run is held no more.
            let live: BTreeSet<u64> = driver.steps.values().map(|step| step.run).collect();
            driver.held.retain(|run, _| live.contains(run));
            let counted = driver.held.values().sum::<usize>();
            if let Some(limit) = limit
                && counted.saturating_add(tools) > limit.get()
            {
                return Ok(Admission::Ended(tool_call_limit(
                    crate::ToolCallLimitExceeded {
                        scope: crate::ToolCallLimitScope::Process,
                        limit,
                        counted,
                        requested: tools,
                    },
                )));
            }
        }
        if tools > 0 {
            driver.held.insert(driver.next_run, tools);
        }
        let run = driver.next_run;
        driver.next_run += 1;
        let admission = ToolCallAdmission::process("", process.clone());
        let mut drafts = Vec::with_capacity(requests.len());
        let mut settled = Vec::with_capacity(requests.len());
        for (member, request) in (0_u64..).zip(requests) {
            if driver.steps.contains_key(request.step()) {
                return Ok(Admission::Ended(refused(format!(
                    "step `{}` is already in flight",
                    request.step().0
                ))));
            }
            let admitted = match self.steps.admit(record, &request, millis(now)).await {
                Ok(admitted) => admitted,
                // Not a refusal: the pass did not commit, and the
                // next one asks again.
                Err(StepRefusal::Unavailable { reason, .. }) => {
                    return Err(unavailable(reason));
                }
                Err(refusal) => return Ok(Admission::Refused(refusal)),
            };
            let call = admission.call_id(&[ToolCallPosition::ProcessStep {
                step: &request.step().0,
                run,
            }]);
            let step = InFlight {
                run,
                member,
                call,
                policy: admitted.policy,
                limit_expires_at: admitted.limit.expires_at,
                limit_max_slice_ms: u64::try_from(admitted.limit.max_slice.as_millis())
                    .unwrap_or(u64::MAX),
                request,
            };
            drafts.push(
                ExecutionDraft::new(
                    step.call.clone(),
                    admitted.tool,
                    step_request_material(process, &step),
                    step.policy,
                    step.limit(),
                    admitted.park,
                )
                .with_binding(admitted.binding),
            );
            settled.push(admitted.settled.map(|output| {
                self.steps
                    .admitted_output(process, &step.request, &step.call, output)
            }));
            driver.steps.insert(step.request.step().clone(), step);
        }
        if drafts.is_empty() {
            return Ok(Admission::Ended(refused(
                "a Steps action names no step".to_owned(),
            )));
        }
        // A step that may park has its completion wait pinned with
        // its admission, as a round member has.
        let admitted = round::admit_round(
            tx,
            &ScopeKey::Process(process.clone()),
            RoundDraft {
                owner: OwnerKey::Process(process.clone()),
                run: RunSeq(run),
                members: drafts,
            },
        )
        .map_err(|refusal| corrupt("a process step's admission", refusal))?;
        // A step its admission settled (D3) commits its outcome with
        // the admission and starts no body.
        for (member, settled) in admitted.members().iter().zip(settled) {
            match settled {
                Some(output) => round::settle(tx, member, output, Vec::new())
                    .map_err(|refusal| corrupt("a settled step's admission", refusal))?,
                None => fresh.push(member.clone()),
            }
        }
        Ok(Admission::Admitted)
    }
}

#[expect(
    clippy::expect_used,
    reason = "a step's request is plain JSON whose material reference always encodes"
)]
fn step_request_material(
    process: &ProcessId,
    step: &InFlight,
) -> lash_core_store::tool_run::MaterialRef {
    MaterialPayload::new(
        MaterialOwner::Process {
            process_id: process.clone(),
        },
        MaterialRole::PreparedRequest,
        step.request.input().to_string(),
    )
    .reference(MaterialLocation::JournalLocal)
    .expect("a step request's material encodes")
}

/// The terminal of a process refused a round of tool steps past its
/// `max_tool_calls`: the typed refusal itself, as a cell's call settles with
/// it (FIG-4546).
fn tool_call_limit(exceeded: crate::ToolCallLimitExceeded) -> crate::ProcessOutcome {
    let mut failure = crate::session::tool_execution::tool_call_limit_failure(exceeded);
    failure.raw = Some(crate::ToolValue::untrusted_json(
        serde_json::json!({ "tool_call_limit": exceeded }),
    ));
    crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::failure(failure))
}

/// The steps `driver` holds: those admitted and not yet answered to the
/// engine.
fn step_requests(driver: &Driver) -> Vec<StepRequest> {
    driver
        .steps
        .values()
        .map(|step| step.request.clone())
        .collect()
}
