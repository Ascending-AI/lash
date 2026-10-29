//! A publication owns its unpublished identity grants until ledger commit.
//! No arena borrow or grant guard escapes a prepared or accepted publication.
use super::*;
use crate::arena::{Arena, Registration};
use lash_kernel_doc::{Value, arena::ArenaHandle};

/// The canonical storage owner receives exactly the edit whose old content
/// and replacement were checked. It reserves fallible storage before publish.
#[derive(Clone, Copy, Debug)]
pub struct MemberEdit {
    pub handle: ArenaHandle,
    pub member: Member,
    pub next: Option<Value>,
}
pub struct PreparedPublication<'a> {
    registration: Registration<'a>,
    dependencies: Dependencies,
    prepared: Prepared,
    members: Vec<MemberEdit>,
}
pub struct AcceptedPublication<'a>(PreparedPublication<'a>);

impl Ledger {
    pub fn prepare_enter_publication<'a>(
        &self,
        task: TaskId,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        inputs: &[Owner],
        bindings: &[(Slot, RootValue<'_>)],
    ) -> Result<(Invocation, PreparedPublication<'a>), HeapFault> {
        let (invocation, prepared) =
            self.prepare_enter(task, registration.arena(), access, inputs, bindings)?;
        let outputs: Vec<_> = bindings
            .iter()
            .map(|(slot, value)| (invocation.owner(*slot), *value))
            .collect();
        let dependencies =
            projection_dependencies(self, registration.arena(), &prepared, &outputs)?;
        Ok((
            invocation,
            PreparedPublication {
                registration,
                dependencies,
                prepared,
                members: Vec::new(),
            },
        ))
    }
    pub fn prepare_leave_publication<'a>(
        &self,
        invocation: &Invocation,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<PreparedPublication<'a>, HeapFault> {
        let prepared = self.prepare_leave(invocation, registration.arena(), access, outputs)?;
        let dependencies = projection_dependencies(self, registration.arena(), &prepared, outputs)?;
        Ok(PreparedPublication {
            registration,
            dependencies,
            prepared,
            members: Vec::new(),
        })
    }
    pub fn prepare_publication<'a>(
        &self,
        mut registration: Registration<'a>,
        access: &Access<'_>,
        inputs: &[Owner],
        outputs: &[(Owner, RootValue<'_>)],
    ) -> Result<PreparedPublication<'a>, HeapFault> {
        let prepared = self.prepare_ownership(registration.arena(), access, inputs, outputs)?;
        let dependencies = projection_dependencies(self, registration.arena(), &prepared, outputs)?;
        Ok(PreparedPublication {
            registration,
            dependencies,
            prepared,
            members: Vec::new(),
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
    ) -> Result<PreparedPublication<'a>, MemberFault> {
        let PreparedMember {
            prepared,
            dependencies,
            ..
        } = self.prepare_member_ownership(
            &mut registration,
            access,
            handle,
            member,
            next,
            inputs,
            outputs,
        )?;
        Ok(PreparedPublication {
            registration,
            dependencies,
            prepared,
            members: vec![MemberEdit {
                handle,
                member,
                next,
            }],
        })
    }
}

impl<'a> PreparedPublication<'a> {
    pub(super) fn from_program(
        registration: Registration<'a>,
        dependencies: Dependencies,
        prepared: Prepared,
        members: Vec<MemberEdit>,
    ) -> Self {
        Self {
            registration,
            dependencies,
            prepared,
            members,
        }
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
        };
        let PreparedMember {
            mut prepared,
            dependencies,
        } = staged.prepare_member_ownership(
            &mut self.registration,
            access,
            handle,
            member,
            next,
            inputs,
            outputs,
        )?;
        prepared.before = self.prepared.before;
        prepared.peak = prepared.peak.max(self.prepared.peak);
        prepared.projected.extend(self.prepared.projected);
        self.prepared = prepared;
        self.dependencies.extend(dependencies)?;
        self.members.push(MemberEdit {
            handle,
            member,
            next,
        });
        Ok(self)
    }
    pub fn admit(self, bound: u64) -> Result<AcceptedPublication<'a>, Box<Self>> {
        if self.prepared.peak() <= bound {
            Ok(AcceptedPublication(self))
        } else {
            Err(Box::new(self))
        }
    }
}
impl AcceptedPublication<'_> {
    /// All fallible semantic storage preparation is owned by the caller's
    /// canonical mutation guard. A stale journal invokes no semantic edit.
    /// The registration is committed inside the same infallible publication.
    pub fn publish(
        self,
        ledger: &mut Ledger,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<(), HeapFault> {
        let PreparedPublication {
            mut registration,
            dependencies,
            prepared,
            members,
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
            RootValue::Capture(handle) => handles.push(*handle),
            RootValue::Existing(_)
            | RootValue::Frame
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
