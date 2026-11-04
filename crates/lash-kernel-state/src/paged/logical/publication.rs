//! A publication owns its unpublished identity grants until ledger commit.
//! No arena borrow or grant guard escapes a prepared or accepted publication.
use super::*;
use crate::arena::{Arena, Registration};
use lash_kernel_doc::{Value, arena::ArenaHandle};
mod roots;
pub(super) use roots::RootProof;

/// The canonical storage owner receives exactly the edit whose old content
/// and replacement were checked. It reserves fallible storage before publish.
#[derive(Clone, Debug)]
pub struct MemberEdit {
    pub handle: ArenaHandle,
    pub member: Member,
    pub next: Option<Value>,
}
/// One canonical arena/ledger boundary. It contains surviving roots and
/// actual graph edits, not a record of each interior scalar lifetime.
/// Registration and dependency authentication remain owned until commit.
pub struct BoundaryTransaction<'a> {
    registration: Registration<'a>,
    dependencies: Dependencies,
    prepared: Prepared,
    members: Vec<MemberEdit>,
    roots: RootProof,
}
pub struct AcceptedBoundary<'a>(BoundaryTransaction<'a>);

impl Ledger {
    /// Prepare the live-out transfer of a compiled source prefix. Resident and
    /// cold values use the same source projection and membership rules as an
    /// exact operation. The compiler separately proves interior lifetimes.
    pub fn prepare_source_boundary<'a>(
        &self,
        mut registration: Registration<'a>,
        heap: &super::super::heap_impl::PagedHeap,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<BoundaryTransaction<'a>, HeapFault> {
        let prepared =
            self.prepare_source_ownership(registration.arena(), heap, inputs, outputs)?;
        let dependencies = projection_dependencies(self, registration.arena(), &prepared, outputs)?;
        Ok(BoundaryTransaction {
            registration,
            dependencies,
            prepared,
            members: Vec::new(),
            roots: RootProof::new(inputs, outputs),
        })
    }
    pub fn prepare_enter_publication<'a>(
        &self,
        task: TaskId,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        inputs: &[Owner],
        bindings: &[(Slot, RootValue<'_>)],
    ) -> Result<(Invocation, BoundaryTransaction<'a>), HeapFault> {
        let (invocation, prepared) =
            self.prepare_enter(task, registration.arena(), access, inputs, bindings)?;
        let mut outputs: Vec<_> = bindings
            .iter()
            .map(|(slot, value)| (invocation.owner(*slot), *value))
            .collect();
        let dependencies =
            projection_dependencies(self, registration.arena(), &prepared, &outputs)?;
        outputs.insert(0, (invocation.frame(), RootValue::Frame));
        Ok((
            invocation,
            BoundaryTransaction {
                registration,
                dependencies,
                prepared,
                members: Vec::new(),
                roots: RootProof::new(inputs, &outputs),
            },
        ))
    }
    /// One actual frame entry, caller continuation transfer and prepared cell
    /// graph share a single affine boundary. `outputs` excludes the canonical
    /// header, which this method inserts before the callee's bindings.
    /// Source identity, formal order, saved roles, charges and the full source
    /// peak remain the caller's compiled entry certificate obligations.
    pub fn prepare_frame_boundary<'a>(
        &self,
        registration: Registration<'a>,
        heap: &super::super::heap_impl::PagedHeap,
        invocation: &Invocation,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
        entities: &[VirtualEntity],
    ) -> Result<BoundaryTransaction<'a>, HeapFault> {
        let Owner::Frame { task, .. } = invocation.frame() else {
            unreachable!()
        };
        if self.prospective_invocation(task) != *invocation {
            return Err(invalid("frame boundary has a stale or foreign invocation"));
        }
        let next = invocation
            .id()
            .checked_add(1)
            .ok_or_else(|| invalid("logical invocation overflow"))?;
        if self.holding(invocation.frame(), heap.pages())?.is_some() {
            return Err(invalid("frame boundary invocation is already entered"));
        }
        for (owner, value) in outputs {
            if matches!(owner, Owner::Frame { .. }) || matches!(value, RootValue::Frame) {
                return Err(invalid("frame boundary cannot substitute a frame header"));
            }
            if let Owner::Value {
                task: owner_task,
                invocation: id,
                slot,
            } = owner
                && *owner_task == task
                && *id == invocation.id()
                && (!matches!(slot, Slot::Binding(_) | Slot::Capture(_))
                    || matches!(value, RootValue::Frozen(_) | RootValue::Departure))
            {
                return Err(invalid(
                    "frame boundary has another formal owner or value kind",
                ));
            }
        }
        let roots: Vec<_> = std::iter::once((invocation.frame(), RootValue::Frame))
            .chain(outputs.iter().copied())
            .collect();
        let (mut boundary, _) =
            self.prepare_construction_boundary(registration, heap, inputs, &roots, entities)?;
        boundary.prepared.next.next_invocation = next;
        Ok(boundary)
    }

    pub fn prepare_leave_publication<'a>(
        &self,
        invocation: &Invocation,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<BoundaryTransaction<'a>, HeapFault> {
        let inputs = self.invocation_owners(invocation, access)?;
        let prepared = self.prepare_leave(invocation, registration.arena(), access, outputs)?;
        let dependencies = projection_dependencies(self, registration.arena(), &prepared, outputs)?;
        Ok(BoundaryTransaction {
            registration,
            dependencies,
            prepared,
            members: Vec::new(),
            roots: RootProof::new(&inputs, outputs),
        })
    }
    pub fn prepare_publication<'a>(
        &self,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<BoundaryTransaction<'a>, HeapFault> {
        let prepared = self.prepare_ownership(registration.arena(), access, inputs, outputs)?;
        let dependencies = projection_dependencies(self, registration.arena(), &prepared, outputs)?;
        Ok(BoundaryTransaction {
            registration,
            dependencies,
            prepared,
            members: Vec::new(),
            roots: RootProof::new(inputs, outputs),
        })
    }
    #[expect(clippy::too_many_arguments, reason = "one atomic source member event")]
    pub fn prepare_member_publication<'a>(
        &self,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        handle: ArenaHandle,
        member: Member,
        next: Option<Value>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<BoundaryTransaction<'a>, MemberFault> {
        let PreparedMember {
            prepared,
            dependencies,
            ..
        } = self.prepare_member_ownership(
            &mut registration,
            access,
            handle,
            member.clone(),
            next,
            inputs,
            outputs,
        )?;
        Ok(BoundaryTransaction {
            registration,
            dependencies,
            prepared,
            members: vec![MemberEdit {
                handle,
                member,
                next,
            }],
            roots: RootProof::new(inputs, outputs),
        })
    }
}

