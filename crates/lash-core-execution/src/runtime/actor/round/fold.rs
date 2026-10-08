//! The fold: an owner's run records into what each admitted execution
//! recovers to. It calls no producer and runs no body; it reads rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::tool_run::{CompletionSource, StateResolution};
use lash_durable::DurableInstant;
use lash_durable::domain::{AdmittedId, Ordinal, OwnerKey, RunRecordKind, RunRecordRow, RunSeq};

use super::records::{
    ADMIT_ORDINAL, AdmitBody, DecideBody, OutcomeBody, PresentBody, RetryBody, StartBody,
    first_start,
};
use super::{
    AdmittedExecution, ExecutionDraft, FoldRefusal, Material, PolicyView, Recovery, RunCursor,
    RunFold, SettledOutput,
};
use crate::ToolCallId;

/// Where one member's call stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberState {
    /// Its attempt `attempt` started at `start` and has no outcome.
    Started {
        /// The attempt's `x_start` ordinal.
        start: Ordinal,
        /// The attempt, from 1.
        attempt: u32,
    },
    /// Its attempt `attempt`, started at `start`, failed with `outcome` and
    /// its retry is due at `due`; the next attempt has not started.
    RetryDue {
        /// The failed attempt's `x_start` ordinal.
        start: Ordinal,
        /// The failed attempt.
        attempt: u32,
        /// How it failed, with its payload.
        outcome: SettledOutput,
        /// When the next attempt may start.
        due: DurableInstant,
    },
    /// Its attempt `attempt`, started at `start`, parked on `source`: the
    /// call ends when one of its waits does.
    Waiting {
        /// The parked attempt's `x_start` ordinal.
        start: Ordinal,
        /// The parked attempt.
        attempt: u32,
        /// What it waits on, with its pending completion's payload.
        source: Material<CompletionSource>,
    },
    /// The call's final outcome, settling the attempt started at `start`.
    Final {
        /// The settled attempt's `x_start` ordinal.
        start: Ordinal,
        /// The output, with its payload.
        outcome: SettledOutput,
    },
}

/// One member of an admission, as its records leave it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundMember {
    pub(super) draft: ExecutionDraft,
    member: u64,
    starts: Vec<Ordinal>,
    state: MemberState,
    /// The plugin-state resolutions its final outcome committed.
    committed_state: Vec<StateResolution>,
    /// Recorded attempt outcomes and the backoff before the next attempt.
    attempts: BTreeMap<u32, (SettledOutput, Option<u64>)>,
}

impl RoundMember {
    /// The draft its admission pinned.
    #[must_use]
    pub fn draft(&self) -> &ExecutionDraft {
        &self.draft
    }

    /// The call.
    #[must_use]
    pub fn call(&self) -> &ToolCallId {
        self.draft.call()
    }

    /// Its index in its admission.
    #[must_use]
    pub fn member(&self) -> u64 {
        self.member
    }

    /// Where its call stands.
    #[must_use]
    pub fn state(&self) -> &MemberState {
        &self.state
    }

    /// The `x_start` ordinal of each of its attempts, in order.
    #[must_use]
    pub fn starts(&self) -> &[Ordinal] {
        &self.starts
    }

    /// The plugin-state resolutions its final outcome committed: none
    /// before it has one.
    #[must_use]
    pub fn committed_state(&self) -> &[StateResolution] {
        &self.committed_state
    }

    /// Its final outcome, once it has one.
    #[must_use]
    pub fn outcome(&self) -> Option<&SettledOutput> {
        match &self.state {
            MemberState::Final { outcome, .. } => Some(outcome),
            MemberState::Started { .. }
            | MemberState::RetryDue { .. }
            | MemberState::Waiting { .. } => None,
        }
    }

    /// The recorded attempts in order, with their outcome and retry delay.
    /// A park is completed by its resolution at the same attempt number.
    pub fn attempts(&self) -> impl Iterator<Item = (u32, &SettledOutput, Option<u64>)> {
        self.attempts
            .iter()
            .map(|(attempt, (output, delay))| (*attempt, output, *delay))
    }

    fn current_start(&self) -> (Ordinal, u32) {
        match &self.state {
            MemberState::Started { start, attempt }
            | MemberState::RetryDue { start, attempt, .. }
            | MemberState::Waiting { start, attempt, .. } => (*start, *attempt),
            MemberState::Final { start, .. } => {
                let attempt = self
                    .starts
                    .iter()
                    .position(|ordinal| ordinal == start)
                    .map_or(1, |index| index as u32 + 1);
                (*start, attempt)
            }
        }
    }

