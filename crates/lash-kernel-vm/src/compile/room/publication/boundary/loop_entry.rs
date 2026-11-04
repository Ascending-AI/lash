//! Loop control consumes canonical source owners, not physical cursor facts alone.
use super::*;
use crate::compile::room::placement::BindingPlacement;
use crate::compile::room::region::ScalarInput;
use crate::compile::{CodeId, Executable, Form, StmtId, expr};
use lash_kernel_doc::Value;
use lash_kernel_state::kernel_internals::logical::{Holding, Slot};
use std::collections::BTreeMap;

/// Issued only after the actual original iterable close has charged.
pub(crate) struct PaidLoopInput {
    source: Arc<Source>,
    statement: StmtId,
    paid_at: u64,
}
impl PaidLoopInput {
    pub(crate) fn record(
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        close: expr::Close,
        before: u64,
        after: u64,
    ) -> Option<Self> {
        let (stream, stmt, _) = exe.statement(statement);
        if !matches!(stmt.form, Form::For { via: None, .. })
            || exe.stmt_at(code, &stmt.site) != Some(statement)
        {
            return None;
        }
        let mut parsed = expr::statement(stream, stmt.ops, false)?;
        let lash_kernel_exec::Op::ForIn { iterable } = parsed.last else {
            return None;
        };
        let value = parsed.take(iterable)?;
        if !parsed.spent()
            || parsed.closes.as_ref() != [close]
            || !matches!(value.kind, expr::Kind::Local { scoped: false, .. })
        {
            return None;
        }
        let units = if exe.code(code).charged {
            u64::from(close.total)
        } else {
            0
        };
        if before.checked_add(units) != Some(after) {
            return None;
        }
        Some(Self {
            source: Arc::clone(&exe.code(code).source_room),
            statement,
            paid_at: after,
        })
    }
}
struct Path {
    interval: Arc<CompiledInterval>,
    publication: Arc<CompiledPublication>,
}
enum Shape {
    For {
        input: Slot,
        item: Slot,
        item_at: usize,
        read: Point,
        first: Path,
        next: Path,
        end: Path,
    },
    While {
        condition: Slot,
        yes: Path,
        no: Path,
    },
}
pub(crate) struct LoopEntry {
    source: Arc<Source>,
    placement: Arc<BindingPlacement>,
    statement: StmtId,
    site: Site,
    shape: Shape,
}
fn event(at: &Point) -> Selection {
    Selection::Event {
        at: at.clone(),
        exit: ExitKind::Normal,
    }
}
fn path(
    exe: &Executable,
    code: CodeId,
    first: Point,
    last: Point,
    interval: Arc<CompiledInterval>,
    successor: Successor,
    header: Option<(StmtId, bool)>,
) -> Result<Path, Refused> {
    let charges = if let Some((statement, ending)) = header {
        vec![ChargeProgram::loop_header(exe, code, statement, ending)?]
    } else {
        vec![]
    };
    let publication = CompiledPublication::compile(
        exe,
        code,
        first,
        last,
        successor,
        charges,
        BoundaryNeeds::default(),
    )?;
    Ok(Path {
        interval,
        publication,
    })
}
fn slot(source: &Source, owner: &Owner) -> Result<Slot, Refused> {
    match source.owner_slots()?.root(owner, doc::TaskId::MAIN, 0) {
        Some(logical::Owner::Value { slot, .. }) => Ok(slot),
        _ => Err(Refused::Source),
    }
}
impl LoopEntry {
    pub(crate) fn compile(
        exe: &Executable,
        placement: &Arc<BindingPlacement>,
        statement: StmtId,
    ) -> Result<Arc<Self>, Refused> {
        let code = placement.code();
        let source = Arc::clone(placement.source());
        let stmt = exe.stmt(statement);
        if exe.stmt_at(code, &stmt.site) != Some(statement) {
            return Err(Refused::Source);
        }
        let site = stmt.site.clone();
        let test = Point {
            site: site.clone(),
            phase: Phase::Test,
        };
        let shape = match &stmt.form {
            Form::For {
                binding,
                via: None,
                else_block: None,
                ..
            } if !binding.shared => {
                let read = source.first_event(&site.child(0))?;
                if read.site != site.child(0) {
                    return Err(Refused::Source);
                }
                let ScalarInput::Binding(input) =
                    source.scalar_input(source.owner_slots()?.as_ref(), &read)?
                else {
                    return Err(Refused::Source);
                };
                let item = slot(
                    &source,
                    source
                        .room
                        .events
                        .get(&test)
                        .and_then(|e| e.output.as_ref())
                        .ok_or(Refused::Source)?,
                )?;
                let item_at = placement.physical(item).ok_or(Refused::Source)? as usize;
                if item_at != binding.at as usize || !placement.unshared(item_at as u32) {
                    return Err(Refused::Source);
                }
                let selected = [
                    event(&read),
                    Selection::Branch {
                        at: site.clone(),
                        taken: true,
                    },
                    event(&test),
                ];
                let interval = source.compile_between(&read, &test, &selected, 32)?;
                let first = path(
                    exe,
                    code,
                    read.clone(),
                    test.clone(),
                    interval,
                    Successor::Cursor(true),
                    Some((statement, false)),
                )?;
                let next = path(
                    exe,
                    code,
                    test.clone(),
                    test.clone(),
                    source.compile_event(&test, ExitKind::Normal)?,
                    Successor::Cursor(true),
                    Some((statement, false)),
                )?;
                let commit = Point {
                    site: site.clone(),
                    phase: Phase::Commit,
                };
                let end = path(
                    exe,
                    code,
                    commit.clone(),
                    commit.clone(),
                    source.compile_event(&commit, ExitKind::Normal)?,
                    Successor::Normal,
                    Some((statement, true)),
                )?;
                Shape::For {
                    input,
                    item,
                    item_at,
                    read,
                    first,
                    next,
                    end,
                }
            }
            Form::While { .. } => {
                let condition = slot(&source, &Owner::Operand(site.child(0)))?;
                let interval = source.compile_event(&test, ExitKind::Normal)?;
                let yes = path(
                    exe,
                    code,
                    test.clone(),
                    test.clone(),
                    Arc::clone(&interval),
                    Successor::Condition(true),
                    None,
                )?;
                let no = path(
                    exe,
                    code,
                    test.clone(),
                    test.clone(),
                    interval,
                    Successor::Condition(false),
                    None,
                )?;
                Shape::While { condition, yes, no }
            }
            _ => return Err(Refused::Source),
        };
        Ok(Arc::new(Self {
            source,
            placement: Arc::clone(placement),
            statement,
            site,
            shape,
        }))
    }
    /// The source bank already owns this read's holding and its canonical
    /// close has paid. Adopt that occurrence before publishing the header.
    #[expect(
        clippy::too_many_arguments,
        reason = "one paid initial iterable and its actual source activation"
    )]
    pub(crate) fn adopt_paid_input(
        &self,
        frontier: &mut Frontier,
        invocation: &Invocation,
        ledger: &mut Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        slots: &[crate::machine::SlotState],
        walked: Value,
        paid: Option<&PaidLoopInput>,
        charged: u64,
    ) -> Result<bool, HeapFault> {
        let Shape::For {
            input, read, first, ..
        } = &self.shape
        else {
            return Ok(false);
        };
        let Some(paid) = paid else { return Ok(false) };
        if frontier.proven
            || !Arc::ptr_eq(&self.source, &frontier.source)
            || frontier.position.unpaid.as_ref() != Some(read)
            || frontier.position.occurrence.invocation != invocation.id()
            || !Arc::ptr_eq(&paid.source, &self.source)
            || paid.statement != self.statement
            || paid.paid_at != charged
        {
            return Ok(false);
        }
        let Some(activation) =
            self.placement
                .capture(self.placement.code(), invocation, ledger, heap, slots)?
        else {
            return Ok(false);
        };
        if !activation.current(&self.source, ledger, invocation)
            || activation
                .value(*input)
                .is_none_or(|actual| !actual.same(&walked))
            || !holding_matches(ledger, arena, heap, invocation.owner(*input), walked)?
        {
            return Ok(false);
        }
        let operand = slot(&self.source, &Owner::Operand(read.site.clone()))
            .map_err(|_| fault("initial iterable owner"))?;
        if !holding_matches(ledger, arena, heap, invocation.owner(operand), walked)? {
            return Ok(false);
        }
        let (Value::List(handle) | Value::Tuple(handle)) = walked else {
            return Ok(false);
        };
        if arena
            .item(handle, 0)
            .map_err(|error| HeapFault::Malformed(error.to_string()))?
            .is_none()
        {
            return Ok(false);
        }
        let Some(next) = first
            .interval
            .events()
            .last()
            .map(|event| event.point.clone())
        else {
            return Ok(false);
        };
        if next
            != (Point {
                site: self.site.clone(),
                phase: Phase::Test,
            })
        {
            return Ok(false);
        }
        let mut position = frontier.position.clone();
        position.at = read.clone();
        position.unpaid = Some(next);
        frontier.handoff(ledger, position)?;
        Ok(true)
    }
    pub(crate) fn item_at(&self) -> Option<usize> {
        match self.shape {
            Shape::For { item_at, .. } => Some(item_at),
            _ => None,
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one source-authenticated loop boundary"
    )]
    pub(crate) fn bind(
        &self,
        frontier: &Frontier,
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        slots: &[crate::machine::SlotState],
        started: u64,
        cursor: Option<(Value, u64)>,
        value: Option<Value>,
        input_paid: Option<&PaidLoopInput>,
        charged: u64,
    ) -> Result<Option<BoundPublication>, HeapFault> {
        let fail = || HeapFault::Malformed("loop source ownership disappeared".into());
        if frontier.proven
            || !Arc::ptr_eq(&self.source, &frontier.source)
            || frontier.position.occurrence.invocation != invocation.id()
        {
            return Ok(None);
        }
        let Some(activation) =
            self.placement
                .capture(self.placement.code(), invocation, ledger, heap, slots)?
        else {
            return Ok(None);
        };
        if !activation.current(&self.source, ledger, invocation) {
            return Ok(None);
        }
        let before = &frontier.position;
        let root = |s| invocation.owner(s);
        let mut outputs = Vec::new();
        let mut inputs = Vec::new();
        let mut measures = BTreeMap::new();
        let mut dependencies = Vec::new();
        let mut next_occurrence = before.occurrence.clone();
        let mut new_read = false;
        let (plan, entering) = match &self.shape {
            Shape::For {
                input,
                item,
                item_at: _,
                read,
                first,
                next,
                end,
            } => {
                let Some((walked, position)) = cursor else {
                    return Ok(None);
                };
                if position != started {
                    return Ok(None);
                }
                let (Value::List(handle) | Value::Tuple(handle)) = walked else {
                    return Ok(None);
                };
                let actual = arena
                    .item(handle, started)
                    .map_err(|e| HeapFault::Malformed(e.to_string()))?;
                if !match (actual, value) {
                    (Some(a), Some(b)) => a.same(&b),
                    (None, None) => true,
                    _ => false,
                } {
                    return Ok(None);
                }
                dependencies.push(handle);
                let operand =
                    slot(&self.source, &Owner::Operand(self.site.child(0))).map_err(|_| fail())?;
                let plan = if value.is_some() {
                    if !matches!(
                        activation.state(*item),
                        Some(crate::machine::SlotState::Empty)
                    ) {
                        return Ok(None);
                    }
                    if before.unpaid.as_ref() == Some(read) {
                        if started != 0 {
                            return Ok(None);
                        }
                        let Some(paid) = input_paid else {
                            return Ok(None);
                        };
                        if !Arc::ptr_eq(&paid.source, &self.source)
                            || paid.statement != self.statement
                            || paid.paid_at > charged
                        {
                            return Ok(None);
                        }
                        let Some(actual) = activation.value(*input) else {
                            return Ok(None);
                        };
                        if !walked.same(&actual)
                            || !holding_matches(ledger, arena, heap, root(*input), walked)?
                        {
                            return Ok(None);
                        }
                        outputs.push((root(operand), PublicationValue::Existing(root(*input))));
                        measures.insert(
                            (
                                read.clone(),
                                Measure::Local(Owner::Operand(read.site.clone())),
                            ),
                            i128::from(arena.logical_local(&walked)?),
                        );
                        new_read = true;
                        first
                    } else {
                        next
                    }
                } else {
                    end
                };
                if !new_read && !holding_matches(ledger, arena, heap, root(operand), walked)? {
                    return Ok(None);
                }
                if let Some(value) = value {
                    let test = Point {
                        site: self.site.clone(),
                        phase: Phase::Test,
                    };
                    let selector = self.source.room.events[&test]
                        .output
                        .as_ref()
                        .ok_or_else(fail)?
                        .clone();
                    measures.insert(
                        (test, Measure::Local(selector)),
                        i128::from(arena.logical_local(&value)?),
                    );
                    outputs.push((root(*item), PublicationValue::Native(value)));
                } else {
                    inputs.push(root(operand));
                    let commit = Point {
                        site: self.site.clone(),
                        phase: Phase::Commit,
                    };
                    measures.insert(
                        (
                            commit,
                            Measure::Release(vec![Owner::Operands(self.site.clone())].into()),
                        ),
                        i128::from(ledger.release_cost(&inputs, heap.pages())?),
                    );
                }
                (plan, value.is_some())
            }
            Shape::While { condition, yes, no } => {
                let Some(Value::Bool(taken)) = value else {
                    return Ok(None);
                };
                if !holding_matches(ledger, arena, heap, root(*condition), Value::Bool(taken))? {
                    return Ok(None);
                }
                inputs.push(root(*condition));
                let test = Point {
                    site: self.site.clone(),
                    phase: Phase::Test,
                };
                measures.insert(
                    (
                        test,
                        Measure::Release(vec![Owner::Operand(self.site.child(0))].into()),
                    ),
                    i128::from(ledger.release_cost(&inputs, heap.pages())?),
                );
                (if taken { yes } else { no }, taken)
            }
        };
        if entering && started == u64::MAX {
            return Ok(None);
        }
        // The body ordinal follows a genuine loop-header occurrence. A backedge
        // advances exactly one completed body; another loop cannot supply it.
        if let Some((at, previous)) = next_occurrence.loops.last() {
            if at == &self.site {
                if *previous != started && previous.checked_add(1) != Some(started) {
                    return Ok(None);
                }
                next_occurrence.loops.pop();
            } else if started != 0 {
                return Ok(None);
            }
        } else if started != 0 {
            return Ok(None);
        }
        if entering {
            next_occurrence.loops.push((self.site.clone(), started));
        }
        let mut execution = LoopFacts {
            values: measures,
            cursor: new_read.then_some(self.site.clone()),
        };
        let source_slots = self.source.owner_slots().map_err(|_| fail())?;
        let Ok(room) = plan
            .interval
            .evaluate_checked(&mut execution, |event, site, facts| {
                let event_outputs: Vec<_> = outputs
                    .iter()
                    .filter(|(owner, _)| {
                        event.output.as_ref().and_then(|o| {
                            source_slots.root(o, before.occurrence.task, invocation.id())
                        }) == Some(*owner)
                    })
                    .cloned()
                    .collect();
                let event_inputs = if matches!(self.shape, Shape::While { .. })
                    || (event.point.phase == Phase::Commit && event.point.site == self.site)
                {
                    &inputs[..]
                } else {
                    &[]
                };
                EventContext {
                    earlier: &[],
                    source: &self.source,
                    slots: &source_slots,
                    occurrence: &before.occurrence,
                    ledger,
                    heap,
                    arena,
                }
                .check(
                    event,
                    site,
                    &EventView {
                        inputs: event_inputs,
                        outputs: &event_outputs,
                        borrowed: &[],
                        member: None,
                        entities: &[],
                        frame: None,
                        scratch: &[],
                        native: None,
                    },
                    facts,
                )
            })
        else {
            return Ok(None);
        };
        let Some(dependencies) = Dependencies::capture(ledger, arena, &dependencies) else {
            return Ok(None);
        };
        let debts = plan
            .publication
            .charges()
            .iter()
            .map(|p| p.bind(&[]))
            .collect::<Option<Vec<_>>>();
        let Some(debts) = debts else { return Ok(None) };
        let evidence = BoundaryEvidence {
            invocation: invocation.clone(),
            room,
            dependencies,
            inputs,
            outputs,
            members: vec![],
        };
        let proposed = plan.publication.exit(before.occurrence.clone());
        let Some(mut bound) = plan
            .publication
            .bind(frontier, ledger, arena, heap, evidence, debts, proposed)?
        else {
            return Ok(None);
        };
        bound.cursor.next.occurrence = next_occurrence;
        Ok(Some(bound))
    }
}
fn holding_matches(
    ledger: &Ledger,
    arena: &mut Arena,
    heap: &PagedHeap,
    owner: logical::Owner,
    value: Value,
) -> Result<bool, HeapFault> {
    let Some(holding) = ledger.holding(owner, heap.pages())? else {
        return Ok(false);
    };
    let expected = Holding::from_native(arena, heap.pages(), &value)?;
    if holding.bytes() != expected.bytes() {
        return Ok(false);
    }
    let mut left = BTreeMap::new();
    let mut right = BTreeMap::new();
    holding.visit_edges(heap.pages(), &mut |id, count| {
        left.insert(id, count);
        Ok(())
    })?;
    expected.visit_edges(heap.pages(), &mut |id, count| {
        right.insert(id, count);
        Ok(())
    })?;
    Ok(left == right)
}
struct LoopFacts {
    values: BTreeMap<(Point, Measure), i128>,
    cursor: Option<Site>,
}
impl Execution for LoopFacts {
    fn event(&mut self, at: &Point) -> Option<Box<dyn Facts + '_>> {
        Some(Box::new(At {
            at: at.clone(),
            values: &self.values,
        }))
    }
    fn choose(&mut self, at: &Site) -> Option<bool> {
        (self.cursor.as_ref() == Some(at)).then_some(true)
    }
}
struct At<'a> {
    at: Point,
    values: &'a BTreeMap<(Point, Measure), i128>,
}
impl Facts for At<'_> {
    fn dependencies(&self) -> Option<&[doc::arena::ArenaHandle]> {
        Some(&[])
    }
    fn measure(&self, m: &Measure) -> Option<i128> {
        self.values.get(&(self.at.clone(), m.clone())).copied()
    }
    fn call(&self, _: &Site, _: CallSource) -> Option<&CheckedRoom> {
        None
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<FunctionId> {
        None
    }
}
