//! A compiler-owned source proof and detached storage have one affine exit.
//! Consumers never supply fee or peak totals to the publication owner.
use super::super::charges::{ChargeDebt, ChargeProgram, settled::SettledDebt};
pub(in crate::compile::room) mod schedule;
use super::*;
mod entry;
mod loop_entry;
pub(crate) use entry::CallEntry;
pub(in crate::compile::room) use entry::ScopeEntry;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::logical::{Dependencies, PublicationValue, RootValue},
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
pub(crate) use loop_entry::{LoopEntry, PaidLoopInput};
use schedule::BoundaryCharges;
use schedule::StatementSchedule;

/// Source compilers derive these from the selected call/occurrence schedule.
/// They are not runtime allowances or declarations that a prefix is paid.
#[derive(Clone, Default)]
pub(in crate::compile::room) struct BoundaryNeeds {
    pub(in crate::compile::room) depth: u64,
    pub(in crate::compile::room) occurrences: Vec<usize>,
    pub(in crate::compile::room) invocations: u64,
}

/// The source compiler has checked the actual entry, survivors, final edits,
/// native contracts, interior allocation order and complete logical envelope.
/// Keeping this constructor inside room prevents a kernel from asserting fees
/// or ownership by constructing a runtime publication capability itself.
pub(in crate::compile::room) struct BoundaryEvidence {
    pub(in crate::compile::room) invocation: Invocation,
    pub(in crate::compile::room) room: CheckedRoom,
    pub(in crate::compile::room) dependencies: Dependencies,
    pub(in crate::compile::room) inputs: Vec<logical::Owner>,
    pub(in crate::compile::room) outputs: Vec<(logical::Owner, PublicationValue)>,
    pub(in crate::compile::room) members: Vec<MemberEdit>,
}

/// One selected source path, constructed only by its owning source compiler.
/// Charge program identity, order and multiplicity remain part of that path.
pub(crate) struct CompiledPublication {
    source: Arc<Source>,
    first: Point,
    through: Point,
    unpaid: Point,
    recovery: [u8; 32],
    charges: Box<[Arc<ChargeProgram>]>,
    needs: BoundaryNeeds,
    statements: Option<StatementSchedule>,
}

/// The entry revision and proposed source exit cannot be rebound after this
/// value is issued. Abandonment changes neither roots, charges nor identities.
struct Cursor {
    source: Arc<Source>,
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
}
pub(crate) struct BoundPublication<D = Vec<SettledDebt>> {
    plan: Arc<CompiledPublication>,
    cursor: Cursor,
    evidence: BoundaryEvidence,
    debts: D,
    completion: Option<(logical::InvocationRange, u64)>,
    needs: BoundaryNeeds,
}

pub(crate) struct PreparedPublication<'a, D = Vec<SettledDebt>> {
    cursor: Cursor,
    debts: D,
    transaction: logical::BoundaryTransaction<'a>,
    memory_bound: u64,
}

impl CompiledPublication {
    /// `charges` must be the complete ordered schedule independently verified
    /// by the source compiler for this path, including its entered suffix.
    /// Compilation verifies every original close again; binding accepts only
    /// debts borrowed from these very programs, never equivalent totals.
    pub(in crate::compile::room) fn compile(
        exe: &crate::compile::Executable,
        code: crate::compile::CodeId,
        first: Point,
        through: Point,
        successor: Successor,
        charges: Vec<Arc<ChargeProgram>>,
        needs: BoundaryNeeds,
    ) -> Result<Arc<Self>, Refused> {
        let source = &exe.code(code).source_room;
        source.verify()?;
        if !source.room.events.contains_key(&first) || !source.room.events.contains_key(&through) {
            return Err(Refused::Source);
        }
        let unpaid = source.exact_successor(&through, successor)?;
        for (index, program) in charges.iter().enumerate() {
            program.verify(exe, code)?;
            if !program.belongs_to(source)
                || charges[..index]
                    .iter()
                    .any(|earlier| program.overlaps(earlier))
            {
                return Err(Refused::Source);
            }
        }
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.affine-boundary");
        first.site.encode(&mut encoding);
        through.site.encode(&mut encoding);
        unpaid.site.encode(&mut encoding);
        Ok(Arc::new(Self {
            source: Arc::clone(source),
            first,
            through,
            unpaid,
            recovery: logical_digest(&encoding.into_bytes()),
            charges: charges.into(),
            needs,
            statements: None,
        }))
    }