    fn recovery(&self, current: &PolicyView) -> Recovery {
        let repeats = current.permits_repeat(self.draft.tool(), self.draft.policy());
        match &self.state {
            MemberState::Final { outcome, .. } => Recovery::Settled(outcome.clone()),
            MemberState::Waiting { source, .. } => Recovery::Waiting(source.clone()),
            MemberState::Started { start, .. } if repeats => Recovery::RerunAtOrdinal(*start),
            MemberState::Started { .. } => Recovery::Interrupt,
            MemberState::RetryDue { attempt, due, .. } if repeats => Recovery::RetryDue {
                at: *due,
                attempt: attempt + 1,
            },
            MemberState::RetryDue { outcome, .. } => Recovery::Vetoed(outcome.clone()),
        }
    }
}

/// One admission's state: its members in declared order, its cursor and its
/// presentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundView {
    owner: OwnerKey,
    run: RunSeq,
    members: Vec<RoundMember>,
    cursor: Arc<RunCursor>,
    presented: Option<Vec<ToolCallId>>,
    /// Whether an owner recorded that it exported the trace admissions the
    /// admission retained.
    trace_exported: bool,
}

impl RoundView {
    /// The owner.
    #[must_use]
    pub fn owner(&self) -> &OwnerKey {
        &self.owner
    }

    /// The run.
    #[must_use]
    pub fn run(&self) -> RunSeq {
        self.run
    }

    /// Its members, in declared order.
    #[must_use]
    pub fn members(&self) -> &[RoundMember] {
        &self.members
    }

    /// The presented calls, once its presentation is recorded.
    #[must_use]
    pub fn presented(&self) -> Option<&[ToolCallId]> {
        self.presented.as_deref()
    }

    /// The traced scopes whose admission exports the admission still owes:
    /// its members' scopes with an anchor to export, until an owner records
    /// that it exported them (`round.traced`), which discharges them
    /// (FIG-5452, FIG-5457). An owner that holds the admission without that
    /// record exports these, then records so.
    #[must_use]
    pub fn owed_trace_exports(&self) -> Vec<&lash_trace::DurableTraceScope> {
        if self.trace_exported {
            return Vec::new();
        }
        self.members
            .iter()
            .filter_map(|member| member.draft.trace())
            .filter(|scope| scope.anchor.context().is_some())
            .collect()
    }

    /// Whether every member has its final outcome.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.members.iter().all(|member| member.outcome().is_some())
    }

    /// The run's next ordinal.
    #[must_use]
    pub fn next_ordinal(&self) -> Ordinal {
        self.cursor.peek()
    }

    pub(super) fn cursor(&self) -> &Arc<RunCursor> {
        &self.cursor
    }

    /// The identity of `member`'s current attempt.
    #[must_use]
    pub fn id_of(&self, member: &RoundMember) -> AdmittedId {
        AdmittedId {
            owner: self.owner.clone(),
            run: self.run,
            ordinal: member.current_start().0,
        }
    }

    /// The execution of `member`'s current attempt, with the run's cursor.
    #[must_use]
    pub fn execution(&self, member: &RoundMember) -> AdmittedExecution {
        let (_, attempt) = member.current_start();
        AdmittedExecution::admitted(
            self.id_of(member),
            member.draft.clone(),
            member.member,
            attempt,
            Arc::clone(&self.cursor),
        )
    }

    pub(super) fn member_started_at(&self, ordinal: Ordinal) -> Option<&RoundMember> {
        self.members
            .iter()
            .find(|member| member.starts.contains(&ordinal))
    }

    pub(super) fn admitted(&self, id: &AdmittedId) -> Option<AdmittedExecution> {
        let member = self.member_started_at(id.ordinal)?;
        let attempt = member
            .starts
            .iter()
            .position(|ordinal| *ordinal == id.ordinal)
            .map_or(1, |index| index as u32 + 1);
        Some(AdmittedExecution::admitted(
            id.clone(),
            member.draft.clone(),
            member.member,
            attempt,
            Arc::clone(&self.cursor),
        ))
    }
}

