//! Source-private member kernels retain one body shadow per actual entity.
//! Only complete source statements are legal exits. Entry operands are vacant;
//! every interior result is consumed before the selected statement boundary.
use super::*;
use crate::compile::room::{
    charges::{ChargeDebt, ChargeProgram, FormulaValues},
    placement::{BindingPlacement, DenseActivation},
};
use crate::compile::{CodeId, Executable, LibId, LibRun, StmtId};
use crate::machine::SlotState;
use lash_kernel_doc::{Value, arena::ArenaHandle};
use lash_kernel_state::arena::PreparedMemberBody;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use logical::{BoundaryTransaction, Dependencies, Holding, Slot};
use observers::{ObserverScope, PrivateObservers};
mod algebra;
mod binding;
mod compilation;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Address {
    Cell(Slot),
    Field { target: usize, name: Arc<str> },
    Index { target: usize, index: usize },
}
#[derive(Clone, Debug)]
enum Operation {
    Literal(Value),
    Binding { slot: Slot, shared: bool },
    Read(Address),
    Native { lib: LibId, inputs: Box<[usize]> },
    Write { address: Address, value: usize },
}
impl Operation {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Literal(a), Self::Literal(b)) => a.same(b),
            (Self::Binding { slot: a, shared: x }, Self::Binding { slot: b, shared: y }) => {
                a == b && x == y
            }
            (Self::Read(a), Self::Read(b)) => a == b,
            (Self::Native { lib: a, inputs: x }, Self::Native { lib: b, inputs: y }) => {
                a == b && x == y
            }
            (
                Self::Write {
                    address: a,
                    value: x,
                },
                Self::Write {
                    address: b,
                    value: y,
                },
            ) => a == b && x == y,
            _ => false,
        }
    }
}
struct Node {
    event: Event,
    operation: Operation,
    inputs: Box<[usize]>,
    output: Option<Slot>,
}
struct Statement {
    id: StmtId,
    first: Point,
    through: Point,
    after: Point,
    nodes: std::ops::Range<usize>,
    charges: ChargeProgram,
    recovery: [u8; 32],
    depth: u64,
}
/// An immutable source product. Storage, arithmetic answers, fees and memory
/// totals supplied by a consumer cannot manufacture this authority.
pub(crate) struct MemberRegion {
    executable: u64,
    code: CodeId,
    placement: Arc<BindingPlacement>,
    source: Arc<Source>,
    observers: ObserverScope,
    footprint: Box<[u32]>,
    nodes: Box<[Node]>,
    statements: Box<[Statement]>,
    vacant: Box<[Slot]>,
    peaks: Box<[algebra::Polynomial]>,
}
struct Compiler<'a> {
    exe: &'a Executable,
    placement: &'a Arc<BindingPlacement>,
    nodes: Vec<Node>,
    live: BTreeMap<Owner, usize>,
    vacant: BTreeSet<Slot>,
}
pub(crate) struct BoundMembers<'p> {
    region: &'p MemberRegion,
    invocation: Invocation,
    count: usize,
    bodies: Vec<PreparedMemberBody>,
    observers: PrivateObservers,
    dependencies: Dependencies,
    debts: Vec<ChargeDebt<'p>>,
    peak: u64,
    depth: u64,
}
pub(crate) struct PreparedMembers<'p, 'a> {
    region: &'p MemberRegion,
    count: usize,
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    storage: Vec<(PreparedMemberBody, Vec<MemberEdit>)>,
    debts: Vec<ChargeDebt<'p>>,
    allowance: Allowance,
    resources: ResourceFacts,
    memory_bound: u64,
}
pub(crate) struct MemberCompletion {
    pub(crate) statements: usize,
    pub(crate) balance: logical::Balance,
}
impl<'p> BoundMembers<'p> {
    pub(crate) fn prepare<'a>(
        self,
        frontier: &Frontier,
        ledger: &Ledger,
        mut registration: Registration<'a>,
        heap: &PagedHeap,
        resources: &SourceResources<'_>,
        memory_bound: u64,
    ) -> Result<Option<PreparedMembers<'p, 'a>>, HeapFault> {
        let first = &self.region.statements[0].first;
        let last = &self.region.statements[self.count - 1];
        if !self.dependencies.is_current(ledger, registration.arena())
            || self
                .bodies
                .iter()
                .any(|body| !body.is_current(registration.arena()))
            || frontier.proven
            || !Arc::ptr_eq(&frontier.source, &self.region.source)
            || frontier.position.unpaid.as_ref() != Some(first)
            || frontier.position.occurrence.invocation != self.invocation.id()
            || self.invocation.frame()
                != (logical::Owner::Frame {
                    task: frontier.position.occurrence.task,
                    invocation: self.invocation.id(),
                })
        {
            return Ok(None);
        }
        let mut units = 0_u64;
        for (stmt, debt) in self.region.statements.iter().zip(&self.debts) {
            if !debt.matches(&self.region.source, &stmt.first, &stmt.through) {
                return Ok(None);
            }
            let Some(total) = units.checked_add(debt.units()) else {
                return Ok(None);
            };
            units = total;
        }
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units,
            close: None,
            strict: true,
            depth: self.depth,
            safe_point: resources.safe_point,
        };
        if crate::admission::Window::new(*resources.charged, resources.bound, resources.slice)
            .admit((), 0, units, None, true)
            .is_none()
            || resources
                .depth
                .checked_add(self.depth)
                .is_none_or(|depth| depth > resources.limit)
        {
            return Ok(None);
        }
        let facts = resources
            .facts(&allowance)
            .ok_or_else(|| fault("member resource facts disappeared"))?;
        let groups: Vec<_> = self.bodies.iter().map(PreparedMemberBody::edits).collect();
        let mut transaction = ledger
            .prepare_source_member_bodies_boundary(registration, heap, &groups, &[], &[])
            .map_err(binding::member_fault)?;
        if !transaction.matches_roots(ledger, &[], &[]) {
            return Ok(None);
        }
        let Some(transaction) = transaction
            .bind_dependencies(self.dependencies)?
            .require_source_room(self.peak)
        else {
            return Ok(None);
        };
        let mut transaction = self.observers.finish(transaction)?;
        if !transaction.is_current(ledger) || transaction.peak() > memory_bound {
            return Ok(None);
        }
        let storage = self
            .bodies
            .into_iter()
            .map(|body| {
                let edits = body.edits().to_vec();
                (body, edits)
            })
            .collect();
        Ok(Some(PreparedMembers {
            region: self.region,
            count: self.count,
            frontier: std::rc::Rc::clone(&frontier.identity),
            revision: frontier.revision,
            before: frontier.position.clone(),
            next: Position {
                at: last.through.clone(),
                unpaid: Some(last.after.clone()),
                recovery: last.recovery,
                occurrence: frontier.position.occurrence.clone(),
            },
            transaction,
            storage,
            debts: self.debts,
            allowance,
            resources: facts,
            memory_bound,
        }))
    }
    #[cfg(test)]
    pub(crate) fn peak(&self) -> u64 {
        self.peak
    }
}
impl<'p, 'a> PreparedMembers<'p, 'a> {
    pub(crate) fn admit<'r>(
        self,
        resources: SourceResources<'r>,
    ) -> Result<Option<SourceAccepted<'r, Self>>, HeapFault> {
        let facts = self.resources.clone();
        let allowance = self.allowance.clone();
        resources.admit_interval(&facts, &allowance, self)
    }
}
impl SourceAccepted<'_, PreparedMembers<'_, '_>> {
    pub(crate) fn publish_members(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<MemberCompletion, HeapFault> {
        self.commit(|mut prepared| {
            if !std::rc::Rc::ptr_eq(&prepared.frontier, &frontier.identity)
                || prepared.revision != frontier.revision
                || frontier.proven
                || frontier.position != prepared.before
                || !prepared.transaction.is_current(ledger)
                || !Arc::ptr_eq(&prepared.region.source, &frontier.source)
            {
                return Err(fault("private member frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("private member frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            let accepted = prepared
                .transaction
                .admit(prepared.memory_bound)
                .map_err(|_| fault("private member memory acceptance changed"))?;
            accepted.publish(ledger, |arena, edits| {
                let mut offset = 0;
                for (body, expected) in &prepared.storage {
                    let end = offset + expected.len();
                    assert!(
                        edits
                            .get(offset..end)
                            .is_some_and(|actual| body.matches_edits(actual))
                    );
                    offset = end;
                }
                assert_eq!(offset, edits.len());
                for (body, expected) in prepared.storage {
                    body.publish(arena, expected);
                }
            })?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            if let Some(tape) = tape {
                for debt in &prepared.debts {
                    debt.record(tape);
                }
            }
            Ok(MemberCompletion {
                statements: prepared.count,
                balance,
            })
        })
    }
}
