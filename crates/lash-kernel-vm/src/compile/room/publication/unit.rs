//! A tuple comparison owns its temporary graph until one source publication.
use super::super::{
    charges::{ChargeDebt, ChargeProgram, FormulaValues},
    placement::BindingPlacement,
    region::ScalarInput,
};
use super::*;
use crate::compile::{CodeId, Executable, LibId, LibRun, StmtId, expr::Close};
use lash_kernel_exec::Op;
use lash_kernel_state::{
    arena::PreparedBatch,
    kernel_internals::logical::{BoundaryTransaction, Dependencies, Holding, RootValue},
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

type UnitAcceptance<'a, 'r, 'p> = (SourceAccepted<'r, UnitPublication<'a>>, ChargeDebt<'p>);

pub(crate) struct UnitComparison {
    source: Arc<Source>,
    placement: Arc<BindingPlacement>,
    code: CodeId,
    statement: StmtId,
    lib: LibId,
    first: Point,
    tuple: Point,
    comparison: Point,
    test: Point,
    input: ScalarInput,
    intervals: [Arc<CompiledInterval>; 2],
    charges: ChargeProgram,
    recovery: [u8; 32],
}
impl UnitComparison {
    pub(crate) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statement: StmtId,
        pc: u32,
        close: Option<Close>,
    ) -> Result<Arc<Self>, Refused> {
        let code = placement.code();
        let source = Arc::clone(placement.source());
        let (stream, stmt, _) = exe.statement(statement);
        if !Arc::ptr_eq(&source, &exe.code(code).source_room)
            || !matches!(
                stmt.form,
                crate::compile::Form::If { .. } | crate::compile::Form::While { .. }
            )
        {
            return Err(Refused::Source);
        }
        let (lib, args) = match stream.ops.get(pc as usize) {
            Some(Op::Operator { lib, args }) if args.len == 2 => (LibId(lib.0), *args),
            _ => return Err(Refused::Source),
        };
        let function = exe.lib(lib);
        if !function.is_same() || function.limit.is_some() || function.shares_params {
            return Err(Refused::Source);
        }
        let comparison = Point {
            site: (**stream
                .source_sites
                .get(pc as usize)
                .and_then(Option::as_ref)
                .ok_or(Refused::Source)?)
            .clone(),
            phase: Phase::Commit,
        };
        if comparison.site != stmt.site.child(0) {
            return Err(Refused::Source);
        }
        let first = Point {
            site: comparison.site.child(0),
            phase: Phase::Commit,
        };
        let tuple = Point {
            site: comparison.site.child(1),
            phase: Phase::Commit,
        };
        let test = Point {
            site: stmt.site.clone(),
            phase: Phase::Test,
        };
        if source.next_after_commit(&comparison.site)? != test {
            return Err(Refused::Source);
        }
        let slots = source.owner_slots()?;
        let input = source.scalar_input(&slots, &first)?;
        let first_pc = (stmt.ops.start..pc)
            .find(|at| stream.source_sites[*at as usize].as_deref() == Some(&first.site))
            .ok_or(Refused::Source)?;
        match (stream.ops[first_pc as usize], &input) {
            (Op::Local { dst, local }, ScalarInput::Binding(_)) if dst == args.start => {
                placement.verify_read(&first, local.at)?
            }
            (Op::Const { dst, c }, ScalarInput::Literal(expected)) if dst == args.start => {
                if !matches!(stream.consts.get(c.ix()), Some(crate::compile::Constant::Resident(actual)) if actual.same(expected))
                {
                    return Err(Refused::Source);
                }
            }
            _ => return Err(Refused::Source),
        }
        let tuple_pc = (first_pc + 1..pc)
            .find(|at| stream.source_sites[*at as usize].as_deref() == Some(&tuple.site))
            .ok_or(Refused::Source)?;
        if !matches!(stream.ops[tuple_pc as usize], Op::MakeTuple { items } if items.len == 0 && items.start.0 == args.start.0 + 1)
        {
            return Err(Refused::Source);
        }
        let route = [first.clone(), tuple.clone(), comparison.clone()].map(|at| Selection::Event {
            at,
            exit: ExitKind::Normal,
        });
        let full = source.compile_between(&first, &comparison, &route, route.len())?;
        let suffix = source.compile_between(&tuple, &comparison, &route, route.len())?;
        let events: Vec<_> = full.events().collect();
        if events.len() != 3 || suffix.events().count() != 2
            || !events[0].inputs.is_empty() || !events[1].inputs.is_empty()
            || events[0].output.as_ref() != Some(&Owner::Operand(first.site.clone()))
            || events[1].output.as_ref() != Some(&Owner::Operand(tuple.site.clone()))
            || events[2].output.as_ref() != Some(&Owner::Operand(comparison.site.clone()))
            || events[2].inputs.as_ref() != [Owner::Operand(first.site.clone()), Owner::Operand(tuple.site.clone())]
            || !events[2].peak.measures().any(|measure| matches!(measure, Measure::CallPeak { at, callee } if *at == comparison.site && *callee == function.specialization.resource)) {
            return Err(Refused::Source);
        }
        drop(events);
        // The source node, independently of its physical MakeTuple, is empty.
        let mut node = doc::Node::Block(&source.body);
        for index in &tuple.site.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        if !matches!(node, doc::Node::Expr(doc::Expr::Tuple(items)) if items.is_empty()) {
            return Err(Refused::Source);
        }
        let charges = ChargeProgram::native_packet(exe, code, statement, pc, close)?;
        let mut recovery = TokenWriter::new();
        recovery.seq(4);
        recovery.text("lash.source.unit-comparison");
        for point in [&first, &tuple, &comparison] {
            point.site.encode(&mut recovery);
        }
        Ok(Arc::new(Self {
            source,
            placement,
            code,
            statement,
            lib,
            first,
            tuple,
            comparison,
            test,
            input,
            intervals: [full, suffix],
            charges,
            recovery: logical_digest(&recovery.into_bytes()),
        }))
    }
    pub(crate) fn matches_site(
        &self,
        code: CodeId,
        statement: StmtId,
        lib: LibId,
        sites: [&Site; 4],
    ) -> bool {
        self.code == code
            && self.statement == statement
            && self.lib == lib
            && sites
                == [
                    &self.first.site,
                    &self.tuple.site,
                    &self.comparison.site,
                    &self.test.site,
                ]
    }
    pub(crate) fn start(
        &self,
        ledger: &Ledger,
        heap: &PagedHeap,
        occurrence: Occurrence,
    ) -> Result<Position, HeapFault> {
        let left = self.root(&self.first, &occurrence)?;
        let held = ledger.holding(left, heap.pages())?.is_some();
        let at = if held { &self.tuple } else { &self.first };
        Ok(Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: self.recovery,
            occurrence,
        })
    }
    fn root(&self, point: &Point, occurrence: &Occurrence) -> Result<logical::Owner, HeapFault> {
        self.source
            .owner_slots()
            .map_err(|_| fault("unit source owners changed"))?
            .root(
                &Owner::Operand(point.site.clone()),
                occurrence.task,
                occurrence.invocation,
            )
            .ok_or_else(|| fault("unit operand has no source owner"))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one source comparison, actual activation and shared resource window"
    )]
    pub(crate) fn prepare<'a, 'r, 'p>(
        &'p self,
        exe: &Executable,
        code: CodeId,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &'a mut Arena,
        slots: &[crate::machine::SlotState],
        value: doc::Value,
        resources: SourceResources<'r>,
        memory_bound: u64,
    ) -> Result<Option<UnitAcceptance<'a, 'r, 'p>>, HeapFault> {
        if code != self.code
            || frontier.proven
            || !Arc::ptr_eq(&frontier.source, &self.source)
            || frontier.position.recovery != self.recovery
            || frontier.position.occurrence.invocation != invocation.id()
            || invocation.frame()
                != (logical::Owner::Frame {
                    task: frontier.position.occurrence.task,
                    invocation: invocation.id(),
                })
            || matches!(value, doc::Value::Tuple(_))
        {
            return Ok(None);
        }
        let Some(activation) = self
            .placement
            .capture(code, invocation, ledger, heap, slots)?
        else {
            return Ok(None);
        };
        let expected = match &self.input {
            ScalarInput::Literal(value) => Some(*value),
            ScalarInput::Binding(slot) => activation.value(*slot),
        };
        if expected.is_none_or(|expected| !expected.same(&value)) {
            return Ok(None);
        }
        let occurrence = &frontier.position.occurrence;
        let left = self.root(&self.first, occurrence)?;
        let right = self.root(&self.tuple, occurrence)?;
        let output = self.root(&self.comparison, occurrence)?;
        let held = ledger.holding(left, heap.pages())?.is_some();
        let start = if held { &self.tuple } else { &self.first };
        if frontier.position.at != *start
            || frontier.position.unpaid.as_ref() != Some(start)
            || ledger.holding(right, heap.pages())?.is_some()
            || ledger.holding(output, heap.pages())?.is_some()
        {
            return Ok(None);
        }
        let dependencies = Dependencies::capture(
            ledger,
            arena,
            &value.handle().into_iter().collect::<Vec<_>>(),
        )
        .ok_or_else(|| fault("unit input dependency changed"))?;
        let left_bytes = Holding::from_native(arena, heap.pages(), &value)?.bytes();
        let (tuple, mut storage) = arena.prepare_empty_tuple().map_err(unit_native_fault)?;
        let page = crate::machine::project::project_prepared(&mut storage, heap.pages(), &tuple)?;
        let tuple_bytes = Holding::from_page(heap.pages(), &page)?.bytes();
        let values = [value, tuple];
        let result = doc::Value::Bool(false);
        let Some(debt) = self.charges.bind_measured(
            &[FormulaValues {
                arguments: &values,
                result,
            }],
            |_, input, measure| {
                let value = match input {
                    crate::compile::Source::Arg(at) => values.get(at),
                    crate::compile::Source::Result => Some(&result),
                    crate::compile::Source::Nothing => None,
                };
                value.map_or(Some(0), |value| match measure {
                    doc::Measure::Size => crate::data::view_size(&storage, value).ok(),
                    doc::Measure::DeepSize => crate::data::view_deep_size(&storage, value).ok(),
                    doc::Measure::NestedSize => crate::data::view_nested_size(&storage, value).ok(),
                    doc::Measure::Magnitude => Some(crate::data::view_magnitude(&storage, value)),
                })
            },
        ) else {
            return Ok(None);
        };
        let (mut registration, batch) = storage.into_parts();
        let function = exe.lib(self.lib);
        let LibRun::Native { native, .. } = &function.run else {
            return Ok(None);
        };
        let arena = registration.arena();
        let contract = match native.resource_contract(doc::Args::new(
            &values,
            arena.objects(),
            &arena.semantics(),
        )) {
            Ok(Some(contract))
                if contract.events.is_empty()
                    && contract.exit == doc::NativeResourceExit::Returned =>
            {
                contract
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            _ => return Ok(None),
        };
        let Some(called) = CheckedRoom::native(function.specialization.function, &contract) else {
            return Ok(None);
        };
        check_native(
            &EventView {
                inputs: &[left, right],
                outputs: &[(output, logical::PublicationValue::Native(result))],
                borrowed: &[],
                member: None,
                entities: &[],
                frame: None,
                scratch: &[],
                native: Some(&contract),
            },
            &contract,
            heap,
            arena,
        )
        .map_err(|_| fault("unit native output disagrees with its installed contract"))?;
        let empty = ledger.release_cost(&[], heap.pages())?;
        let mut transaction = ledger.prepare_source_boundary(
            registration,
            heap,
            &[],
            &if held {
                vec![]
            } else {
                vec![(left, RootValue::Value(&value))]
            },
        )?;
        transaction = transaction.then_ownership(
            ledger,
            heap.pages(),
            &[],
            &[(right, RootValue::Page(&page))],
        )?;
        let released = transaction.release_cost(&[left, right], heap.pages())?;
        transaction = transaction
            .then_ownership(
                ledger,
                heap.pages(),
                &[left, right],
                &[(output, RootValue::Value(&result))],
            )?
            .bind_dependencies(dependencies)?;
        let mut facts = UnitFacts {
            first: &self.first,
            tuple: &self.tuple,
            comparison: &self.comparison,
            source: function.specialization.resource,
            called: &called,
            left: left_bytes,
            right: tuple_bytes,
            empty,
            released,
        };
        let room = self.intervals[usize::from(held)]
            .evaluate_interval(&mut facts)
            .map_err(|_| fault("unit source facts failed verification"))?;
        let before = ledger.balance()?.total()?;
        let balance = room.balance();
        if i128::from(transaction.balance()?.total()?) - i128::from(before)
            != balance.retained_delta
            || transaction.peak()
                > before
                    .checked_add(balance.peak_extra)
                    .ok_or_else(|| fault("unit source peak overflow"))?
        {
            return Err(fault("unit source room disagrees with its actual roots"));
        }
        transaction = transaction
            .require_source_room(balance.peak_extra)
            .ok_or_else(|| fault("unit source peak overflow"))?;
        if transaction.peak() > memory_bound {
            transaction = transaction.collect(ledger, heap.pages())?;
        }
        if transaction.peak() > memory_bound {
            return Ok(None);
        }
        let Some(allowance) = debt.native_allowance(resources.safe_point) else {
            return Ok(None);
        };
        let Some(facts) = resources.facts(&allowance) else {
            return Ok(None);
        };
        let next = Position {
            at: self.comparison.clone(),
            unpaid: Some(self.test.clone()),
            ..frontier.position.clone()
        };
        Ok(resources
            .admit_interval(
                &facts,
                &allowance,
                UnitPublication {
                    frontier: std::rc::Rc::clone(&frontier.identity),
                    revision: frontier.revision,
                    before: frontier.position.clone(),
                    next,
                    transaction,
                    batch,
                    memory_bound,
                },
            )?
            .map(|accepted| (accepted, debt)))
    }
}