fn undecodable(row: &RunRecordRow, error: &serde_json::Error) -> FoldRefusal {
    FoldRefusal::Undecodable {
        run: row.run,
        ordinal: row.ordinal,
        reason: error.to_string(),
    }
}

fn decode<T: serde::de::DeserializeOwned>(row: &RunRecordRow) -> Result<T, FoldRefusal> {
    serde_json::from_str(&row.record_json).map_err(|error| undecodable(row, &error))
}

fn out_of_order(row: &RunRecordRow) -> FoldRefusal {
    FoldRefusal::OutOfOrder {
        run: row.run,
        ordinal: row.ordinal,
    }
}

/// Fold an owner's run records into what each admitted execution recovers
/// to, under the policies `current` declares. Calls no producer.
///
/// Each run's records must take its ordinals from 0 without a gap, start
/// with its admission, and keep each call's order: an attempt starts only
/// as the admission's first or after its call's retry, and an outcome or a
/// retry settles the call's open attempt. A call has one final.
///
/// # Errors
///
/// [`FoldRefusal`] when the rows are inconsistent.
pub fn fold(rows: &[RunRecordRow], current: &PolicyView) -> Result<RunFold, FoldRefusal> {
    let mut runs: BTreeMap<RunSeq, Vec<&RunRecordRow>> = BTreeMap::new();
    for row in rows {
        runs.entry(row.run).or_default().push(row);
    }
    let mut folded = RunFold::default();
    for (run, mut records) in runs {
        records.sort_by_key(|row| row.ordinal);
        for (expected, row) in (0_u64..).zip(&records) {
            if row.ordinal.0 != expected {
                return Err(FoldRefusal::OrdinalGap {
                    run,
                    missing: Ordinal(expected),
                });
            }
        }
        let view = fold_run(run, &records)?;
        for member in &view.members {
            folded
                .recoveries
                .push((view.id_of(member), member.recovery(current)));
        }
        folded.rounds.insert(run, view);
    }
    Ok(folded)
}

