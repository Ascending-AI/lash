//! How a process's steps are admitted (ADR 0132 §10): an engine's batch
//! held against the process's `max_tool_calls`, each step pinned by its
//! host's admission and recorded on the driver; and the handoff of a
//! granted tool's call (P6), whose committed resolution is consumed by the
//! commit that admits the call of its matched tool, without the engine.

use std::collections::BTreeSet;
use std::sync::Arc;

use super::activation::Claim as Owned;
use lash_core_store::tool_run::{MaterialLocation, MaterialOwner, MaterialPayload, MaterialRole};
use lash_durable::domain::{OwnerKey, ProcessActorRow, ProcessWrite, RunSeq, ScopeKey};
use lash_durable::{ActorTx, CommitLabel, DomainWrite, DurableError, DurableInstant};
use lash_protocol_types::{ToolCallAdmission, ToolCallPosition};

use super::activation::{Live, Pass, corrupt, millis, refused, unavailable};
use super::driver::{Blocked, Driver, Immediate, InFlight, Pinned, StoredWaitId};
use super::effects::record_omitted_effects;
use super::node_trace::NodeCloses;
use super::terminal::record_terminal;
use super::waiting::Ending;
use crate::runtime::actor::round::{self, AdmittedExecution, ExecutionDraft, RoundDraft, RunFold};
use crate::runtime::actor::waits;
use crate::runtime::actor::waits::WaitPurpose;
use crate::runtime::process::StepRuntime;
use crate::runtime::process::steps::StepRefusal;
use lash_execution_contracts::park::ProcessParkReason;
use lash_execution_contracts::process::EngineAction;
use lash_execution_contracts::process::ProcessInput;
use lash_execution_contracts::process::ProcessRecord;
use lash_execution_contracts::process::StepRequest;
use lash_execution_contracts::waits::ParkDeadline;
use lash_execution_contracts::waits::WaitDeadline;
use lash_execution_contracts::waits::WaitSpec;
use lash_vocabulary::identity::ProcessId;

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

/// A granted tool's call's committed resolution, and the call of the tool
/// its match names, which lash admits in its place (P6).
pub(super) struct Handoff {
    pub(super) resolution: InFlight,
    pub(super) target: StepRequest,
    pub(super) pin: crate::runtime::process::steps::InvocationTarget,
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
    Ended(lash_execution_contracts::process::ProcessOutcome),
    /// A step was refused before its admission.
    Refused(StepRefusal),
}

impl ProcessActivation {
    /// Rewrite on `tx` the requirements of the process `record` once it
    /// holds `driver`'s steps (S7): what its outstanding work owes, with the
    /// steps `fresh` admits. Every write is projected from the current records.
    pub(super) async fn rewrite_requires(
        &self,
        tx: &mut ActorTx,
        record: &ProcessRecord,
        driver: &Driver,
        fold: &RunFold,
        fresh: &[AdmittedExecution],
        live: &Live,
    ) -> Result<(), DurableError> {
        let environment = self
            .steps
            .requirements(&live.bodies.runtime, record, &step_requests(driver))
            .await?
            .unwrap_or_default();
        let mut work = outstanding_work(&record.id, environment, &live.record, driver, Some(fold));
        for execution in fresh {
            work.admit_linked(
                OwnerKey::Process(record.id.clone()),
                execution.draft().call().clone(),
                execution.draft().binding(),
                driver
                    .steps
                    .values()
                    .find(|step| step.call == *execution.draft().call())
                    .and_then(|step| step.target.as_ref())
                    .map(|target| &target.route),
            );
        }
        let requires = work.requires();
        // A lifecycle outcome may have changed the stored obligations
        // since the last engine transition; always publish this projection.
        tx.set_requires(requires);
        Ok(())
    }