    pub(in crate::compile::room) fn authenticates(
        &self,
        source: &Arc<Source>,
        first: &Point,
        through: &Point,
    ) -> bool {
        Arc::ptr_eq(&self.source, source) && self.first == *first && self.through == *through
    }
    #[cfg(test)]
    pub(crate) fn accepts_debt_for_law(&self, debt: ChargeDebt<'_>) -> bool {
        self.charges[0].settle(debt).is_some()
    }
    pub(in crate::compile::room) fn charges(&self) -> &[Arc<ChargeProgram>] {
        &self.charges
    }
    pub(crate) fn start(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.first.clone(),
            unpaid: Some(self.first.clone()),
            recovery: self.recovery,
            occurrence,
        }
    }
    pub(crate) fn exit(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.through.clone(),
            unpaid: Some(self.unpaid.clone()),
            recovery: self.recovery,
            occurrence,
        }
    }

    /// Only a source verifier may seal its evaluated facts. The runtime gets
    /// an opaque proof to join with detached storage, not a numeric escape.
    #[expect(
        clippy::too_many_arguments,
        reason = "one compiler-issued source proof"
    )]
    pub(in crate::compile::room) fn bind(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        evidence: BoundaryEvidence,
        debts: Vec<ChargeDebt<'_>>,
        proposed: Position,
    ) -> Result<Option<BoundPublication>, HeapFault> {
        if self.statements.is_some() || debts.len() != self.charges.len() {
            return Ok(None);
        }
        let Some(debts) = self
            .charges
            .iter()
            .zip(debts)
            .map(|(program, debt)| program.settle(debt))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        self.bind_checked(frontier, ledger, arena, heap, evidence, debts, proposed)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one source proof and canonical debts"
    )]
    fn bind_checked<D: BoundaryCharges>(
        self: &Arc<Self>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        evidence: BoundaryEvidence,
        debts: D,
        proposed: Position,
    ) -> Result<Option<BoundPublication<D>>, HeapFault> {
        if frontier.proven
            || !Arc::ptr_eq(&self.source, &frontier.source)
            || frontier.position.unpaid.as_ref() != Some(&self.first)
            || evidence.invocation.id() != frontier.position.occurrence.invocation
            || evidence.invocation.frame()
                != (logical::Owner::Frame {
                    task: frontier.position.occurrence.task,
                    invocation: frontier.position.occurrence.invocation,
                })
            || ledger
                .invocation(
                    frontier.position.occurrence.task,
                    evidence.invocation.id(),
                    heap.pages(),
                )
                .ok()
                .as_ref()
                != Some(&evidence.invocation)
            || evidence.room.owner != self.source.owner
            || evidence.room.native
            || evidence.room.exit != ExitKind::Normal
            || !evidence.dependencies.is_current(ledger, arena)
            || proposed != self.exit(frontier.position.occurrence.clone())
        {
            return Ok(None);
        }
        Ok(Some(BoundPublication {
            plan: Arc::clone(self),
            cursor: Cursor {
                source: Arc::clone(&self.source),
                frontier: std::rc::Rc::clone(&frontier.identity),
                revision: frontier.revision,
                before: frontier.position.clone(),
                next: proposed,
            },
            evidence,
            debts,
            completion: None,
            needs: self.needs.clone(),
        }))
    }
}

impl<D: BoundaryCharges> BoundPublication<D> {
    pub(crate) fn roots(&self) -> (&[logical::Owner], &[(logical::Owner, PublicationValue)]) {
        (&self.evidence.inputs, &self.evidence.outputs)
    }
    /// The call verifier supplies its one ledger-owned reservation and the
    /// genuinely completed prefix. The source plan requires exactly that many.
    pub(in crate::compile::room) fn with_invocations(
        mut self,
        ledger: &Ledger,
        range: logical::InvocationRange,
        completed: u64,
    ) -> Option<Self> {
        if self.completion.is_some()
            || completed == 0
            || completed != self.needs.invocations
            || completed > range.len()
            || !range.is_current(ledger, &self.evidence.invocation)
        {
            return None;
        }
        self.completion = Some((range, completed));
        Some(self)
    }

