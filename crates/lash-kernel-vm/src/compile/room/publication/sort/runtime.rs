//! The original caller proof and genuine call prefix share one affine bind.
use super::*;
use crate::admission::{SourceAccepted, SourceResources};
use crate::compile::room::publication::boundary::{
    BoundaryEvidence, PreparedPublication, schedule::StatementCharges,
};
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::{
    arena::PreparedListWrite,
    kernel_internals::{
        PagedHeap,
        logical::{Member, MemberEdit, MemberFault, PublicationValue, RootValue},
    },
};

pub(crate) struct CallbackPublication<'arena, 'resources, 'plan> {
    pub(crate) accepted:
        SourceAccepted<'resources, PreparedPublication<'arena, StatementCharges<'plan>>>,
    pub(crate) storage: CallbackStorage,
    pub(crate) values: Vec<(Slot, Value)>,
    pub(crate) clears: Vec<Slot>,
}
pub(crate) struct CallbackStorage(Option<PreparedListWrite>);
impl CallbackStorage {
    pub(crate) fn publish(self, arena: &mut Arena, mut edits: Vec<MemberEdit>) {
        match self.0 {
            Some(member) => {
                assert_eq!(edits.len(), 1);
                member.publish(arena, edits.pop().unwrap_or_else(|| unreachable!()));
            }
            None => assert!(edits.is_empty()),
        }
    }
}
impl CallerProgram {
    #[expect(
        clippy::too_many_arguments,
        reason = "one actual source activation and affine callback scope"
    )]
    pub(crate) fn prepare<'plan, 'arena, 'resources>(
        &'plan self,
        callback: &'plan CallbackProgram,
        exe: &Executable,
        target: CallbackTarget,
        expected: &[Value],
        frontier: &Frontier,
        activation: &DenseActivation<'_>,
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &'arena mut Arena,
        resources: SourceResources<'resources>,
        memory_bound: u64,
    ) -> Result<Option<CallbackPublication<'arena, 'resources, 'plan>>, HeapFault> {
        if !self.route.interval.matches(
            exe,
            callback.caller_code(),
            std::slice::from_ref(callback.call()),
        ) {
            return Ok(None);
        }
        let Some(width) = callback.call.reservation_width().filter(|width| *width > 0) else {
            return Ok(None);
        };
        let Some(range) = ledger.reserve_invocations(frontier.position().occurrence.task, width)
        else {
            return Ok(None);
        };
        let Some(scope) = callback.bind(
            exe, frontier, ledger, arena, invocation, activation, range, target,
        ) else {
            return Ok(None);
        };
        let evaluation = CallbackEvaluation {
            program: callback,
            exe,
            scope: &scope,
            candidates: RefCell::new(Vec::new()),
            expected,
        };
        let Some(facts) = self.proof.evaluate(CallerContext {
            exe,
            activation,
            ledger,
            heap,
            arena,
            invocation,
            callback: &evaluation,
        })?
        else {
            return Ok(None);
        };
        let candidates = evaluation.finish();
        let Some(prefix) = scope.consume_prefix(arena, candidates)? else {
            return Ok(None);
        };
        let Some(work) = self.billing.bind(&facts.formulas, arena) else {
            return Ok(None);
        };
        if facts.members.len() > 1 {
            return Ok(None);
        }
        let member = match facts.members.first() {
            Some(MemberEdit {
                handle,
                member: Member::List(index),
                next: Some(value),
            }) => match arena.prepare_list_write(*handle, *index, *value)? {
                Some(write) => Some(write),
                None => return Ok(None),
            },
            Some(_) => return Ok(None),
            None => None,
        };
        let proposed = self
            .billing
            .plan
            .exit(frontier.position().occurrence.clone());
        let Some(bound) = self.billing.plan.bind_statements(
            frontier,
            ledger,
            arena,
            heap,
            BoundaryEvidence {
                invocation: invocation.clone(),
                room: facts.room,
                dependencies: facts.dependencies,
                inputs: facts.inputs.clone(),
                outputs: facts.outputs.clone(),
                members: facts.members.clone(),
            },
            work,
            Some(prefix),
            proposed,
        )?
        else {
            return Ok(None);
        };
        let outputs: Vec<_> = facts
            .outputs
            .iter()
            .map(|(owner, value)| match value {
                PublicationValue::Native(value) => Some((*owner, RootValue::Value(value))),
                PublicationValue::Existing(owner_from) => {
                    Some((*owner, RootValue::Existing(*owner_from)))
                }
                _ => None,
            })
            .collect::<Option<_>>()
            .unwrap_or_default();
        if outputs.len() != facts.outputs.len() {
            return Ok(None);
        }
        let registration = arena.registration().map_err(|error| match error {
            doc::NativeError::Host(error) => error,
            error => HeapFault::Malformed(error.to_string()),
        })?;
        let transaction = match ledger.prepare_source_members_boundary(
            registration,
            heap,
            &facts.members,
            &facts.inputs,
            &outputs,
        ) {
            Ok(transaction) => transaction,
            Err(MemberFault::Heap(error) | MemberFault::Native(doc::NativeError::Host(error))) => {
                return Err(error);
            }
            Err(_) => return Ok(None),
        };
        // This grammar creates no mutable entity. A collected child cannot
        // be priced as a local occurrence and silently removed from room.
        if transaction.balance()?.entities != ledger.balance()?.entities {
            return Ok(None);
        }
        let Some(accepted) =
            bound.prepare_external(frontier, ledger, heap, transaction, resources, memory_bound)?
        else {
            return Ok(None);
        };
        Ok(Some(CallbackPublication {
            accepted,
            storage: CallbackStorage(member),
            values: facts.values,
            clears: facts.clears,
        }))
    }
}