    /// Consume `handoff`'s resolution, a granted tool's call's resolution
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
        owned: &Owned<'_>,
        mut tx: ActorTx,
        consumed: &Consumed<'_>,
        mut driver: Driver,
        live: &mut Live,
        handoff: Handoff,
    ) -> Result<Pass, DurableError> {
        let Handoff {
            resolution,
            target,
            pin,
        } = handoff;
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
                &live.bodies.runtime,
                &mut tx,
                process,
                record,
                &mut driver,
                vec![target],
                now,
                &mut fresh,
                Hold::Moved,
                Some(&pin),
            )
            .await?
        {
            Admission::Admitted => {}
            Admission::Refused(StepRefusal::CompositionUnavailable { reason }) => {
                let reason = ProcessParkReason::CompositionUnavailable { message: reason };
                return self.park(owned, tx, &reason).await;
            }
            Admission::Refused(StepRefusal::EnvRefused { refusal }) => {
                return self
                    .park(owned, tx, &ProcessParkReason::EnvRefused { refusal })
                    .await;
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
        super::waiting::settle(&mut tx, process, record, &driver, Ending::Resumed);
        tx.write(DomainWrite::Process(ProcessWrite::Advance {
            process: process.clone(),
            expected_rev: row.state_rev,
            driver_json: driver.encode(),
        }));
        self.record_published(&mut tx, process, row);
        self.rewrite_requires(&mut tx, record, &driver, fold, &fresh, live)
            .await?;
        if owned.cx.process_revision().await? != tx.revision() {
            return Ok(Pass::Again);
        }
        let appended =
            lash_durable::domain::RunFacts::appended(tx.domain(), tx.graph(), owned.epoch());
        let mut continuation = owned.cx.process_continuation().await?;
        let committed = owned.commit(tx, CommitLabel::PROCESS_ADVANCE).await?;
        continuation
            .committed(
                owned.store().as_ref(),
                committed.revision,
                appended,
                &driver,
            )
            .await?;
        owned
            .cx
            .enrich_process(committed.revision, continuation)
            .await?;
        live.lifecycle.admitted(&fresh);
        Ok(Pass::Again)
    }

    /// End `process` with `outcome`, which lash decided for it.
    pub(super) async fn end_with(
        &self,
        owned: &Owned<'_>,
        mut tx: ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &Driver,
        outcome: lash_execution_contracts::process::ProcessOutcome,
    ) -> Result<Pass, DurableError> {
        super::waiting::end(&mut tx, process, record, Ending::Resumed);
        record_omitted_effects(&mut tx, process, driver, self.fleet());
        record_terminal(&mut tx, process, &outcome)?;
        tx.ack_seen();
        self.commit_terminal(owned, tx, process, NodeCloses::default())
            .await?;
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
        runtime: &Arc<crate::runtime::process::StepRuntime>,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &mut Driver,
        requests: Vec<StepRequest>,
        now: DurableInstant,
        fresh: &mut Vec<AdmittedExecution>,
        hold: Hold,
        target: Option<&crate::runtime::process::steps::InvocationTarget>,
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
                Err(StepRefusal::Resolution(error)) => {
                    return Err(match error {
                        crate::runtime::actor::resolutions::ResolutionError::Durable(error) => {
                            error
                        }
                        crate::runtime::actor::resolutions::ResolutionError::Provider(fault) => {
                            DurableError::ResolutionDiscovery {
                                message: fault.message,
                            }
                        }
                        error => unavailable(error.to_string()),
                    });
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
            let admitted = match self
                .steps
                .admit(runtime, record, &request, target, millis(now))
                .await
            {
                Ok(admitted) => admitted,
                // Not a refusal: the pass did not commit, and the
                // next one asks again.
                Err(StepRefusal::Unavailable { reason, .. }) => {
                    return Err(unavailable(reason));
                }
                Err(StepRefusal::Resolution(error)) => {
                    return Err(match error {
                        crate::runtime::actor::resolutions::ResolutionError::Durable(error) => {
                            error
                        }
                        crate::runtime::actor::resolutions::ResolutionError::Provider(fault) => {
                            DurableError::ResolutionDiscovery {
                                message: fault.message,
                            }
                        }
                        error => unavailable(error.to_string()),
                    });
                }
                Err(refusal) => return Ok(Admission::Refused(refusal)),
            };
            let current = runtime.cx().process_revision().await?;
            if current != tx.revision() {
                // First-use pinning commits before invocation fitting. The
                // engine transition must be replayed over that committed revision.
                return Err(DurableError::ResolutionRevisionConflict {
                    actor: tx.actor().clone(),
                    expected: tx.revision(),
                    current,
                });
            }
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
                target: admitted.target,
                resolves: admitted.resolves,
            };
            let trace = super::node_trace::step_trace_scope(
                self.tracing.as_ref(),
                record,
                &step,
                millis(now),
            );
            drafts.push(
                ExecutionDraft::new(
                    now,
                    step.call.clone(),
                    admitted.tool,
                    step_request_material(process, &step),
                    step.policy,
                    step.limit(),
                    admitted.park,
                )
                .with_binding(admitted.binding)
                .with_trace(trace),
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
                Some(output) => round::settle(tx, member, output, Vec::new(), None)
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
fn tool_call_limit(
    exceeded: crate::ToolCallLimitExceeded,
) -> lash_execution_contracts::process::ProcessOutcome {
    let mut failure = crate::session::tool_execution::tool_call_limit_failure(exceeded);
    failure.raw = Some(crate::ToolValue::untrusted_json(
        serde_json::json!({ "tool_call_limit": exceeded }),
    ));
    lash_execution_contracts::process::ProcessAwaitOutput::from_tool_output(
        crate::ToolCallOutput::failure(failure),
    )
}

/// The steps `driver` holds: those admitted and not yet answered to the
/// engine.
pub(super) fn step_requests(driver: &Driver) -> Vec<StepRequest> {
    driver
        .steps
        .values()
        .map(|step| step.request.clone())
        .collect()
}

/// What `process`'s outstanding recorded work owes (S7): `environment`, its
/// components' configs, and the binding of each step `driver` holds, as
/// `fold`'s rounds admitted it.
pub(super) fn outstanding_work(
    process: &ProcessId,
    environment: BTreeSet<crate::ObligationToken>,
    record: &lash_vocabulary::claims::ClaimRequirements,
    driver: &Driver,
    fold: Option<&RunFold>,
) -> crate::runtime::actor::claim_obligations::RecordedWork {
    let mut work = crate::runtime::actor::claim_obligations::RecordedWork::process(
        process,
        environment,
        record,
    );
    for step in driver.steps.values() {
        let member = fold
            .and_then(|fold| fold.round(RunSeq(step.run)))
            .and_then(|round| round.members().get(usize::try_from(step.member).ok()?));
        if let Some(member) = member {
            if let Some(outcome) = member.outcome() {
                // Until handoff consumes a resolution, its committed match
                // owns the target link, even though the resolver settled.
                if step.resolves
                    && let Some(matched) = crate::runtime::process::steps::resolved_match(outcome)
                {
                    work.admit_linked(
                        OwnerKey::Process(process.clone()),
                        member.call().clone(),
                        None,
                        matched.resolved(),
                    );
                }
            } else {
                work.admit_linked(
                    OwnerKey::Process(process.clone()),
                    member.call().clone(),
                    member.draft().binding(),
                    step.target.as_ref().map(|target| &target.route),
                );
            }
        }
    }
    work
}

impl ProcessActivation {
    /// Record `action`'s admission on `tx` and its effect on `driver`, and
    /// end the waits it leaves behind as `ending` says. Answers the terminal
    /// to commit instead when the action ends the process or its admission
    /// is refused.
    #[expect(
        clippy::too_many_arguments,
        reason = "the transition and the rows its admission is recorded against"
    )]
    pub(super) async fn apply(
        &self,
        runtime: &Arc<StepRuntime>,
        tx: &mut ActorTx,
        process: &ProcessId,
        record: &ProcessRecord,
        driver: &mut Driver,
        action: EngineAction,
        ending: Ending,
        now: DurableInstant,
        fresh: &mut Vec<AdmittedExecution>,
    ) -> Result<Option<lash_execution_contracts::process::ProcessOutcome>, DurableError> {
        let ProcessInput::Engine { .. } = record.input.as_ref() else {
            return Err(corrupt("an advanced process", "it runs no engine"));
        };
        // An engine wait is bounded by the engine's own bound, fixed now.
        let deadline = |bound| ParkDeadline::admitted(bound, now).deadline();
        driver.blocked = None;
        match action {
            EngineAction::Steps {
                steps: requests,
                wake,
            } => {
                match self
                    .admit_steps(
                        runtime,
                        tx,
                        process,
                        record,
                        driver,
                        requests,
                        now,
                        fresh,
                        Hold::Counted,
                        None,
                    )
                    .await?
                {
                    Admission::Admitted => {}
                    Admission::Ended(outcome) => return Ok(Some(outcome)),
                    Admission::Refused(StepRefusal::CompositionUnavailable { reason }) => {
                        super::terminal::record_park(
                            tx,
                            &ProcessParkReason::CompositionUnavailable { message: reason },
                        );
                        return Ok(None);
                    }
                    Admission::Refused(StepRefusal::EnvRefused { refusal }) => {
                        super::terminal::record_park(
                            tx,
                            &ProcessParkReason::EnvRefused { refusal },
                        );
                        return Ok(None);
                    }
                    Admission::Refused(refusal) => {
                        return Ok(Some(refused(refusal.to_string())));
                    }
                }
                driver.blocked = wake.map(|wake| Blocked::Sleep {
                    until: wake.until.0,
                    site: wake.site,
                });
            }
            EngineAction::PinKey { name, bound } => {
                let (wait, key) = waits::pin(
                    tx,
                    WaitSpec {
                        scope: ScopeKey::Process(process.clone()),
                        purpose: WaitPurpose::EngineKey {
                            name: name.0.clone(),
                            deadline: deadline(bound).map(WaitDeadline::at),
                        },
                    },
                );
                let key = key.ok_or_else(|| corrupt("a pinned key", "no key was minted"))?;
                driver.keys.insert(
                    name.clone(),
                    Pinned {
                        wait: StoredWaitId(wait.id()),
                        key: key.as_str().to_owned(),
                    },
                );
                driver.immediate = Some(Immediate::KeyPinned { name });
            }
            EngineAction::AwaitExternal { name, site } => {
                let Some(pinned) = driver.keys.get(&name) else {
                    return Ok(Some(refused(format!("key `{}` was never pinned", name.0))));
                };
                driver.blocked = Some(Blocked::External {
                    name,
                    wait: pinned.wait,
                    site,
                });
            }
            EngineAction::AwaitProcess {
                process: target,
                bound,
                site,
            } => {
                let (wait, _) = waits::pin(
                    tx,
                    WaitSpec {
                        scope: ScopeKey::Process(process.clone()),
                        purpose: WaitPurpose::ProcessTerminal {
                            process: target.clone(),
                            deadline: deadline(bound).map(WaitDeadline::at),
                        },
                    },
                );
                driver.blocked = Some(Blocked::Process {
                    process: target,
                    wait: StoredWaitId(wait.id()),
                    site,
                });
            }
            EngineAction::Sleep { until, site } => {
                driver.blocked = Some(Blocked::Sleep {
                    until: until.0,
                    site,
                });
            }
            EngineAction::Idle => driver.blocked = Some(Blocked::Idle),
            EngineAction::Terminal(outcome) => return Ok(Some(outcome)),
        }
        super::waiting::settle(tx, process, record, driver, ending);
        Ok(None)
    }
}
