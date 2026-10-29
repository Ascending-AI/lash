//! The fold: an owner's run records into what each admitted execution
//! recovers to. It calls no producer and runs no body; it reads rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core_store::tool_run::{CompletionSource, StateResolution};
use lash_durable::DurableInstant;
use lash_durable::domain::{
    AdmittedId, Ordinal, OwnerKey, RunFactKind, RunFactRow, RunFacts, RunSeq,
};

use super::records::{self, ADMIT_ORDINAL, Record, StagedRecord, first_start};
use super::{
    AdmittedExecution, ExecutionDraft, FoldRefusal, PinnedWaits, PolicyView, Recovery, RunCursor,
    RunFold,
};
use crate::ToolCallId;
use lash_core_store::tool_run::Material;
use lash_core_store::tool_run::SettledOutput;

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
    /// When its final outcome's commit retained it.
    ended_at_ms: Option<u64>,
    cancellation: Option<lash_protocol_types::ToolCancellation>,
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

    /// The member's end: the store's clock at the commit that retained its
    /// final outcome, once it has one. Its tool span ends here.
    #[must_use]
    pub fn ended_at_ms(&self) -> Option<u64> {
        self.ended_at_ms
    }

    /// The immutable cancellation decision, including its source, if cancel won.
    /// Its final outcome remains absent until cancellation has been discharged.
    #[must_use]
    pub fn cancellation(&self) -> Option<&lash_protocol_types::ToolCancellation> {
        self.cancellation.as_ref()
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
        let reruns = current.reruns_interrupted(self.draft.tool(), self.draft.policy());
        match &self.state {
            MemberState::Final { outcome, .. } => Recovery::Settled(outcome.clone()),
            MemberState::Waiting { source, .. } => Recovery::Waiting(source.clone()),
            MemberState::Started { start, .. } if reruns => Recovery::RerunAtOrdinal(*start),
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
    presented: bool,
    reads: Vec<super::ModelReadPlan>,
    read_facts: Vec<lash_core_store::checkpoint_graph::NodeId>,
}

impl RoundView {
    pub fn reads(&self) -> &[super::ModelReadPlan] {
        &self.reads
    }
    pub fn read_facts(&self) -> &[lash_core_store::checkpoint_graph::NodeId] {
        &self.read_facts
    }

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
    pub fn presented(&self) -> Option<Vec<ToolCallId>> {
        self.presented.then(|| {
            self.members
                .iter()
                .map(|member| member.call().clone())
                .collect()
        })
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

    /// The identity `member`'s admission minted: the first start its
    /// admission committed. Every attempt of it resolves to this one
    /// ([`RunFold::recovery`]).
    #[must_use]
    pub fn admitted_id(&self, member: &RoundMember) -> AdmittedId {
        AdmittedId {
            owner: self.owner.clone(),
            run: self.run,
            ordinal: first_start(member.member),
        }
    }

    /// The identity of `member`'s current attempt: the execution that runs
    /// or settles it.
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

/// An activation's owned facts and checked records. Repeated folds borrow
/// the checked records and share their immutable payload images. Appending
/// facts preserves earlier images; a reload constructs a new set of records.
pub struct RunRecords {
    facts: RunFacts,
    decoded: BTreeMap<(RunSeq, Ordinal), Arc<Record>>,
}

impl RunRecords {
    /// Start reading one protected snapshot of an owner's facts.
    #[must_use]
    pub fn new(facts: RunFacts) -> Self {
        Self {
            facts,
            decoded: BTreeMap::new(),
        }
    }

    /// The immutable snapshot these records read.
    #[must_use]
    pub fn facts(&self) -> &RunFacts {
        &self.facts
    }

    /// Include appended facts and their immutable nodes, each row whose
    /// node `staged` names as the record staged there: an acknowledged
    /// commit's own records ([`Committed::staged`](super::lifecycle::Committed::staged))
    /// are not read and checked again.
    pub fn extend(&mut self, appended: RunFacts, staged: Vec<StagedRecord>) {
        // Normally appended nodes are new or byte-identical. A conflicting
        // image invalidates every checked record, including its descendants:
        // the next fold must encounter the decoder's integrity refusal.
        if appended.nodes.iter().any(|(id, node)| {
            self.facts
                .nodes
                .get(id)
                .is_some_and(|previous| previous != node)
        }) {
            self.decoded.clear();
        } else {
            let mut held: BTreeMap<_, _> = staged
                .into_iter()
                .filter_map(|staged| staged.record.map(|record| (staged.node, record)))
                .collect();
            for row in &appended.rows {
                if let Some(record) = held.remove(&row.record)
                    && record.kind() == row.kind
                {
                    self.decoded.entry((row.run, row.ordinal)).or_insert(record);
                }
            }
        }
        self.facts.extend(appended);
    }

    /// Fold the records under the current policies and pinned waits.
    ///
    /// # Errors
    /// The same record and ordering refusals as [`fold`].
    pub fn fold(
        &mut self,
        current: &PolicyView,
        waits: &PinnedWaits,
    ) -> Result<RunFold, FoldRefusal> {
        let facts = &self.facts;
        fold_records(facts, current, waits, &mut |row| {
            let record = match self.decoded.entry((row.run, row.ordinal)) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(decode(facts, row)?)
                }
            };
            Ok(Arc::clone(record))
        })
    }
}

fn decode(facts: &RunFacts, row: &RunFactRow) -> Result<Arc<Record>, FoldRefusal> {
    records::read(facts, row)
        .map(Arc::new)
        .map_err(|reason| FoldRefusal::Undecodable {
            run: row.run,
            ordinal: row.ordinal,
            reason,
        })
}

fn out_of_order(row: &RunFactRow) -> FoldRefusal {
    FoldRefusal::OutOfOrder {
        run: row.run,
        ordinal: row.ordinal,
    }
}

/// Fold an owner's run records into what each admitted execution recovers
/// to, under the policies `current` declares. Calls no producer. A member
/// that may park takes its park from its completion wait's row in `waits`
/// ([`PinnedWaits::read`] of the same rows).
///
/// Each run's records must take its ordinals from 0 without a gap, start
/// with its admission, and keep each call's order: an attempt starts only
/// as the admission's first or after its call's retry, and an outcome or a
/// retry settles the call's open attempt. A call has one final.
///
/// # Errors
///
/// [`FoldRefusal`] when the rows are inconsistent, or an admission's pinned
/// wait is missing from `waits` or is not its call's.
pub fn fold(
    facts: &RunFacts,
    current: &PolicyView,
    waits: &PinnedWaits,
) -> Result<RunFold, FoldRefusal> {
    fold_records(facts, current, waits, &mut |row| decode(facts, row))
}

type ReadRecord<'a> = dyn FnMut(&RunFactRow) -> Result<Arc<Record>, FoldRefusal> + 'a;

fn fold_records(
    facts: &RunFacts,
    current: &PolicyView,
    waits: &PinnedWaits,
    read: &mut ReadRecord<'_>,
) -> Result<RunFold, FoldRefusal> {
    let mut runs: BTreeMap<RunSeq, Vec<&RunFactRow>> = BTreeMap::new();
    for row in &facts.rows {
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
        if records
            .first()
            .is_some_and(|row| row.kind == RunFactKind::CellReport)
        {
            let report = fold_report(facts, &records, read)?;
            folded.reports.insert(run, report);
            continue;
        }
        let view = fold_run(facts, run, &records, waits, read)?;
        for member in &view.members {
            folded
                .recoveries
                .push((view.admitted_id(member), member.recovery(current)));
        }
        folded.rounds.insert(run, view);
    }
    Ok(folded)
}

/// A cell's report: its run's one fact.
fn fold_report(
    facts: &RunFacts,
    records: &[&RunFactRow],
    read: &mut ReadRecord<'_>,
) -> Result<lash_core_store::checkpoint_graph::NodeId, FoldRefusal> {
    let [row] = records else {
        return Err(out_of_order(records[1]));
    };
    let record = read(row)?;
    let Record::CellReport(report) = record.as_ref() else {
        return Err(out_of_order(row));
    };
    super::graph::ReportView::load(
        &mut lash_core_store::checkpoint_graph::GraphDecoder::new(facts, facts.read_limits),
        *report,
    )
    .map(|_| *report)
    .map_err(|refusal| FoldRefusal::Undecodable {
        run: row.run,
        ordinal: row.ordinal,
        reason: refusal.to_string(),
    })
}

fn fold_run(
    facts: &RunFacts,
    run: RunSeq,
    records: &[&RunFactRow],
    waits: &PinnedWaits,
    read: &mut ReadRecord<'_>,
) -> Result<RoundView, FoldRefusal> {
    let Some(admit) = records
        .first()
        .filter(|row| row.ordinal == ADMIT_ORDINAL && row.kind == RunFactKind::Admit)
    else {
        return Err(FoldRefusal::OrdinalGap {
            run,
            missing: ADMIT_ORDINAL,
        });
    };
    let record = read(admit)?;
    let Record::Admit(body) = record.as_ref() else {
        return Err(out_of_order(admit));
    };
    let corrupt_plan = |reason: &str| FoldRefusal::Undecodable {
        run,
        ordinal: ADMIT_ORDINAL,
        reason: reason.into(),
    };
    let mut identities: std::collections::BTreeSet<_> = if body.reads.is_empty() {
        std::collections::BTreeSet::new()
    } else {
        body.members
            .iter()
            .map(|member| member.call.clone())
            .collect()
    };
    let mut previous = None;
    for (index, plan) in body.reads.iter().enumerate() {
        if plan.ordinal != index as u64 + 1
            || plan.slot >= (body.members.len() + body.reads.len()) as u64
            || previous.is_some_and(|slot| slot >= plan.slot)
            || plan.metadata().call_id() != plan.call_id
            || !matches!(plan.caller, crate::ToolReadCaller::ModelRound { .. })
            || !identities.insert(plan.call_id.clone())
        {
            return Err(corrupt_plan(
                "a saved model read plan has inconsistent identities or slots",
            ));
        }
        previous = Some(plan.slot);
    }
    let mut members: Vec<Option<RoundMember>> = Vec::new();
    members.resize_with(body.members.len(), || None);
    let drafts: Vec<ExecutionDraft> = body
        .members
        .iter()
        .map(|member| member.draft(&admit.owner, waits))
        .collect::<Result<_, _>>()
        .map_err(|reason| FoldRefusal::Undecodable {
            run,
            ordinal: ADMIT_ORDINAL,
            reason: reason.to_owned(),
        })?;
    let mut presented = false;
    let mut read_facts = Vec::new();
    for row in &records[1..] {
        let record = read(row)?;
        match record.as_ref() {
            Record::Admit(_) | Record::CellReport(_) => return Err(out_of_order(row)),
            Record::Start(start) => {
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
                    // The admission commits each member's first start at
                    // its place: the identity every attempt resolves to.
                    None if start.attempt == 1 && row.ordinal == first_start(start.member) => {
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
                            ended_at_ms: None,
                            cancellation: None,
                        });
                    }
                    Some(member)
                        if matches!(
                            member.state,
                            MemberState::RetryDue { attempt, .. } if member.cancellation.is_none() && attempt + 1 == start.attempt
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
            Record::Wait(parked) => {
                let SettledOutput::Waiting(source) = &parked.output else {
                    return Err(out_of_order(row));
                };
                let member = open_attempt(&mut members, row, parked.start)?;
                if member.cancellation.is_some() {
                    return Err(out_of_order(row));
                }
                // A park settles only a started attempt, once.
                let MemberState::Started { attempt, .. } = member.state else {
                    return Err(out_of_order(row));
                };
                member.state = MemberState::Waiting {
                    start: Ordinal(parked.start),
                    attempt,
                    source: source.clone(),
                };
            }
            Record::Outcome(settled) => {
                if matches!(settled.output, SettledOutput::Waiting(_)) {
                    return Err(out_of_order(row));
                }
                let member = open_attempt(&mut members, row, settled.start)?;
                if let Some(cancellation) = &member.cancellation
                    && !matches!(&settled.output, SettledOutput::Cancelled { cancellation: actual, .. } if actual == cancellation)
                {
                    return Err(out_of_order(row));
                }
                let attempt = member.current_start().1;
                member
                    .attempts
                    .insert(attempt, (settled.output.clone(), None));
                member.state = MemberState::Final {
                    start: Ordinal(settled.start),
                    outcome: settled.output.clone(),
                };
                member.committed_state = settled.state.clone();
                member.ended_at_ms = settled.ended_at_ms;
            }
            Record::Retry(retry) => {
                let member = open_attempt(&mut members, row, retry.start)?;
                let MemberState::Started { attempt, .. } = member.state else {
                    return Err(out_of_order(row));
                };
                if member.cancellation.is_some() {
                    return Err(out_of_order(row));
                }
                // Only a bounded retry writes one, within its bound.
                let Some(bounded) = member.draft.policy().retry() else {
                    return Err(out_of_order(row));
                };
                if !retry.output.may_repeat() || attempt >= bounded.max_attempts.get() {
                    return Err(out_of_order(row));
                }
                let suggested_delay = match &retry.output {
                    SettledOutput::Failed(failure) => failure.named().suggested_delay_ms,
                    _ => None,
                };
                let delay = bounded.delay_ms(attempt - 1, suggested_delay);
                member
                    .attempts
                    .insert(attempt, (retry.output.clone(), Some(delay)));
                member.state = MemberState::RetryDue {
                    start: Ordinal(retry.start),
                    attempt,
                    outcome: retry.output.clone(),
                    due: DurableInstant(retry.due_at_ms),
                };
            }
            Record::Cancel {
                start,
                cancellation,
            } => {
                let member = open_attempt(&mut members, row, *start)?;
                if member.cancellation.replace(cancellation.clone()).is_some() {
                    return Err(out_of_order(row));
                }
            }
            Record::Present(reads) => {
                if reads.len() != body.reads.len() {
                    return Err(out_of_order(row));
                }
                let mut decoder =
                    lash_core_store::checkpoint_graph::GraphDecoder::new(facts, facts.read_limits);
                for (plan, node) in body.reads.iter().zip(reads) {
                    let fact = super::graph::load_model_read(&mut decoder, *node)
                        .map_err(|error| corrupt_plan(&error.to_string()))?
                        .fact;
                    if fact.call_id != plan.call_id
                        || fact.ordinal != plan.ordinal
                        || fact.caller != plan.caller
                        || fact.provider != plan.provider
                        || fact.tool != plan.definition.manifest.id.as_str()
                    {
                        return Err(corrupt_plan(
                            "a saved model read fact disagrees with its plan",
                        ));
                    }
                }
                read_facts = reads.clone();
                if presented
                    || row.call.is_some()
                    || members.iter().any(|member| {
                        member
                            .as_ref()
                            .is_none_or(|member| member.outcome().is_none())
                    })
                {
                    return Err(out_of_order(row));
                }
                presented = true;
            }
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
        reads: body.reads.clone(),
        read_facts,
    })
}

/// The member whose open attempt started at `start` and is named by `row`'s
/// call: the one an outcome or a retry settles.
fn open_attempt<'m>(
    members: &'m mut [Option<RoundMember>],
    row: &RunFactRow,
    start: u64,
) -> Result<&'m mut RoundMember, FoldRefusal> {
    let call = row.call.as_ref().ok_or_else(|| out_of_order(row))?;
    let member = members
        .iter_mut()
        .flatten()
        .find(|member| member.call() == call)
        .ok_or_else(|| out_of_order(row))?;
    match member.state {
        MemberState::Final { .. }
            if matches!(row.kind, RunFactKind::XOutcome | RunFactKind::XCancel) =>
        {
            Err(FoldRefusal::SecondFinal(call.clone()))
        }
        MemberState::Started { start: open, .. } if open.0 == start => Ok(member),
        // A call whose retry is due may end without its next attempt: a
        // cancel or a veto settles it at the attempt that failed.
        MemberState::RetryDue { start: failed, .. }
            if failed.0 == start
                && matches!(row.kind, RunFactKind::XOutcome | RunFactKind::XCancel) =>
        {
            Ok(member)
        }
        // A parked call ends at the attempt that parked it.
        MemberState::Waiting { start: parked, .. }
            if parked.0 == start
                && matches!(row.kind, RunFactKind::XOutcome | RunFactKind::XCancel) =>
        {
            Ok(member)
        }
        MemberState::Started { .. }
        | MemberState::RetryDue { .. }
        | MemberState::Waiting { .. }
        | MemberState::Final { .. } => Err(out_of_order(row)),
    }
}
