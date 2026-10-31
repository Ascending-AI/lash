//! Source activations outlive their physical packet windows.
mod binding;
mod closure;
mod construction;
mod member;
mod native;
pub(in crate::machine::islands::execute) enum ValuePublication {
    Unavailable,
    Declined,
    Published(Value),
}
mod publication;
mod returning;
mod scope;
mod session;
mod unit;
use super::*;
use crate::compile::room::{self, publication::Frontier};
use lash_kernel_doc::{self as doc, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::{Invocation, Owner, SourceValue};

pub(in crate::machine::islands) struct Activation {
    pub(in crate::machine::islands) invocation: Invocation,
    pub(in crate::machine::islands) frontier: Option<Frontier>,
    pub(in crate::machine::islands) cleaning: Option<Result<Value, Value>>,
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    fn source_return_frame(&self, frame: &storage::Values<'_, '_>) -> bool {
        if frame.state.resident
            || !frame.state.callee.plan.charged
            || frame.state.boundary.is_some()
            || frame.state.callee.compiled.cleanups
            || frame.state.control.len() != 1
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

    fn source_operand_transfer(
        &self,
        frame: &storage::Values<'_, '_>,
        at: &lash_kernel_value::sites::Site,
    ) -> bool {
        match self
            .exe
            .code(frame.state.callee.plan.code)
            .source_room
            .transfer_target(at)
        {
            Some(room::TransferTarget::Binding) => true,
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
        let invocation = self
            .logical
            .enter(self.task, self.arena, self.heap, inputs, &bindings)?;
        self.logical_memory
            .set(self.heap.logical_balance()?.total()?);
        Ok(Activation {
            invocation,
            frontier: None,
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
                .replace(owner, Some(SourceValue::Value(frame.regs[reg.ix()])));
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
                .replace(*owner, Some(SourceValue::Value(*value)));
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
            self.logical.replace(owner, source_value(slot, *state));
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
            self.logical.replace(owner, None);
        }
        let publish = |selector: room::Owner, value: Option<Value>| -> Result<(), HeapFault> {
            if let Some(value) = value {
                let owner = slots
                    .root(&selector, self.task, activation.invocation.id())
                    .ok_or_else(|| malformed("packet control has no canonical owner"))?;
                self.logical.replace(owner, Some(SourceValue::Value(value)));
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
                    publish(room::Owner::LoopCursor(site.clone()), cursor)?;
                    publish(room::Owner::LoopHeader(site.clone()), header)?;
                }
                Some(Roles::Try { stmt, pending }) => publish(
                    room::Owner::PendingDeparture(self.exe.stmt(stmt).site.clone()),
                    pending,
                )?,
                _ => {}
            }
        }
        if let Some(cleaning) = activation.cleaning {
            publish(
                room::Owner::ActiveDeparture(source.site.clone()),
                Some(cleaning.unwrap_or_else(|value| value)),
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
                self.logical
                    .replace(operand, Some(SourceValue::Value(frame.regs[role.reg.ix()])));
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
            self.logical.replace(owner, Some(SourceValue::Value(value)));
        }
        self.logical.replace(Owner::Incoming(self.task), None);
        self.logical.flush(self.arena, self.heap)
    }

    pub(super) fn source_leave(
        &mut self,
        activation: &Activation,
        value: Value,
    ) -> Result<(), HeapFault> {
        self.logical.leave(
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
) -> Option<SourceValue> {
    use lash_kernel_state::kernel_internals::logical::Slot;
    match state {
        SlotState::Empty => None,
        SlotState::Value(value) => Some(SourceValue::Value(value)),
        SlotState::Cell(cell) if matches!(slot, Slot::Capture(_)) => {
            Some(SourceValue::Capture(cell))
        }
        SlotState::Cell(cell) => Some(SourceValue::Value(Value::ObjectRef(cell))),
    }
}
fn malformed(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}

#[derive(Default)]
struct LeafFacts {
    dependencies: Vec<ArenaHandle>,
    call: Option<(
        lash_kernel_value::sites::Site,
        room::CallSource,
        room::CheckedRoom,
    )>,
    values: std::collections::BTreeMap<room::Measure, i128>,
}
impl room::Facts for LeafFacts {
    fn dependencies(&self) -> Option<&[ArenaHandle]> {
        Some(&self.dependencies)
    }
    fn measure(&self, measure: &room::Measure) -> Option<i128> {
        self.values.get(measure).copied()
    }
    fn call(
        &self,
        at: &lash_kernel_value::sites::Site,
        callee: room::CallSource,
    ) -> Option<&room::CheckedRoom> {
        self.call
            .as_ref()
            .filter(|(site, source, _)| site == at && *source == callee)
            .map(|(_, _, room)| room)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(
        &self,
        _: &lash_kernel_value::sites::Site,
    ) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}
struct LeafExecution(std::collections::BTreeMap<room::Point, LeafFacts>);
impl room::Execution for LeafExecution {
    fn event(&mut self, at: &room::Point) -> Option<Box<dyn room::Facts + '_>> {
        struct Borrowed<'a>(&'a LeafFacts);
        impl room::Facts for Borrowed<'_> {
            fn dependencies(&self) -> Option<&[ArenaHandle]> {
                self.0.dependencies()
            }
            fn measure(&self, measure: &room::Measure) -> Option<i128> {
                self.0.measure(measure)
            }
            fn call(
                &self,
                at: &lash_kernel_value::sites::Site,
                callee: room::CallSource,
            ) -> Option<&room::CheckedRoom> {
                self.0.call(at, callee)
            }
            fn raised(&self) -> bool {
                false
            }
            fn resolved(
                &self,
                _: &lash_kernel_value::sites::Site,
            ) -> Option<lash_kernel_value::FunctionId> {
                None
            }
        }
        Some(Box::new(Borrowed(self.0.get(at)?)))
    }
    fn choose(&mut self, _: &lash_kernel_value::sites::Site) -> Option<bool> {
        None
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Entry has already committed its real parameter and capture owners.
    /// This suffix may decline without discarding or repeating that entry.
    pub(super) fn publish_literal_body(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
    ) -> Result<bool, HeapFault> {
        use crate::admission::Allowance;
        use lash_kernel_state::kernel_internals::logical::{
            FrameEvent, Holding, JournalEvent, Program, PublicationValue as PV,
        };
        use room::{
            ExitKind, Measure, Phase, Point, Selection,
            publication::{Occurrence, Position},
        };
        let Some(literal) = &callee.compiled.literal else {
            return Ok(false);
        };
        // This rule needs no native/body contract or result dependency. A
        // non-inline value takes its ordinary source path.
        if literal.value.handle().is_some() {
            return Ok(false);
        }
        let source = &self.exe.code(callee.plan.code).source_room;
        let statement = self.exe.stmt(literal.statement);
        let return_site = statement.site.clone();
        let literal_site = return_site.child(0);
        let point = |site, phase| Point { site, phase };
        let enter = point(source.site.clone(), Phase::Enter);
        let operand_at = point(literal_site.clone(), Phase::Commit);
        let return_at = point(return_site.clone(), Phase::Commit);
        let leave = point(source.site.clone(), Phase::Leave);
        let selections = [
            Selection::Event {
                at: enter,
                exit: ExitKind::Normal,
            },
            Selection::Event {
                at: operand_at.clone(),
                exit: ExitKind::Normal,
            },
            Selection::Event {
                at: return_at.clone(),
                exit: ExitKind::Return,
            },
            Selection::Event {
                at: leave.clone(),
                exit: ExitKind::Normal,
            },
        ];
        let Ok(interval) =
            source.compile_between(&operand_at, &leave, &selections, selections.len())
        else {
            return Ok(false);
        };
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("literal source owners failed verification"))?;
        let owner = |selector| {
            slots
                .root(&selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("literal source owner is absent"))
        };
        let operand_selector = room::Owner::Operand(literal_site);
        let departure_selector = room::Owner::Departure(return_site);
        let operand = owner(operand_selector.clone())?;
        let departure = owner(departure_selector.clone())?;
        let invocation = activation.invocation.id();
        let task = self.task;
        let make = || {
            Program::new(vec![
                JournalEvent {
                    outputs: vec![(operand, PV::Native(literal.value))],
                    ..Default::default()
                },
                JournalEvent {
                    inputs: vec![operand],
                    outputs: vec![(departure, PV::Existing(operand))],
                    ..Default::default()
                },
                JournalEvent {
                    frame: Some(FrameEvent::Leave { task, invocation }),
                    outputs: vec![(Owner::Incoming(task), PV::Existing(departure))],
                    ..Default::default()
                },
            ])
        };
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let bytes = i128::from(
            Holding::from_native(self.arena, self.heap.pages(), &literal.value)?.bytes(),
        );
        let groups = vec![
            room::Owner::Scope(source.site.clone()),
            room::Owner::Operands(source.site.clone()),
        ];
        let released = groups
            .iter()
            .flat_map(|selector| {
                slots
                    .group(selector, self.task, activation.invocation.id())
                    .into_iter()
                    .flatten()
            })
            .filter_map(|root| match ledger.holding(root, self.heap.pages()) {
                Ok(Some(_)) => Some(Ok(root)),
                Ok(None) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let release = i128::from(ledger.release_cost(&released, self.heap.pages())?);
        let facts = LeafExecution(
            [
                (
                    operand_at.clone(),
                    LeafFacts {
                        values: [(Measure::Local(operand_selector.clone()), bytes)].into(),
                        ..Default::default()
                    },
                ),
                (
                    return_at,
                    LeafFacts {
                        values: [
                            (Measure::Local(departure_selector), bytes),
                            (Measure::Release(vec![operand_selector].into()), bytes),
                        ]
                        .into(),
                        ..Default::default()
                    },
                ),
                (
                    leave.clone(),
                    LeafFacts {
                        values: [(Measure::Release(groups.into()), release)].into(),
                        ..Default::default()
                    },
                ),
            ]
            .into(),
        );
        use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.literal-return");
        operand_at.site.encode(&mut encoding);
        operand.encode(&mut encoding);
        departure.encode(&mut encoding);
        let recovery = logical_digest(&encoding.into_bytes());
        let start = Position {
            at: operand_at.clone(),
            unpaid: Some(operand_at),
            recovery,
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: vec![],
                action: None,
                departure: None,
            },
        };
        let next = Position {
            at: leave,
            unpaid: None,
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 1,
            close: literal.close,
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        drop(ledger);
        let published = self.publish_source(
            callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            make,
            |_, edits| debug_assert!(edits.is_empty()),
        )?;
        if published {
            witnessed!(SourcePublication, Commit);
        }
        Ok(published)
    }
}