fn fold_run(run: RunSeq, records: &[&RunRecordRow]) -> Result<RoundView, FoldRefusal> {
    let Some(admit) = records
        .first()
        .filter(|row| row.ordinal == ADMIT_ORDINAL && row.kind == RunRecordKind::Admit)
    else {
        return Err(FoldRefusal::OrdinalGap {
            run,
            missing: ADMIT_ORDINAL,
        });
    };
    let body: AdmitBody = decode(admit)?;
    let mut members: Vec<Option<RoundMember>> = Vec::new();
    members.resize_with(body.members.len(), || None);
    let drafts: Vec<ExecutionDraft> = body
        .members
        .iter()
        .map(super::records::AdmittedMember::draft)
        .collect::<Result<_, _>>()
        .map_err(|reason| FoldRefusal::Undecodable {
            run,
            ordinal: ADMIT_ORDINAL,
            reason: reason.to_owned(),
        })?;
    let mut presented = None;
    let mut trace_exported = false;
    for row in &records[1..] {
        match row.kind {
            RunRecordKind::Admit => return Err(out_of_order(row)),
            RunRecordKind::XStart => {
                let start: StartBody = decode(row)?;
                let index = usize::try_from(start.member)
                    .ok()
                    .filter(|index| *index < drafts.len())
                    .ok_or_else(|| out_of_order(row))?;
                let draft = &drafts[index];
                if row.call.as_ref() != Some(draft.call()) {
                    return Err(out_of_order(row));
                }
                let slot = &mut members[index];
                match slot {
                    None if start.attempt == 1 => {
                        *slot = Some(RoundMember {
                            draft: draft.clone(),
                            member: start.member,
                            starts: vec![row.ordinal],
                            state: MemberState::Started {
                                start: row.ordinal,
                                attempt: 1,
                            },
                            committed_state: Vec::new(),
                            attempts: BTreeMap::new(),
                        });
                    }
                    Some(member)
                        if matches!(
                            member.state,
                            MemberState::RetryDue { attempt, .. } if attempt + 1 == start.attempt
                        ) =>
                    {
                        member.starts.push(row.ordinal);
                        member.state = MemberState::Started {
                            start: row.ordinal,
                            attempt: start.attempt,
                        };
                    }
                    Some(_) | None => return Err(out_of_order(row)),
                }
            }
            RunRecordKind::XWait => {
                let parked: OutcomeBody = decode(row)?;
                let SettledOutput::Waiting(source) = parked.output else {
                    return Err(out_of_order(row));
                };
                let member = open_attempt(&mut members, row, parked.start)?;
                // A park settles only a started attempt, once.
                let MemberState::Started { attempt, .. } = member.state else {
                    return Err(out_of_order(row));
                };
                member.state = MemberState::Waiting {
                    start: Ordinal(parked.start),
                    attempt,
                    source,
                };
            }
            RunRecordKind::XOutcome => {
                let settled: OutcomeBody = decode(row)?;
                if matches!(settled.output, SettledOutput::Waiting(_)) {
                    return Err(out_of_order(row));
                }
                let member = open_attempt(&mut members, row, settled.start)?;
                let attempt = member.current_start().1;
                member
                    .attempts
                    .insert(attempt, (settled.output.clone(), None));
                member.state = MemberState::Final {
                    start: Ordinal(settled.start),
                    outcome: settled.output,
                };
                member.committed_state = settled.state;
            }
            RunRecordKind::Retry => {
                let retry: RetryBody = decode(row)?;
                let member = open_attempt(&mut members, row, retry.start)?;
                let MemberState::Started { attempt, .. } = member.state else {
                    return Err(out_of_order(row));
                };
                if !retry.output.may_repeat() || attempt >= member.draft.policy().max_attempts() {
                    return Err(out_of_order(row));
                }
                let suggested_delay = match &retry.output {
                    SettledOutput::Failed(failure) => failure.named().suggested_delay_ms,
                    _ => None,
                };
                let delay = member
                    .draft
                    .policy()
                    .delay_ms_for_retry(attempt - 1, suggested_delay);
                member
                    .attempts
                    .insert(attempt, (retry.output.clone(), Some(delay)));
                member.state = MemberState::RetryDue {
                    start: Ordinal(retry.start),
                    attempt,
                    outcome: retry.output,
                    due: DurableInstant(retry.due_at_ms),
                };
            }
            RunRecordKind::Present => {
                let present: PresentBody = decode(row)?;
                if presented.is_some() {
                    return Err(out_of_order(row));
                }
                presented = Some(present.calls);
            }
            // A coordinator decision is the tool round's to fold; the
            // primitive's recoveries do not depend on it.
            RunRecordKind::Decide => match decode(row)? {
                DecideBody::TraceExported => trace_exported = true,
            },
        }
    }
    // The admission commits every member's first start with it, so a
    // member without one is a record the rows lost.
    let members = (0_u64..)
        .zip(members)
        .map(|(index, member)| {
            member.ok_or(FoldRefusal::OrdinalGap {
                run,
                missing: first_start(index),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(RoundView {
        owner: admit.owner.clone(),
        run,
        members,
        cursor: RunCursor::at(Ordinal(records.len() as u64)),
        presented,
        trace_exported,
    })
}

/// The member whose open attempt started at `start` and is named by `row`'s
/// call: the one an outcome or a retry settles.
fn open_attempt<'m>(
    members: &'m mut [Option<RoundMember>],
    row: &RunRecordRow,
    start: u64,
) -> Result<&'m mut RoundMember, FoldRefusal> {
    let call = row.call.as_ref().ok_or_else(|| out_of_order(row))?;
    let member = members
        .iter_mut()
        .flatten()
        .find(|member| member.call() == call)
        .ok_or_else(|| out_of_order(row))?;
    match member.state {
        MemberState::Final { .. } if row.kind == RunRecordKind::XOutcome => {
            Err(FoldRefusal::SecondFinal(call.clone()))
        }
        MemberState::Started { start: open, .. } if open.0 == start => Ok(member),
        // A call whose retry is due may end without its next attempt: a
        // cancel or a veto settles it at the attempt that failed.
        MemberState::RetryDue { start: failed, .. }
            if failed.0 == start && row.kind == RunRecordKind::XOutcome =>
        {
            Ok(member)
        }
        // A parked call ends at the attempt that parked it.
        MemberState::Waiting { start: parked, .. }
            if parked.0 == start && row.kind == RunRecordKind::XOutcome =>
        {
            Ok(member)
        }
        MemberState::Started { .. }
        | MemberState::RetryDue { .. }
        | MemberState::Waiting { .. }
        | MemberState::Final { .. } => Err(out_of_order(row)),
    }
}