impl<'a> BoundaryTransaction<'a> {
    /// Keep the exact input/callee/cell versions used by the certificate,
    /// including values which do not survive this exit. Output projection
    /// alone cannot authenticate a discarded input or a resolved callee.
    pub fn bind_dependencies(mut self, dependencies: Dependencies) -> Result<Self, HeapFault> {
        self.dependencies.extend(dependencies)?;
        Ok(self)
    }

    /// Invocation reservations settle with the same roots, edits and revision.
    /// A failed or abandoned boundary consumes none of its reserved IDs.
    pub fn complete_invocations(
        mut self,
        range: InvocationRange,
        completed: u64,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        self.prepared = self
            .prepared
            .complete_invocations(range, completed, access)?;
        Ok(self)
    }

    pub(super) fn from_program(
        registration: Registration<'a>,
        dependencies: Dependencies,
        prepared: Prepared,
        members: Vec<MemberEdit>,
        roots: RootProof,
    ) -> Self {
        Self {
            registration,
            dependencies,
            prepared,
            members,
            roots,
        }
    }

    /// Consume observer exclusion at the same revision as this canonical
    /// member transaction. Every actual target must belong to the proof;
    /// ordinary source and resource authentication still owns acceptance.
    pub fn with_private_observers(mut self, proof: ObserverExclusion) -> Result<Self, HeapFault> {
        if !proof.matches(self.dependencies.ledger()) || self.members.is_empty() {
            return Err(invalid("private member observers changed"));
        }
        for edit in &self.members {
            let entity = self
                .registration
                .arena()
                .ledger_key(edit.handle)
                .map_err(|error| invalid(&error.to_string()))?;
            if !proof.contains(entity) {
                return Err(invalid("private member target has another observer proof"));
            }
        }
        Ok(self)
    }

