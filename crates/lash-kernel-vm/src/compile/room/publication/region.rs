//! Compiled exits settle roots, source continuation and shared charges once.
use super::*;
use crate::compile::room::{
    charges::composition::StatementDebt,
    region::{BoundScalarRegion, ScalarRegion},
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

/// A source-derived successor. No runtime producer supplies a paid event as
/// an unpaid sentinel or chooses a continuation by comparing physical PCs.
pub(crate) struct CompiledExit {
    source: Arc<Source>,
    interval: Arc<CompiledInterval>,
    first: Point,
    through: Point,
    unpaid: Point,
    recovery: [u8; 32],
    depth: u64,
}

impl CompiledExit {
    pub(crate) fn scalar(
        exe: &crate::compile::Executable,
        region: &ScalarRegion,
    ) -> Result<Self, Refused> {
        let source = region.source();
        let interval = region.interval();
        let first = interval.events().next().ok_or(Refused::Path)?.point.clone();
        let through = interval.events().last().ok_or(Refused::Path)?.point.clone();
        if interval.exit() != ExitKind::Normal || through.phase != Phase::Commit {
            return Err(Refused::Exit);
        }
        let mut depth = 0;
        for event in interval.events() {
            for measure in event.peak.measures() {
                if let Measure::CallPeak {
                    callee: CallSource::Native(id),
                    ..
                } = measure
                {
                    let lib = exe.library_lib(id).ok_or(Refused::Source)?;
                    depth = depth.max(u64::from(exe.lib(lib).operation()));
                }
            }
        }
        let unpaid = source.next_after_commit(&through.site)?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.compiled-region");
        through.site.encode(&mut encoding);
        unpaid.site.encode(&mut encoding);
        Ok(Self {
            source: Arc::clone(source),
            interval: Arc::clone(interval),
            first,
            through,
            unpaid,
            recovery: logical_digest(&encoding.into_bytes()),
            depth,
        })
    }
    pub(crate) fn start(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.first.clone(),
            unpaid: Some(self.first.clone()),
            recovery: self.recovery,
            occurrence,
        }
    }
}

/// Arena grants and canonical graph/root edits remain under the ledger owner.
/// This wrapper binds their publication to one actual source frontier.
pub(crate) struct PreparedRegion<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: logical::BoundaryTransaction<'a>,
    memory_bound: u64,
}

impl Frontier {
    #[expect(clippy::too_many_arguments, reason = "one compiled source acceptance")]
    pub(crate) fn prepare_region<'r, 'a>(
        &self,
        bound: BoundScalarRegion<'_>,
        exit: &CompiledExit,
        debt: &StatementDebt<'_>,
        resources: SourceResources<'r>,
        ledger: &Ledger,
        registration: Registration<'a>,
        heap: &PagedHeap,
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'r, PreparedRegion<'a>>>, HeapFault> {
        if self.proven
            || !Arc::ptr_eq(&self.source, &exit.source)
            || !Arc::ptr_eq(bound.region().interval(), &exit.interval)
            || self.position.unpaid.as_ref() != Some(&exit.first)
            || self.position.occurrence.invocation != bound.invocation().id()
            || !debt.matches(&exit.source, &exit.first, &exit.through)
        {
            return Ok(None);
        }
        let next = Position {
            at: exit.through.clone(),
            unpaid: Some(exit.unpaid.clone()),
            recovery: exit.recovery,
            occurrence: self.position.occurrence.clone(),
        };
        let Some(transaction) = bound.prepare_boundary(ledger, registration, heap)? else {
            return Ok(None);
        };
        if transaction.peak() > memory_bound {
            return Ok(None);
        }
        resources.admit_statement(
            debt,
            &[],
            exit.depth,
            PreparedRegion {
                frontier: std::rc::Rc::clone(&self.identity),
                revision: self.revision,
                before: self.position.clone(),
                next,
                transaction,
                memory_bound,
            },
        )
    }
}

impl SourceAccepted<'_, PreparedRegion<'_>> {
    pub(crate) fn publish_region(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(),
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|mut prepared| {
            if !std::rc::Rc::ptr_eq(&prepared.frontier, &frontier.identity)
                || prepared.revision != frontier.revision
                || frontier.proven
                || prepared.before != frontier.position
                || !prepared.transaction.is_current(ledger)
            {
                return Err(fault("compiled source frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("source frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            let accepted = prepared
                .transaction
                .admit(prepared.memory_bound)
                .map_err(|_| fault("compiled source admission changed"))?;
            accepted.publish(ledger, |_, edits| {
                assert!(edits.is_empty(), "scalar exit cannot carry a member edit");
                semantic();
            })?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            Ok(balance)
        })
    }
}