struct UnitFacts<'a> {
    first: &'a Point,
    tuple: &'a Point,
    comparison: &'a Point,
    source: CallSource,
    called: &'a CheckedRoom,
    left: u64,
    right: u64,
    empty: u64,
    released: u64,
}
impl Facts for &UnitFacts<'_> {
    fn dependencies(&self) -> Option<&[doc::arena::ArenaHandle]> {
        Some(&[])
    }
    fn measure(&self, measure: &Measure) -> Option<i128> {
        match measure {
            Measure::Local(Owner::Operand(at)) if *at == self.first.site => Some(self.left.into()),
            Measure::Local(Owner::Operand(at)) if *at == self.tuple.site => Some(self.right.into()),
            Measure::Release(owners) if owners.is_empty() => Some(self.empty.into()),
            Measure::Release(owners)
                if owners.as_ref()
                    == [
                        Owner::Operand(self.first.site.clone()),
                        Owner::Operand(self.tuple.site.clone()),
                    ] =>
            {
                Some(self.released.into())
            }
            _ => None,
        }
    }
    fn call(&self, at: &Site, source: CallSource) -> Option<&CheckedRoom> {
        (*at == self.comparison.site && source == self.source).then_some(self.called)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<lash_kernel_value::FunctionId> {
        None
    }
}
impl Execution for UnitFacts<'_> {
    fn event(&mut self, at: &Point) -> Option<Box<dyn Facts + '_>> {
        ([self.first, self.tuple, self.comparison].contains(&at))
            .then(|| Box::new(&*self) as Box<dyn Facts>)
    }
    fn choose(&mut self, _: &Site) -> Option<bool> {
        None
    }
}
fn unit_native_fault(error: doc::NativeError) -> HeapFault {
    match error {
        doc::NativeError::Host(error) => error,
        _ => fault("unit tuple preparation failed"),
    }
}