    /// Authenticate an external source certificate against the exact prepared
    /// entry owners and surviving values, including native/page provenance.
    /// A matching holding price alone never authenticates an output value.
    pub fn matches_roots(
        &mut self,
        ledger: &Ledger,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> bool {
        self.is_current(ledger) && self.roots.matches(inputs, outputs)
    }

    /// The source certificate must account for every final staged edit, with
    /// its exact target, normalized address and replacement value.
    pub fn matches_members(&self, expected: &[MemberEdit]) -> bool {
        self.members.len() == expected.len()
            && self.members.iter().zip(expected).all(|(actual, expected)| {
                actual.handle == expected.handle
                    && actual.member.same(&expected.member)
                    && match (actual.next, expected.next) {
                        (Some(a), Some(b)) => a.same(&b),
                        (None, None) => true,
                        _ => false,
                    }
            })
    }

    pub fn holding(&self, owner: Owner, access: &Access<'_>) -> Result<Option<Holding>, HeapFault> {
        self.view().holding(owner, access)
    }
    pub fn release_cost(&self, owners: &[Owner], access: &Access<'_>) -> Result<u64, HeapFault> {
        self.view().release_cost(owners, access)
    }
    fn view(&self) -> Ledger {
        Ledger {
            state: self.prepared.next.clone(),
            identity: Rc::clone(&self.prepared.identity),
            revision: self.prepared.revision,
            facts: Default::default(),
        }
    }
    pub fn peak(&self) -> u64 {
        self.prepared.peak()
    }
    pub fn balance(&self) -> Result<Balance, HeapFault> {
        self.prepared.balance()
    }
    pub fn is_current(&mut self, ledger: &Ledger) -> bool {
        self.dependencies
            .is_current(ledger, self.registration.arena())
    }
    pub fn require_source_room(mut self, extra: u64) -> Option<Self> {
        self.prepared = self.prepared.require_source_room(extra)?;
        Some(self)
    }
    /// See [`Prepared::collect`].
    pub fn collect(mut self, before: &Ledger, access: &Access<'_>) -> Result<Self, HeapFault> {
        self.prepared = self.prepared.collect(
            before,
            access,
            crate::paged::census_impl::TraceLimits::default(),
        )?;
        Ok(self)
    }
    /// Each appended event contributes its own peak. A single member event
    /// consumes its operands through prepare_member_publication instead.
    pub fn then_ownership(
        mut self,
        ledger: &Ledger,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Self, HeapFault> {
        if !self.is_current(ledger) {
            return Err(invalid("stale source publication"));
        }
        self.prepared =
            self.prepared
                .then_ownership(self.registration.arena(), access, inputs, outputs)?;
        let next =
            projection_dependencies(ledger, self.registration.arena(), &self.prepared, outputs)?;
        self.dependencies.extend(next)?;
        self.roots.append(inputs, outputs);
        Ok(self)
    }
    /// Independently staged targets may share one pure publication. Repeated
    /// writes to a target require a committed successor frontier: the canonical
    /// arena still contains its original member until this journal publishes.
    #[expect(clippy::too_many_arguments, reason = "one source member frontier")]
    pub fn then_member(
        mut self,
        ledger: &Ledger,
        access: &Access<'_>,
        handle: ArenaHandle,
        member: Member,
        next: Option<Value>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<Self, MemberFault> {
        if !self.is_current(ledger) || self.members.iter().any(|edit| edit.handle == handle) {
            return Err(invalid("member publication needs a successor frontier").into());
        }
        let staged = Ledger {
            state: self.prepared.next,
            identity: self.prepared.identity,
            revision: self.prepared.revision,
            facts: Default::default(),
        };
        let PreparedMember {
            mut prepared,
            dependencies,
        } = staged.prepare_member_ownership(
            &mut self.registration,
            access,
            handle,
            member.clone(),
            next,
            inputs,
            outputs,
        )?;
        prepared.before = self.prepared.before;
        prepared.peak = prepared.peak.max(self.prepared.peak);
        prepared.projected.extend(self.prepared.projected);
        self.prepared = prepared;
        self.dependencies.extend(dependencies)?;
        self.roots.append(inputs, outputs);
        self.members.push(MemberEdit {
            handle,
            member,
            next,
        });
        Ok(self)
    }
    pub fn admit(self, bound: u64) -> Result<AcceptedBoundary<'a>, Box<Self>> {
        if self.prepared.peak() <= bound {
            Ok(AcceptedBoundary(self))
        } else {
            Err(Box::new(self))
        }
    }
}
impl AcceptedBoundary<'_> {
    /// All fallible semantic storage preparation is owned by the caller's
    /// canonical mutation guard. A stale journal invokes no semantic edit.
    /// The registration is committed inside the same infallible publication.
    pub fn publish(
        self,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<(), HeapFault> {
        let BoundaryTransaction {
            mut registration,
            dependencies,
            prepared,
            members,
            roots: _,
        } = self.0;
        if !dependencies.is_current(ledger, registration.arena()) {
            return Err(invalid("stale source publication"));
        }
        ledger.publish(Accepted(prepared), || {
            semantic(registration.arena(), members);
            registration.commit();
        })
    }
}
fn projection_dependencies(
    ledger: &Ledger,
    arena: &Arena,
    prepared: &Prepared,
    outputs: &[(Owner, RootValue<'_>)],
) -> Result<Dependencies, HeapFault> {
    let mut handles = Vec::new();
    for (_, value) in outputs {
        match value {
            RootValue::Value(value) => handles.extend(value.handle()),
            RootValue::Prepared(value) => handles.extend_from_slice(value.graph().dependencies()),
            RootValue::Capture(handle) => handles.push(*handle),
            RootValue::Existing(_)
            | RootValue::Frame
            | RootValue::Departure
            | RootValue::Page(_)
            | RootValue::Frozen(_) => {}
        }
    }
    // New entity membership was read from resident bodies. Bind each of those
    // projections, without scanning any existing or cold population.
    for entity in &prepared.projected {
        handles.push(
            arena
                .logical_handle(*entity)
                .ok_or_else(|| invalid("projected body disappeared"))?,
        );
    }
    Dependencies::capture(ledger, arena, &handles)
        .ok_or_else(|| invalid("source content facts are unavailable"))
}
