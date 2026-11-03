//! A native text result and its source write share one storage obligation.
use super::*;
use crate::compile::room::{
    charges::{AcceptedOriginalPrefix, ChargeDebt, ChargeProgram, FormulaValues, OriginalPayment},
    placement::{BindingPlacement, DenseActivation},
};
use crate::compile::{CodeId, Executable, LibId, LibRun, StmtId};
use lash_kernel_doc::{Value, arena::ArenaHandle};
use lash_kernel_state::arena::PreparedTexts;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use logical::{Invocation, Slot, Stamp};

/// This token is issued only after the fresh result and binding were published.
/// A source loop has exactly one read of the binding, at this append's left
/// operand. Its next iteration cannot have exported that result in between.
/// Cold recovery starts without tokens and earns a new one on its next write.
pub(crate) struct TextOwnership {
    source: Arc<Source>,
    invocation: Invocation,
    stamp: Stamp,
    physical: usize,
    value: ArenaHandle,
    loops: Vec<(Site, u64)>,
}
impl TextOwnership {
    pub(crate) fn physical(&self) -> usize {
        self.physical
    }
    fn permits(
        &self,
        plan: &TextWrite,
        ledger: &Ledger,
        invocation: &Invocation,
        occurrence: &Occurrence,
        value: ArenaHandle,
    ) -> bool {
        Arc::ptr_eq(&self.source, plan.placement.source())
            && &self.invocation == invocation
            && ledger.owns_stamp(&self.stamp)
            && self.physical == plan.physical
            && self.value == value
            && self.loops.len() == occurrence.loops.len()
            && plan.private_loop.as_ref().is_some_and(|target| {
                self.loops
                    .iter()
                    .zip(&occurrence.loops)
                    .all(|((old, before), (new, after))| {
                        old == new
                            && if new == target {
                                before.checked_add(1) == Some(*after)
                            } else {
                                before == after
                            }
                    })
                    && occurrence.loops.iter().any(|(site, _)| site == target)
            })
    }
}

pub(crate) struct TextWrite {
    function: lash_kernel_value::FunctionId,
    primitive_function: lash_kernel_value::FunctionId,
    code: CodeId,
    statement: StmtId,
    placement: Arc<BindingPlacement>,
    library: LibId,
    primitive: LibId,
    native_pc: u32,
    write_pc: u32,
    physical: usize,
    binding: Slot,
    roles: Box<[Site]>,
    first: Point,
    through: Point,
    after: Point,
    interval: Arc<CompiledInterval>,
    native_charges: Arc<ChargeProgram>,
    write_charges: Arc<ChargeProgram>,
    private_loop: Option<Site>,
    recovery: [u8; 32],
}

/// Only this compiler's canonical storage call can construct these facts.
/// The forwarding body may use summaries after an input has donated its bytes.
pub(crate) struct PreparedConcat {
    primitive: LibId,
    arguments: [Value; 2],
    result: Value,
    contract: doc::NativeResourceContract,
}
impl PreparedConcat {
    pub(crate) fn primitive(&self) -> LibId {
        self.primitive
    }
    pub(crate) fn arguments(&self) -> &[Value] {
        &self.arguments
    }
    pub(crate) fn result(&self) -> Value {
        self.result
    }
    pub(crate) fn contract(&self) -> &doc::NativeResourceContract {
        &self.contract
    }
}

pub(crate) struct PreparedTextWrite<'p, 'a> {
    plan: &'p TextWrite,
    storage: PreparedTexts<'a>,
    invocation: Invocation,
    inputs: Vec<logical::Owner>,
    output: logical::Owner,
    value: Value,
    room: CheckedRoom,
    debt: ChargeDebt<'p>,
    write_debt: ChargeDebt<'p>,
    payment: Option<OriginalPayment>,
    completion: Option<super::super::region::text::TextBodyCompletion>,
    allowance: Allowance,
    before: Position,
    identity: std::rc::Rc<()>,
    revision: u64,
    next: Position,
}
pub(crate) struct TextCompletion {
    pub(crate) value: Value,
    pub(crate) balance: logical::Balance,
    pub(crate) ownership: Option<TextOwnership>,
    pub(crate) moved: u64,
}

