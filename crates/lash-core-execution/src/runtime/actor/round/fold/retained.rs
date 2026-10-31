//! Checked records and acknowledged incremental reductions.

use super::*;

/// One owner's checked records and retained per-run reductions. Appends are
/// applied once; policy projection never decodes or reduces earlier records.
#[derive(Clone)]
pub struct RunRecords {
    state: Arc<RecordsState>,
    projected: Option<(PolicyView, RunFold)>,
}

#[derive(Clone)]
struct RecordsState {
    facts: RunFacts,
    decoded: BTreeMap<(RunSeq, Ordinal), Arc<Record>>,
    pending: BTreeMap<RunSeq, Vec<RunFactRow>>,
    reduced: BTreeMap<RunSeq, ReducedRun>,
    views: RunFold,
    failure: Option<FoldRefusal>,
}

#[derive(Clone)]
enum ReducedRun {
    Round(RoundReduction),
    Report(lash_core_store::checkpoint_graph::NodeId),
}

impl RunRecords {
    #[must_use]
    /// Retain one protected snapshot of an owner's active facts.
    pub fn new(facts: RunFacts) -> Self {
        let mut state = RecordsState {
            facts,
            decoded: BTreeMap::new(),
            pending: BTreeMap::new(),
            reduced: BTreeMap::new(),
            views: RunFold::default(),
            failure: None,
        };
        state.reset();
        Self {
            state: Arc::new(state),
            projected: None,
        }
    }

    #[must_use]
    pub fn facts(&self) -> &RunFacts {
        &self.state.facts
    }

    /// Adopt acknowledged appends. Conflicting images invalidate their checked
    /// descendants and every reduction built from them, as on a cold read.
    pub fn extend(&mut self, appended: RunFacts, staged: Vec<StagedRecord>) {
        self.projected = None;
        let state = Arc::make_mut(&mut self.state);
        let conflicting = appended.nodes.iter().any(|(id, node)| {
            state
                .facts
                .nodes
                .get(id)
                .is_some_and(|previous| previous != node)
        });
        if conflicting {
            state.decoded.clear();
        } else {
            let mut held: BTreeMap<_, _> = staged
                .into_iter()
                .filter_map(|staged| staged.record.map(|record| (staged.node, record)))
                .collect();
            for row in &appended.rows {
                if let Some(record) = held.remove(&row.record)
                    && record.kind() == row.kind
                {
                    state
                        .decoded
                        .entry((row.run, row.ordinal))
                        .or_insert(record);
                }
            }
        }
        for row in &appended.rows {
            state.pending.entry(row.run).or_default().push(row.clone());
        }
        state.facts.extend(appended);
        if conflicting || state.failure.is_some() {
            state.reset();
        }
    }

    /// Project recovery under the current declarations after reducing new facts.
    ///
    /// # Errors
    /// The same record, ordinal, wait and second-final refusals as a cold fold.
    pub fn fold(
        &mut self,
        current: &PolicyView,
        waits: &PinnedWaits,
    ) -> Result<RunFold, FoldRefusal> {
        if let Some(error) = &self.state.failure {
            return Err(error.clone());
        }
        if !self.state.pending.is_empty() {
            self.projected = None;
            let state = Arc::make_mut(&mut self.state);
            if let Err(error) = state.reduce(waits) {
                state.failure = Some(error.clone());
                return Err(error);
            }
        }
        if let Some((policies, folded)) = &self.projected
            && policies == current
        {
            return Ok(folded.fresh_cursors());
        }
        let mut folded = self.state.views.clone();
        folded.recoveries = Arc::new(
            folded
                .rounds
                .values()
                .flat_map(|view| {
                    view.members
                        .iter()
                        .map(|member| (view.admitted_id(member), member.recovery(current)))
                })
                .collect(),
        );
        let projected = folded.fresh_cursors();
        self.projected = Some((current.clone(), folded));
        Ok(projected)
    }
}

impl RecordsState {
    fn reset(&mut self) {
        self.pending.clear();
        for row in &self.facts.rows {
            self.pending.entry(row.run).or_default().push(row.clone());
        }
        self.reduced.clear();
        self.views = RunFold::default();
        self.failure = None;
    }

    fn reduce(&mut self, waits: &PinnedWaits) -> Result<(), FoldRefusal> {
        for (run, mut rows) in std::mem::take(&mut self.pending) {
            rows.sort_by_key(|row| row.ordinal);
            let next = match self.reduced.get(&run) {
                Some(ReducedRun::Round(round)) => round.next,
                Some(ReducedRun::Report(_)) => 1,
                None => 0,
            };
            for (expected, row) in (next..).zip(&rows) {
                if row.ordinal.0 != expected {
                    return Err(FoldRefusal::OrdinalGap {
                        run,
                        missing: Ordinal(expected.min(row.ordinal.0.saturating_add(1))),
                    });
                }
            }
            for row in rows {
                let record = match self.decoded.entry((row.run, row.ordinal)) {
                    std::collections::btree_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        Arc::clone(entry.insert(decode(&self.facts, &row)?))
                    }
                };
                match self.reduced.entry(run) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        let reduced = if row.kind == RunFactKind::CellReport {
                            ReducedRun::Report(fold_report(&self.facts, &[&row], &mut |_| {
                                Ok(Arc::clone(&record))
                            })?)
                        } else {
                            ReducedRun::Round(RoundReduction::new(run, &row, record, waits)?)
                        };
                        entry.insert(reduced);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        match entry.get_mut() {
                            ReducedRun::Round(round) => {
                                round.apply(&self.facts, &row, record.as_ref())?
                            }
                            ReducedRun::Report(_) => return Err(out_of_order(&row)),
                        }
                    }
                }
            }
            match self.reduced.get(&run) {
                Some(ReducedRun::Round(round)) => {
                    self.views.rounds.insert(run, round.view()?);
                }
                Some(ReducedRun::Report(report)) => {
                    self.views.reports.insert(run, *report);
                }
                None => {}
            }
        }
        Ok(())
    }
}

impl RunFold {
    pub(crate) fn fresh_cursors(&self) -> Self {
        let mut folded = self.clone();
        for view in folded.rounds.values_mut() {
            view.fresh_cursor();
        }
        folded
    }

    pub(crate) fn extend(&mut self, other: Self) {
        if self.rounds.is_empty() && self.reports.is_empty() {
            *self = other;
            return;
        }
        Arc::make_mut(&mut self.recoveries).extend(other.recoveries.iter().cloned());
        self.rounds.extend(other.rounds);
        self.reports.extend(other.reports);
    }
}