pub(crate) struct UnitPublication<'a> {
    frontier: std::rc::Rc<()>,
    revision: u64,
    before: Position,
    next: Position,
    transaction: BoundaryTransaction<'a>,
    batch: PreparedBatch,
    memory_bound: u64,
}
impl SourceAccepted<'_, UnitPublication<'_>> {
    pub(crate) fn publish_unit(
        self,
        frontier: &mut Frontier,
        ledger: &mut Ledger,
    ) -> Result<logical::Balance, HeapFault> {
        self.commit(|mut prepared| {
            if !std::rc::Rc::ptr_eq(&prepared.frontier, &frontier.identity)
                || prepared.revision != frontier.revision
                || frontier.proven
                || prepared.before != frontier.position
                || !prepared.transaction.is_current(ledger)
            {
                return Err(fault("unit source frontier changed"));
            }
            let revision = frontier
                .revision
                .checked_add(1)
                .ok_or_else(|| fault("unit source frontier overflow"))?;
            let balance = prepared.transaction.balance()?;
            prepared
                .transaction
                .admit(prepared.memory_bound)
                .map_err(|_| fault("unit source admission changed"))?
                .publish(ledger, |arena, edits| {
                    assert!(edits.is_empty());
                    prepared.batch.publish(arena);
                })?;
            frontier.revision = revision;
            frontier.position = prepared.next;
            Ok(balance)
        })
    }
}