    /// No journal is reconstructed here. The compiler's exact roots and final
    /// edits authenticate the external transaction; its interior room is added
    /// before the single shared resource verdict.
    pub(crate) fn prepare_external<'r, 'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        heap: &PagedHeap,
        mut transaction: logical::BoundaryTransaction<'a>,
        resources: SourceResources<'r>,
        memory_bound: u64,
    ) -> Result<Option<SourceAccepted<'r, PreparedPublication<'a, D>>>, HeapFault> {
        if !self.cursor.current(frontier)
            || !ledger.is_current(self.evidence.dependencies.ledger())
            || self.completion.as_ref().map_or(0, |(_, n)| *n) != self.needs.invocations
            || !transaction.matches_roots(
                ledger,
                &self.evidence.inputs,
                &self
                    .evidence
                    .outputs
                    .iter()
                    .map(|(owner, value)| (*owner, root(value)))
                    .collect::<Vec<_>>(),
            )
            || !transaction.matches_members(&self.evidence.members)
        {
            return Ok(None);
        }
        let Self {
            plan: _,
            cursor,
            evidence,
            debts,
            completion,
            needs,
        } = self;
        transaction = transaction.bind_dependencies(evidence.dependencies)?;
        if !transaction.is_current(ledger) {
            return Ok(None);
        }
        let before = ledger.balance()?.total()?;
        let after = transaction.balance()?.total()?;
        if i128::from(after) - i128::from(before) != evidence.room.balance.retained_delta
            || before
                .checked_add(evidence.room.balance.peak_extra)
                .is_none_or(|peak| transaction.peak() > peak)
        {
            return Ok(None);
        }
        let Some(mut transaction) =
            transaction.require_source_room(evidence.room.balance.peak_extra)
        else {
            return Ok(None);
        };
        if let Some((range, completed)) = completion {
            if !range.is_current(ledger, &evidence.invocation) {
                return Ok(None);
            }
            transaction = transaction.complete_invocations(range, completed, heap.pages())?;
        }
        if transaction.peak() > memory_bound {
            return Ok(None);
        }
        let Some(units) = debts.units() else {
            return Ok(None);
        };
        let need = Allowance {
            occurrences: needs.occurrences,
            before: 0,
            units,
            close: None,
            strict: true,
            depth: needs.depth,
            safe_point: resources.safe_point,
        };
        let Some(facts) = resources.facts(&need) else {
            return Ok(None);
        };
        resources.admit_interval(
            &facts,
            &need,
            PreparedPublication {
                cursor,
                debts,
                transaction,
                memory_bound,
            },
        )
    }
}
impl<D: BoundaryCharges> SourceAccepted<'_, PreparedPublication<'_, D>> {
    pub(crate) fn publish_boundary(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|mut prepared| {
            if !prepared.cursor.current(frontier) || !prepared.transaction.is_current(ledger) {
                return Err(fault("affine source boundary changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("source frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            prepared
                .transaction
                .admit(prepared.memory_bound)
                .map_err(|_| fault("affine source admission changed"))?
                .publish(ledger, semantic)?;
            frontier.position = prepared.cursor.next;
            frontier.revision = revision;
            if let Some(tape) = tape {
                prepared.debts.record(tape);
            }
            Ok(balance)
        })
    }
}
fn root(value: &PublicationValue) -> RootValue<'_> {
    match value {
        PublicationValue::Native(value) => RootValue::Value(value),
        PublicationValue::Prepared(value) => RootValue::Prepared(value),
        PublicationValue::Page(value) => RootValue::Page(value),
        PublicationValue::Frozen(value) => RootValue::Frozen(value),
        PublicationValue::Capture(value) => RootValue::Capture(*value),
        PublicationValue::Existing(value) => RootValue::Existing(*value),
        PublicationValue::Frame => RootValue::Frame,
        PublicationValue::Departure => RootValue::Departure,
    }
}

impl Cursor {
    fn current(&self, frontier: &Frontier) -> bool {
        !frontier.proven
            && std::rc::Rc::ptr_eq(&self.frontier, &frontier.identity)
            && self.revision == frontier.revision
            && self.before == frontier.position
            && Arc::ptr_eq(&self.source, &frontier.source)
    }
}
