//! Source activations outlive their physical packet windows.
mod binding;
mod calling;
mod closure;
mod construction;
mod end;
mod frame_entry;
mod initial_entry;
mod loop_entry;
mod member;
mod native;
mod parameters;
mod text;
#[cfg(test)]
pub(crate) use text::{
    take_body_text_publications, take_original_text_publications, take_text_publications,
};
pub(in crate::machine::islands::execute) enum ValuePublication {
    Unavailable,
    Declined,
    Published(Value),
}
mod publication;
mod read;
mod resuming;
mod returning;
mod scope;
mod session;
mod unit;
use super::*;
use crate::compile::room::{self, publication::Frontier};
use crate::machine::source_publication::{LeafExecution, LeafFacts, ScopeLoop};
use lash_kernel_doc::{self as doc, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::{Invocation, Owner, PublicationValue};

pub(in crate::machine::islands) struct Activation {
    pub(in crate::machine::islands) invocation: Invocation,
    pub(in crate::machine::islands) frontier: Option<Frontier>,
    pub(in crate::machine::islands) loop_input: Option<room::publication::boundary::PaidLoopInput>,
    pub(in crate::machine::islands) original: Option<room::charges::AcceptedOriginalPrefix>,
    pub(in crate::machine::islands) cleaning: Option<Result<Value, Value>>,
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn source_return_frame(&self, frame: &storage::Values<'_, '_>) -> bool {
        // An uncharged resident library still owes its semantic caller's
        // completion formula. Its ordinary return owns that final boundary.
        if (frame.state.resident && !frame.state.callee.plan.charged)
            || (frame.state.callee.plan.charged && frame.state.boundary.is_some())
            || frame.state.callee.compiled.cleanups
            || !frame.state.control.iter().all(|control| match control {
                Position::Block(..) => true,
                Position::Loop {
                    id,
                    started,
                    cursor,
                    ..
                } => {
                    *started > 0
                        && match self.exe.stmt(*id).form {
                            crate::compile::Form::While { .. } => cursor.is_none(),
                            crate::compile::Form::For { .. } => cursor.is_some(),
                            _ => false,
                        }
                }
                // A function with cleanup is excluded above; its suffix
                // authenticates the entered body or catch block.
                Position::Try {
                    phase: Phase::Body | Phase::Catch,
                    ..
                } => true,
                _ => false,
            })
        {
            return false;
        }
        let Some(Position::Block(block, _)) = frame.state.control.first() else {
            return false;
        };
        self.exe
            .block(frame.state.callee.compiled.blocks[*block].id)
            .site
            == self.exe.code(frame.state.callee.plan.code).source_room.site
    }

    /// Source transfer classification for an exact dynamic boundary.
    fn source_operand_transfer(
        &self,
        frame: &storage::Values<'_, '_>,
        _stream: &crate::compile::Stream,
        _pc: u32,
        at: &lash_kernel_value::sites::Site,
    ) -> bool {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let target = source.transfer_target(at);
        match target {
            Some(room::TransferTarget::Binding | room::TransferTarget::NativeInput) => true,
            Some(room::TransferTarget::RootReturn) => self.source_return_frame(frame),
            None => false,
        }
    }

    fn source_next_statement(&self, frame: &storage::Values<'_, '_>) -> Option<room::Point> {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Position::Block(block, next) = frame.state.control.last()? else {
            return None;
        };
        let block = self
            .exe
            .block(frame.state.callee.compiled.blocks[*block].id);
        if let Some(next) = block.stmts.get(*next) {
            source.first_event(&self.exe.stmt(*next).site).ok()
        } else {
            Some(room::Point {
                site: block.site.clone(),
                phase: if block.site == source.site {
                    room::Phase::Fallthrough
                } else {
                    room::Phase::Leave
                },
            })
        }
    }

    pub(super) fn source_enter(
        &mut self,
        frame: &storage::Values<'_, '_>,
        inputs: &[Owner],
    ) -> Result<Activation, HeapFault> {
        let code = self.exe.code(frame.state.callee.plan.code);
        let source = &code.source_room;
        let bindings = frame
            .slots
            .iter()
            .enumerate()
            .filter_map(|(at, state)| {
                let owner = source.binding_root(code.slots_by_at[at], self.task, 0)?;
                let Owner::Value { slot, .. } = owner else {
                    return None;
                };
                source_value(slot, *state).map(|value| (slot, value))
            })
            .collect::<Vec<_>>();
        self.logical.flush(self.arena, self.heap)?;
        let invocation = self
            .logical
            .ledger()
            .borrow_mut()
            .enter_source(self.task, self.arena, self.heap, inputs, &bindings)?;
        self.logical_memory
            .set(self.heap.logical_balance()?.total()?);
        Ok(Activation {
            invocation,
            frontier: None,
            original: None,
            loop_input: None,
            cleaning: None,
        })
    }

    pub(super) fn source_call_inputs(
        &mut self,
        frame: &storage::Values<'_, '_>,
        site: &compiled::CallSite,
        args: &[u32],
    ) -> Result<Vec<Owner>, HeapFault> {
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet caller has no source activation"))?;
        self.source_bindings(frame)?;
        let code = self.exe.code(frame.state.callee.plan.code);
        let source = &code.source_room;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("packet source owners failed verification"))?;
        let stmt = frame
            .state
            .statement
            .ok_or_else(|| malformed("packet call has no source statement"))?;
        let (stream, statement, _) = self.exe.statement(stmt);
        let roles = statement
            .source_roles_at(stream, site.pc)
            .ok_or_else(|| malformed("packet call has no source operand projection"))?;
        let mut inputs = Vec::new();
        for (reg, site) in roles.iter() {
            let owner = slots
                .root(
                    &room::Owner::Operand((**site).clone()),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("packet argument has no source owner"))?;
            self.logical
                .replace(owner, Some(PublicationValue::Native(frame.regs[reg.ix()])));
            if args.contains(&reg.0) {
                inputs.push(owner);
            }
        }
        self.logical.flush(self.arena, self.heap)?;
        Ok(inputs)
    }

    pub(super) fn source_continuation(
        &self,
        frame: &storage::Values<'_, '_>,
        site: &compiled::CallSite,
    ) -> Result<Vec<(Owner, Value)>, HeapFault> {
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet caller has no source activation"))?;
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("packet source owners failed verification"))?;
        let mut saved = Vec::new();
        if let Some(act) = site.act {
            for role in &frame.state.callee.stream.nested[act.ix()].roles {
                let owner = slots
                    .continuation(&role.site, self.task, activation.invocation.id())
                    .ok_or_else(|| malformed("packet continuation has no canonical owner"))?;
                saved.push((owner, frame.regs[role.reg.ix()]));
            }
        }
        Ok(saved)
    }

    pub(super) fn source_called(
        &mut self,
        frame: &storage::Values<'_, '_>,
        saved: &[(Owner, Value)],
    ) -> Result<(), HeapFault> {
        self.source_clear_operands(frame)?;
        for (owner, value) in saved {
            self.logical
                .replace(*owner, Some(PublicationValue::Native(*value)));
        }
        self.logical.flush(self.arena, self.heap)
    }

    pub(super) fn source_clear_operands(
        &self,
        frame: &storage::Values<'_, '_>,
    ) -> Result<(), HeapFault> {
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet frame has no source activation"))?;
        self.logical
            .clear_operands(&activation.invocation, self.heap.pages())
    }

    pub(super) fn source_bindings(&self, frame: &storage::Values<'_, '_>) -> Result<(), HeapFault> {
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet frame has no source activation"))?;
        let code = self.exe.code(frame.state.callee.plan.code);
        for (at, state) in frame.slots.iter().enumerate() {
            let owner = code
                .source_room
                .binding_root(code.slots_by_at[at], self.task, activation.invocation.id())
                .ok_or_else(|| malformed("packet binding has no source owner"))?;
            let Owner::Value { slot, .. } = owner else {
                return Err(malformed("packet binding has another owner class"));
            };
            self.logical.rebind(owner, source_value(slot, *state));
        }
        self.source_controls(frame)
    }

    fn source_controls(&self, frame: &storage::Values<'_, '_>) -> Result<(), HeapFault> {
        use crate::machine::controls::Roles;
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet controls have no source activation"))?;
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("packet source controls failed verification"))?;
        for owner in slots.control_roots(self.task, activation.invocation.id()) {
            self.logical.rebind(owner, None);
        }
        let publish =
            |selector: room::Owner, value: Option<PublicationValue>| -> Result<(), HeapFault> {
                if let Some(value) = value {
                    let owner = slots
                        .root(&selector, self.task, activation.invocation.id())
                        .ok_or_else(|| malformed("packet control has no canonical owner"))?;
                    self.logical.rebind(owner, Some(value));
                }
                Ok(())
            };
        for position in &frame.state.control {
            let control = position.projected(frame.state.callee.compiled, &frame.pending);
            match crate::machine::controls::roles(&control) {
                Some(Roles::For {
                    stmt,
                    cursor,
                    header,
                    initial,
                }) if !initial => {
                    let site = &self.exe.stmt(stmt).site;
                    publish(
                        room::Owner::LoopCursor(site.clone()),
                        cursor.map(PublicationValue::Native),
                    )?;
                    publish(
                        room::Owner::LoopHeader(site.clone()),
                        header.map(PublicationValue::Native),
                    )?;
                }
                Some(Roles::Try { stmt, pending }) => publish(
                    room::Owner::PendingDeparture(self.exe.stmt(stmt).site.clone()),
                    pending.map(crate::machine::controls::Departed::source),
                )?,
                _ => {}
            }
        }
        if let Some(cleaning) = activation.cleaning {
            publish(
                room::Owner::ActiveDeparture(source.site.clone()),
                Some(PublicationValue::Native(
                    cleaning.unwrap_or_else(|value| value),
                )),
            )?;
        }
        Ok(())
    }

    pub(super) fn source_returned(
        &mut self,
        frame: &storage::Values<'_, '_>,
        site: &compiled::CallSite,
        value: Value,
    ) -> Result<(), HeapFault> {
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("packet caller has no source activation"))?;
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let owners = source
            .owner_slots()
            .map_err(|_| malformed("packet source owners failed verification"))?;
        let stmt = frame
            .state
            .statement
            .ok_or_else(|| malformed("packet call has no source statement"))?;
        let (stream, statement, _) = self.exe.statement(stmt);
        let roles = statement
            .source_roles_at(stream, site.pc)
            .ok_or_else(|| malformed("packet return has no source projection"))?;
        for (_, at) in roles.iter() {
            let owner = owners
                .root(
                    &room::Owner::Operand((**at).clone()),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("packet return operand has no owner"))?;
            self.logical.replace(owner, None);
        }
        if let Some(act) = site.act {
            for role in &stream.nested[act.ix()].roles {
                let continuation = owners
                    .continuation(&role.site, self.task, activation.invocation.id())
                    .ok_or_else(|| malformed("packet return continuation has no source owner"))?;
                let operand = owners
                    .root(
                        &room::Owner::Operand(role.site.clone()),
                        self.task,
                        activation.invocation.id(),
                    )
                    .ok_or_else(|| malformed("packet return role has no source owner"))?;
                self.logical.replace(continuation, None);
                self.logical.replace(
                    operand,
                    Some(PublicationValue::Native(frame.regs[role.reg.ix()])),
                );
            }
        }
        if site.act.is_some()
            && let Some(Some(at)) = stream.source_sites.get(site.pc as usize)
        {
            let owner = owners
                .root(
                    &room::Owner::Operand((**at).clone()),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("packet call result has no source owner"))?;
            self.logical
                .replace(owner, Some(PublicationValue::Native(value)));
        }
        self.logical.replace(Owner::Incoming(self.task), None);
        self.logical.flush(self.arena, self.heap)
    }

    pub(super) fn source_leave(
        &mut self,
        activation: &Activation,
        value: Value,
    ) -> Result<(), HeapFault> {
        self.logical.flush(self.arena, self.heap)?;
        self.logical.ledger().borrow_mut().leave_source(
            &activation.invocation,
            self.arena,
            self.heap,
            (Owner::Incoming(self.task), value),
        )
    }
}
fn source_value(
    slot: lash_kernel_state::kernel_internals::logical::Slot,
    state: SlotState,
) -> Option<PublicationValue> {
    use lash_kernel_state::kernel_internals::logical::Slot;
    match state {
        SlotState::Empty => None,
        SlotState::Value(value) => Some(PublicationValue::Native(value)),
        SlotState::Cell(cell) if matches!(slot, Slot::Capture(_)) => {
            Some(PublicationValue::Capture(cell))
        }
        SlotState::Cell(cell) => Some(PublicationValue::Native(Value::ObjectRef(cell))),
    }
}
fn malformed(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// The compiler owns the literal, its full canonical debt and its only
    /// interior acquisition. Entry is already a committed source prefix.
    pub(super) fn publish_literal_body(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
    ) -> Result<bool, HeapFault> {
        use crate::admission::SourceResources;
        use room::publication::Occurrence;
        let Some(literal) = &callee.compiled.literal else {
            return Ok(false);
        };
        let source = &self.exe.code(callee.plan.code).source_room;
        let Ok(exit) = source.literal_return_exit(self.exe, callee.plan.code) else {
            return Ok(false);
        };
        if !exit.literal_matches(literal.statement, literal.reg, literal.close, literal.value) {
            return Ok(false);
        }
        let start = exit.literal_start(Occurrence {
            task: self.task,
            invocation: activation.invocation.id(),
            loops: vec![],
            action: None,
        });
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        if let Some(frontier) = activation.frontier.as_mut()
            && frontier.departs(&start)
        {
            frontier.handoff(&mut store.borrow_mut(), start.clone())?;
        }
        let ledger = store.borrow();
        if activation.frontier.is_none() {
            let document = self
                .exe
                .instances
                .get(&callee.link)
                .ok_or_else(|| malformed("literal return has no defining document"))?;
            let kernel = doc::KernelVersion::of(document.kernel())
                .ok_or_else(|| malformed("literal return has an unsupported kernel"))?;
            activation.frontier = Some(
                source
                    .frontier(
                        &ledger,
                        self.heap,
                        &activation.invocation,
                        callee.link,
                        kernel,
                        start,
                    )
                    .map_err(|_| malformed("literal frontier failed authentication"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("literal frontier disappeared"))?;
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        };
        let registration = self
            .arena
            .registration()
            .map_err(publication::native_fault)?;
        let Some(accepted) = exit.prepare_literal(
            source,
            frontier,
            &activation.invocation,
            &ledger,
            registration,
            self.heap,
            literal.value,
            resources,
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted.publish_literal(frontier, &mut store.borrow_mut())?;
        self.logical_memory.set(balance.total()?);
        if let Some(tape) = self.recorder {
            exit.record_literal(tape);
        }
        witnessed!(SourcePublication, Commit);
        Ok(true)
    }
}