impl TextWrite {
    #[expect(
        clippy::too_many_arguments,
        reason = "one original native and binding suffix"
    )]
    pub(crate) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statement: StmtId,
        native_pc: u32,
        write_pc: u32,
        physical: usize,
        native_charges: Arc<ChargeProgram>,
        write_charges: Arc<ChargeProgram>,
    ) -> Result<Arc<Self>, Refused> {
        let code = placement.code();
        let source = placement.source();
        let (stream, stmt, _) = exe.statement(statement);
        if native_pc >= write_pc
            || native_pc < stmt.ops.start
            || write_pc >= stmt.ops.end()
            || !placement.unshared(physical as u32)
        {
            return Err(Refused::Source);
        }
        let op = stream.ops[native_pc as usize];
        let (library, args) = match op {
            lash_kernel_exec::Op::Call { lib, args }
            | lash_kernel_exec::Op::Operator { lib, args } => (LibId(lib.0), args),
            _ => return Err(Refused::Source),
        };
        let function = exe.lib(library);
        if args.len != 2
            || function.arity != 2
            || function.limit.is_some()
            || function.shares_params
            || function.kernels(exe).concat.is_none()
        {
            return Err(Refused::Source);
        }
        let primitive = function
            .kernels(exe)
            .text_body
            .as_ref()
            .map_or(library, |body| body.primitive());
        if !matches!(exe.lib(primitive).run, LibRun::Native { .. }) {
            return Err(Refused::Source);
        }
        let lash_kernel_exec::Op::WriteLocal { local, src } = stream.ops[write_pc as usize] else {
            return Err(Refused::Source);
        };
        if local.at as usize != physical || op.writes() != Some(src) {
            return Err(Refused::Source);
        }
        let point = |pc: u32| -> Result<Point, Refused> {
            Ok(Point {
                site: (**stream
                    .source_sites
                    .get(pc as usize)
                    .and_then(Option::as_ref)
                    .ok_or(Refused::Source)?)
                .clone(),
                phase: Phase::Commit,
            })
        };
        let first = point(native_pc)?;
        let through = point(write_pc)?;
        native_charges.verify(exe, code)?;
        write_charges.verify(exe, code)?;
        if native_charges.first() != &first
            || native_charges.through() != &first
            || write_charges.first() != &through
            || write_charges.through() != &through
        {
            return Err(Refused::Source);
        }
        let mut route = TextRoute {
            source,
            selected: Vec::new(),
        };
        evaluate::walk(source, &through.site, &mut route, 4096)?;
        let interval = source.compile_between(&first, &through, &route.selected, 4096)?;
        if interval.events().len() != 2 {
            return Err(Refused::Source);
        }
        let mapping = stmt
            .source_roles_at(stream, native_pc)
            .ok_or(Refused::Source)?;
        let roles: Box<[_]> = op
            .reads()
            .iter()
            .map(|reg| {
                mapping
                    .iter()
                    .find(|(actual, _)| actual == reg)
                    .map(|(_, site)| (**site).clone())
                    .ok_or(Refused::Source)
            })
            .collect::<Result<_, _>>()?;
        let binding =
            match source.binding_root(exe.code(code).slots_by_at[physical], TaskId::MAIN, 0) {
                Some(logical::Owner::Value {
                    slot: slot @ Slot::Binding(_),
                    ..
                }) => slot,
                _ => return Err(Refused::Source),
            };
        let owners = source.owner_slots()?;
        // Both lexical resolution and the selected write must name this binding.
        let input = Point {
            site: roles[0].clone(),
            phase: Phase::Commit,
        };
        if !matches!(source.scalar_input(&owners, &input), Ok(super::super::region::ScalarInput::Binding(actual)) if actual == binding)
        {
            return Err(Refused::Source);
        }
        let mut node = doc::Node::Block(&source.body);
        for index in &through.site.path[source.site.path.len()..] {
            node = *node
                .children()
                .get(*index as usize)
                .ok_or(Refused::Source)?;
        }
        let doc::Node::Stmt(doc::Stmt::Assign {
            place: doc::Place::Variable(name),
            ..
        }) = node
        else {
            return Err(Refused::Source);
        };
        let write_owner = owners
            .root(
                &Owner::Visible {
                    at: through.site.clone(),
                    name: name.clone(),
                },
                TaskId::MAIN,
                0,
            )
            .ok_or(Refused::Source)?;
        if !matches!(write_owner, logical::Owner::Value { slot, .. } if slot == binding)
            || source.assigns_cell(&through.site)
        {
            return Err(Refused::Source);
        }
        let mut private_loop = None;
        let mut node = doc::Node::Block(&source.body);
        let mut at = source.site.clone();
        for index in &through.site.path[source.site.path.len()..] {
            if matches!(
                node,
                doc::Node::Stmt(doc::Stmt::While { .. } | doc::Stmt::For { .. })
            ) {
                let reads = source.room.events.keys().filter(|point| point.phase == Phase::Commit && point.site.path.starts_with(&at.path) && matches!(source.scalar_input(&owners, point), Ok(super::super::region::ScalarInput::Binding(slot)) if slot == binding)).count();
                if reads == 1 {
                    private_loop = Some(at.clone());
                }
            }
            node = *node
                .children()
                .get(*index as usize)
                .ok_or(Refused::Source)?;
            at.path.push(*index);
        }
        let after = source.exact_successor(&through, Successor::Normal)?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.text-write");
        first.site.encode(&mut encoding);
        through.site.encode(&mut encoding);
        Ok(Arc::new(Self {
            function: function.specialization.function,
            primitive_function: exe.lib(primitive).specialization.function,
            code,
            statement,
            placement,
            library,
            primitive,
            native_pc,
            write_pc,
            physical,
            binding,
            roles,
            first,
            through,
            after,
            interval,
            native_charges,
            write_charges,
            private_loop,
            recovery: logical_digest(&encoding.into_bytes()),
        }))
    }
    pub(crate) fn matches(
        &self,
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        pc: u32,
        write: u32,
    ) -> bool {
        Arc::ptr_eq(self.placement.source(), &exe.code(code).source_room)
            && exe.library_lib(&self.function) == Some(self.library)
            && exe.library_lib(&self.primitive_function) == Some(self.primitive)
            && self.code == code
            && self.statement == statement
            && self.native_pc == pc
            && self.write_pc == write
    }
    pub(crate) fn native_pc(&self) -> u32 {
        self.native_pc
    }
    pub(crate) fn start(&self, occurrence: Occurrence) -> Position {
        Position {
            at: self.first.clone(),
            unpaid: Some(self.first.clone()),
            recovery: self.recovery,
            occurrence,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one source-authenticated native/write preparation"
    )]
    pub(crate) fn prepare<'p, 'a>(
        &'p self,
        exe: &Executable,
        arena: &'a mut Arena,
        ledger: &Ledger,
        heap: &PagedHeap,
        frontier: &Frontier,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        arguments: &[Value],
        ownership: Option<&TextOwnership>,
        original: Option<&AcceptedOriginalPrefix>,
        safe_point: u64,
    ) -> Result<Option<PreparedTextWrite<'p, 'a>>, HeapFault> {
        let source = self.placement.source();
        if frontier.proven
            || !Arc::ptr_eq(source, &frontier.source)
            || frontier.position.unpaid.as_ref() != Some(&self.first)
            || frontier.position.occurrence.invocation != invocation.id()
            || !activation.current(source, ledger, invocation)
            || arguments.len() != 2
            || activation
                .value(self.binding)
                .is_none_or(|value| !value.same(&arguments[0]))
        {
            return Ok(None);
        }
        let function = exe.lib(self.library);
        if !function
            .header
            .signature
            .params
            .iter()
            .zip(arguments)
            .all(|(param, value)| crate::compile::import_result_fits(&param.ty, *value))
        {
            return Ok(None);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| fault("text owners changed"))?;
        let inputs = self
            .roles
            .iter()
            .map(|site| {
                slots
                    .root(
                        &Owner::Operand(site.clone()),
                        frontier.position.occurrence.task,
                        invocation.id(),
                    )
                    .ok_or_else(|| fault("text operand disappeared"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if inputs.iter().any(|owner| {
            ledger
                .holding(*owner, heap.pages())
                .ok()
                .flatten()
                .is_none()
        }) {
            return Ok(None);
        }
        let output = invocation.owner(self.binding);
        let mut retired = inputs.clone();
        retired.push(output);
        let released = ledger.release_cost(&inputs, heap.pages())?;
        let old = ledger.release_cost(&[output], heap.pages())?;
        let LibRun::Native { native, .. } = &exe.lib(self.primitive).run else {
            return Ok(None);
        };
        let contract = match native.resource_contract(doc::Args::new(
            arguments,
            arena.objects(),
            &arena.semantics(),
        )) {
            Ok(Some(contract))
                if contract.exit == doc::NativeResourceExit::Returned
                    && contract
                        .events
                        .iter()
                        .all(|event| matches!(event, doc::NativeResourceEvent::Scratch { .. })) =>
            {
                contract
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(fault(reason)),
            _ => return Ok(None),
        };
        let private = match arguments[0] {
            Value::LongText(handle)
                if ownership.is_some_and(|ownership| {
                    ownership.permits(
                        self,
                        ledger,
                        invocation,
                        &frontier.position.occurrence,
                        handle,
                    )
                }) && arguments[1].handle() != Some(handle) =>
            {
                vec![handle]
            }
            _ => vec![],
        };
        let Some(mut storage) = arena.prepare_text_region(ledger, arguments, &private) else {
            return Ok(None);
        };
        let Some((value, _facts)) = storage.concat(arguments[0], arguments[1], !private.is_empty())
        else {
            return Ok(None);
        };
        if !crate::compile::import_result_fits(&function.header.signature.result, value) {
            return Ok(None);
        }
        let prepared = PreparedConcat {
            primitive: self.primitive,
            arguments: [arguments[0], arguments[1]],
            result: value,
            contract,
        };
        let arena = storage.arena();
        let Some(debt) = self.native_charges.bind_measured(
            &[FormulaValues {
                arguments,
                result: value,
            }],
            |_, input, measure| {
                let value = match input {
                    crate::compile::Source::Arg(index) => arguments.get(index),
                    crate::compile::Source::Result => Some(&value),
                    crate::compile::Source::Nothing => None,
                };
                value.map_or(Some(0), |value| match measure {
                    doc::Measure::Size => crate::data::view_size(arena, value).ok(),
                    doc::Measure::DeepSize => crate::data::view_deep_size(arena, value).ok(),
                    doc::Measure::NestedSize => crate::data::view_nested_size(arena, value).ok(),
                    doc::Measure::Magnitude => Some(crate::data::view_magnitude(arena, value)),
                })
            },
        ) else {
            return Ok(None);
        };
        let Some(write_debt) = self.write_charges.bind(&[]) else {
            return Ok(None);
        };
        let Some(mut allowance) = debt.native_allowance(safe_point) else {
            return Ok(None);
        };
        let payment = match original {
            Some(receipt) => match receipt.native_payment(frontier, ledger, &debt, safe_point) {
                Ok(payment) => {
                    allowance = payment.need.clone();
                    Some(payment)
                }
                Err(_) => return Ok(None),
            },
            None => None,
        };
        let Some(units) = allowance.units.checked_add(write_debt.units()) else {
            return Ok(None);
        };
        allowance.units = units;
        let (called, completion) = if let Some(body) = function.kernels(exe).text_body.as_ref() {
            let Some(call) = body.bind_prepared(exe, ledger, arena, invocation, &prepared)? else {
                return Ok(None);
            };
            let (room, completion) = call.room();
            allowance.depth = allowance.depth.max(completion.depth());
            (room, Some(completion))
        } else {
            let Some(room) = CheckedRoom::native(
                exe.lib(self.primitive).specialization.function,
                &prepared.contract,
            ) else {
                return Ok(None);
            };
            (room, None)
        };
        let mut facts = TextFacts {
            released,
            old,
            called,
            callee: function.specialization.resource,
            native: self.first.site.clone(),
            through: self.through.clone(),
            first: self.first.clone(),
            scratch: prepared
                .contract
                .peak_scratch_bytes()
                .ok_or_else(|| fault("text scratch changed"))?,
        };
        let room = match self.interval.evaluate_checked(&mut facts, |_, _, _| Ok(())) {
            Ok(room) => room,
            Err(_) => return Ok(None),
        };
        Ok(Some(PreparedTextWrite {
            plan: self,
            storage,
            invocation: invocation.clone(),
            inputs: retired,
            output,
            value,
            room,
            debt,
            write_debt,
            payment,
            completion,
            allowance,
            before: frontier.position.clone(),
            identity: std::rc::Rc::clone(&frontier.identity),
            revision: frontier.revision,
            next: Position {
                at: self.through.clone(),
                unpaid: Some(self.after.clone()),
                recovery: self.recovery,
                occurrence: frontier.position.occurrence.clone(),
            },
        }))
    }
}

struct TextRoute<'a> {
    source: &'a Source,
    selected: Vec<interval::Selection>,
}
impl evaluate::Events for TextRoute<'_> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = Point {
            site: site.clone(),
            phase,
        };
        let exit = self
            .source
            .room
            .exits
            .iter()
            .find(|exit| exit.point == at && exit.kind != ExitKind::Raise)
            .ok_or(Refused::Source)?
            .kind;
        self.selected.push(interval::Selection::Event { at, exit });
        Ok(exit)
    }
    fn choose(&mut self, _: &Site) -> Result<bool, Refused> {
        Err(Refused::Source)
    }
}
#[cfg(test)]
thread_local! { static TRANSFERS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(crate) fn take_transfers() -> u64 {
    TRANSFERS.with(|count| count.replace(0))
}

struct TextFacts {
    released: u64,
    old: u64,
    called: CheckedRoom,
    callee: CallSource,
    native: Site,
    first: Point,
    through: Point,
    scratch: u64,
}
struct EventFacts<'a> {
    all: &'a TextFacts,
    write: bool,
}
impl Execution for TextFacts {
    fn event(&mut self, at: &Point) -> Option<Box<dyn Facts + '_>> {
        if at == &self.first || at == &self.through {
            Some(Box::new(EventFacts {
                write: at == &self.through,
                all: self,
            }))
        } else {
            None
        }
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
impl Facts for EventFacts<'_> {
    fn measure(&self, measure: &Measure) -> Option<i128> {
        match measure {
            Measure::Release(_) => Some(i128::from(if self.write {
                self.all.old
            } else {
                self.all.released
            })),
            Measure::NativeScratch { .. } => Some(i128::from(self.all.scratch)),
            _ => None,
        }
    }
    fn call(&self, at: &Site, callee: CallSource) -> Option<&CheckedRoom> {
        (at == &self.all.native && callee == self.all.callee && !self.write)
            .then_some(&self.all.called)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}

impl PreparedTextWrite<'_, '_> {
    #[expect(clippy::too_many_arguments, reason = "one affine text/write exit")]
    pub(crate) fn publish(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
        heap: &PagedHeap,
        original: &mut Option<AcceptedOriginalPrefix>,
        resources: SourceResources<'_>,
        memory_bound: u64,
        tape: Option<&crate::observe::ChargeTape>,
    ) -> Result<Option<TextCompletion>, HeapFault> {
        let Self {
            plan,
            storage,
            invocation,
            inputs,
            output,
            value,
            room,
            debt,
            write_debt,
            payment,
            completion,
            allowance,
            before,
            identity,
            revision,
            next,
        } = self;
        if frontier.proven
            || frontier.position != before
            || frontier.revision != revision
            || !std::rc::Rc::ptr_eq(&frontier.identity, &identity)
            || payment
                .as_ref()
                .is_some_and(|payment| !payment.validate_prior(original.as_ref()))
        {
            return Ok(None);
        }
        let moved = storage.moved();
        let outputs = [(output, value)];
        let published = storage.boundary(
            ledger,
            heap,
            &inputs,
            &outputs,
            |mut transaction, ledger, apply| {
                if let Some(completion) = completion {
                    transaction = completion.finish(ledger, heap, transaction)?;
                }
                let balance = transaction.balance()?;
                let before_balance = ledger.balance()?.total()?;
                if i128::from(balance.total()?) - i128::from(before_balance)
                    != room.balance.retained_delta
                    || before_balance
                        .checked_add(room.balance.peak_extra)
                        .is_none_or(|peak| transaction.peak() > peak)
                {
                    return Ok(None);
                }
                let Some(transaction) = transaction.require_source_room(room.balance.peak_extra)
                else {
                    return Ok(None);
                };
                if transaction.peak() > memory_bound {
                    return Ok(None);
                }
                let Some(facts) = resources.facts(&allowance) else {
                    return Ok(None);
                };
                let Some(accepted) = resources.admit_interval(&facts, &allowance, transaction)?
                else {
                    return Ok(None);
                };
                accepted.commit(|transaction| {
                    let revision = frontier
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| fault("text frontier overflow"))?;
                    transaction
                        .admit(memory_bound)
                        .map_err(|_| fault("text accepted room changed"))?
                        .publish(ledger, |arena, edits| apply(arena, edits))?;
                    frontier.position = next;
                    frontier.revision = revision;
                    if let Some(payment) = payment {
                        *original = payment.published(
                            frontier,
                            ledger,
                            original.take(),
                            Box::default(),
                            tape,
                        );
                    } else if let Some(tape) = tape {
                        debt.record(tape);
                    }
                    if let Some(tape) = tape {
                        write_debt.record(tape);
                    }
                    let ownership = match (value, &plan.private_loop) {
                        (Value::LongText(value), Some(_)) => Some(TextOwnership {
                            source: Arc::clone(plan.placement.source()),
                            invocation,
                            stamp: ledger.stamp(),
                            physical: plan.physical,
                            value,
                            loops: frontier.position.occurrence.loops.clone(),
                        }),
                        _ => None,
                    };
                    #[cfg(test)]
                    TRANSFERS.with(|count| count.set(count.get() + moved));
                    Ok(Some(TextCompletion {
                        value,
                        balance,
                        ownership,
                        moved,
                    }))
                })
            },
        )?;
        Ok(published.flatten())
    }
}
