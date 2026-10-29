//! Recognize a stable merge from its complete source data flow. The fast
//! steps retain the source body and callback coordinates for every exit.
use super::*;

#[derive(Clone)]
pub(crate) enum CollectionStep {
    Merge(Merge),
    Place(Placement),
    Visit(Visit),
    ReadSlot(ReadSlot),
    Reduce(Reduction),
    Append(Append),
}

#[cfg(any(test, feature = "testing"))]
impl CollectionStep {
    pub(in crate::machine::islands) fn family(&self) -> crate::machine::islands::witness::Family {
        use crate::machine::islands::witness::Family;
        match self {
            Self::Merge(_) => Family::Merge,
            Self::Place(_) => Family::Placement,
            Self::Visit(_) => Family::Visit,
            Self::ReadSlot(_) => Family::ReadSlot,
            Self::Reduce(_) => Family::Reduction,
            Self::Append(_) => Family::CollectionAppend,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Append {
    pub(in crate::machine::islands) target: usize,
    pub(in crate::machine::islands) item: usize,
    pub(in crate::machine::islands) body: usize,
}

#[derive(Clone)]
pub(crate) struct Reduction {
    pub(in crate::machine::islands) accumulator: usize,
    pub(in crate::machine::islands) item: usize,
    pub(in crate::machine::islands) body: usize,
    pub(in crate::machine::islands) lib: LibId,
    pub(in crate::machine::islands) checked: bool,
    pub(in crate::machine::islands) reverse: bool,
    pub(in crate::machine::islands) depth: Depth,
}

#[derive(Clone)]
pub(crate) struct ReadSlot {
    pub(in crate::machine::islands) bound: LibId,
    pub(in crate::machine::islands) mark: LibId,
    pub(in crate::machine::islands) target: usize,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) list: usize,
    pub(in crate::machine::islands) value: usize,
    pub(in crate::machine::islands) present: usize,
    pub(in crate::machine::islands) body: usize,
}

#[derive(Clone)]
pub(crate) struct Visit {
    pub(in crate::machine::islands) bound: LibId,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) length: usize,
    pub(in crate::machine::islands) value: usize,
    pub(in crate::machine::islands) body: usize,
    pub(in crate::machine::islands) arms: [Option<usize>; 2],
    pub(in crate::machine::islands) after: [usize; 2],
}

#[derive(Clone)]
pub(crate) struct Placement {
    pub(in crate::machine::islands) target: usize,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) value: usize,
    pub(in crate::machine::islands) list: Option<usize>,
    pub(in crate::machine::islands) body: usize,
    pub(in crate::machine::islands) statements: usize,
    pub(in crate::machine::islands) advance: LibId,
    pub(in crate::machine::islands) increment: Value,
}

#[derive(Clone)]
pub(crate) struct Merge {
    pub(in crate::machine::islands) start: LibId,
    pub(in crate::machine::islands) sums: [LibId; 2],
    pub(in crate::machine::islands) minimum: [LibId; 2],
    pub(in crate::machine::islands) tests: [LibId; 3],
    pub(in crate::machine::islands) advances: [LibId; 3], // right, left, destination
    pub(in crate::machine::islands) counters: [usize; 7], // dest, end, mid, left, right, width, n
    pub(in crate::machine::islands) lists: [usize; 2],    // sorted, out
    pub(in crate::machine::islands) from_right: usize,
    pub(in crate::machine::islands) items: [usize; 2],
    pub(in crate::machine::islands) order: usize,
    pub(in crate::machine::islands) body: usize,
    pub(in crate::machine::islands) compared: [usize; 2],
    pub(in crate::machine::islands) callback: usize,
}

impl Build<'_> {
    pub(in crate::machine::islands) fn merges(&mut self, source: &SourceBlock) {
        for statement in &source.statements {
            match &statement.kind {
                StatementKind::For { body, .. } => {
                    if let Some(append) = self.append(statement, body) {
                        let head = self.blocks[append.body].starts[0] - 1;
                        if self.ops[head].kernel.is_none()
                            && matches!(self.ops[head].op, Op::Open(block) if block == append.body)
                        {
                            self.ops[head].kernel = Some(CollectionKernel {
                                step: std::sync::Arc::new(CollectionStep::Append(append)),
                                tail: false,
                            });
                        }
                    }
                    if let Some(reduction) = self.reduction(statement, body) {
                        let head = self.blocks[reduction.body].starts[0] - 1;
                        if self.ops[head].kernel.is_none()
                            && matches!(self.ops[head].op, Op::Open(block) if block == reduction.body)
                        {
                            self.ops[head].kernel = Some(CollectionKernel {
                                step: std::sync::Arc::new(CollectionStep::Reduce(reduction)),
                                tail: false,
                            });
                        }
                    }
                    if let Some(merge) = self.merge(body) {
                        let head = self.blocks[merge.body].starts[0] - 1;
                        if self.ops[head].kernel.is_some()
                            || !matches!(self.ops[head].op, Op::Open(block) if block == merge.body)
                        {
                            continue;
                        }
                        let tail = self.blocks[merge.compared[1]].starts[4];
                        let step = std::sync::Arc::new(CollectionStep::Merge(merge));
                        self.ops[head].kernel = Some(CollectionKernel {
                            step: step.clone(),
                            tail: false,
                        });
                        self.ops[tail].kernel = Some(CollectionKernel { step, tail: true });
                    } else if let Some(placement) = self.placement(statement, body) {
                        let head = self.blocks[placement.body].starts[0] - 1;
                        if self.ops[head].kernel.is_none()
                            && matches!(self.ops[head].op, Op::Open(block) if block == placement.body)
                        {
                            self.ops[head].kernel = Some(CollectionKernel {
                                step: std::sync::Arc::new(CollectionStep::Place(placement)),
                                tail: false,
                            });
                        }
                    }
                    if let Some(visit) = self.visit(statement, body) {
                        let head = self.blocks[visit.body].starts[0] - 1;
                        if self.ops[head].kernel.is_none()
                            && matches!(self.ops[head].op, Op::Open(block) if block == visit.body)
                        {
                            self.ops[head].kernel = Some(CollectionKernel {
                                step: std::sync::Arc::new(CollectionStep::Visit(visit)),
                                tail: false,
                            });
                        }
                    }
                    self.merges(body);
                }
                StatementKind::While { body, .. } => {
                    if let Some(read) = self.read_slot(body) {
                        let head = self.blocks[read.body].starts[0] - 1;
                        if self.ops[head].kernel.is_none()
                            && matches!(self.ops[head].op, Op::Open(block) if block == read.body)
                        {
                            self.ops[head].kernel = Some(CollectionKernel {
                                step: std::sync::Arc::new(CollectionStep::ReadSlot(read)),
                                tail: false,
                            });
                        }
                    }
                    self.merges(body);
                }
                StatementKind::If { then_, else_, .. } => {
                    self.merges(then_);
                    self.merges(else_);
                }
                StatementKind::Try {
                    body,
                    catch,
                    finally,
                } => {
                    self.merges(body);
                    if let Some(block) = catch {
                        self.merges(block);
                    }
                    if let Some(block) = finally {
                        self.merges(block);
                    }
                }
                _ => {}
            }
        }
    }

    fn append(&self, loop_: &Statement, body: &SourceBlock) -> Option<Append> {
        let StatementKind::For { at: item, .. } = loop_.kind else {
            return None;
        };
        body.declares.iter().all(|at| *at == item).then_some(())?;
        let [write] = body.statements.as_ref() else {
            return None;
        };
        write.root.is_none().then_some(())?;
        let StatementKind::Store {
            operands,
            target,
            index: Some(index),
            value,
            field: None,
            ..
        } = &write.kind
        else {
            return None;
        };
        let read = |reg| operands.iter().find(|expr| expr.reg == reg);
        let target = local(read(*target)?)?;
        (local(read(*value)?)? == item
            && !self.shared[target as usize]
            && !self.shared[item as usize])
            .then_some(())?;
        let Kind::Native { lib, args, .. } = &unpin(read(*index)?).kind else {
            return None;
        };
        self.exe.lib(*lib).length_read()?;
        let [sequence] = &**args else { return None };
        (local(sequence)? == target).then_some(())?;
        Some(Append {
            target: target as usize,
            item: item as usize,
            body: self.blocks.iter().position(|block| block.id == body.id)?,
        })
    }

    fn reduction(&self, loop_: &Statement, body: &SourceBlock) -> Option<Reduction> {
        let StatementKind::For { at: item, .. } = loop_.kind else {
            return None;
        };
        body.declares.iter().all(|at| *at == item).then_some(())?;
        let [write] = body.statements.as_ref() else {
            return None;
        };
        let StatementKind::Write {
            close: None,
            bind: false,
            ..
        } = write.kind
        else {
            return None;
        };
        let (accumulator, value) = assigned(write, false)?;
        (item != accumulator && !self.shared[item as usize] && !self.shared[accumulator as usize])
            .then_some(())?;
        let value = unpin(value);
        let (lib, args, checked) = match &value.kind {
            Kind::Operator {
                lib, args, invoke, ..
            } => (*lib, &**args, invoke.is_some_and(|invoke| invoke.checked)),
            Kind::Call { invoke, args, .. } => {
                let lib = invoke
                    .act
                    .and_then(|act| self.stream.nested[act.ix()].kind.operator_lib())
                    .or_else(|| match &self.exe.stmt(write.id).form {
                        crate::compile::Form::Action(action) => match &action.kind {
                            crate::compile::ActionKind::Call { callee, .. } => callee.library(),
                            _ => None,
                        },
                        _ => None,
                    })?;
                (lib, &**args, invoke.checked)
            }
            _ => return None,
        };
        let [first, second] = args else { return None };
        let roles = [local(first)?, local(second)?];
        let reverse = if roles == [accumulator, item] {
            false
        } else if roles == [item, accumulator] {
            true
        } else {
            return None;
        };
        let function = self.exe.lib(lib);
        function.limit.is_none().then_some(())?;
        let (entry, _) = function.specialization.prepared.as_ref()?;
        entry.immediate().then_some(())?;
        let depth = match function.run {
            LibRun::Body(_) => Depth::Template(super::super::answer_depth(self.exe, lib)?),
            LibRun::Native { .. } => {
                let extra = entry.depth();
                if function.operation() {
                    Depth::Operation(extra)
                } else if extra > 0 {
                    Depth::Template(extra)
                } else {
                    Depth::None
                }
            }
            _ => return None,
        };
        Some(Reduction {
            accumulator: accumulator as usize,
            item: item as usize,
            body: self.blocks.iter().position(|block| block.id == body.id)?,
            lib,
            checked,
            reverse,
            depth,
        })
    }

    fn read_slot(&self, body: &SourceBlock) -> Option<ReadSlot> {
        let [present, value, read, rest @ ..] = &*body.statements else {
            return None;
        };
        (!rest.is_empty()).then_some(())?;
        let (present, initial) = assigned(present, true)?;
        matches!(initial.kind, Kind::Const(Value::Bool(false))).then_some(())?;
        let (value, initial) = assigned(value, true)?;
        matches!(initial.kind, Kind::Const(Value::Absent)).then_some(())?;
        let (condition, yes, _) = branch(read)?;
        let list = local(condition)?;
        let [bound] = plain(yes)?;
        let (condition, yes, no) = branch(bound)?;
        empty(no)?;
        let bound = self.primitive(condition, 2)?;
        let [index, length] = self.merge_args(condition, "num.lt")?;
        let index = local(index)?;
        let Kind::Native { lib, args, .. } = &unpin(length).kind else {
            return None;
        };
        self.exe.lib(*lib).length_read()?;
        let [target] = &**args else {
            return None;
        };
        let target = local(target)?;
        let [get, mark] = plain(yes)?;
        let (at, item) = assigned(get, false)?;
        (at == value && get.root.is_none()).then_some(())?;
        let Kind::Index {
            target: source,
            index: position,
            ..
        } = &unpin(item).kind
        else {
            return None;
        };
        (local(source)? == target && local(position)? == index).then_some(())?;
        let (at, test) = assigned(mark, false)?;
        (at == present).then_some(())?;
        let mark = self.primitive(test, 1)?;
        let Kind::Operator { args, .. } = &unpin(test).kind else {
            return None;
        };
        let [test] = &**args else {
            return None;
        };
        let Kind::Operator { lib, args, .. } = &unpin(test).kind else {
            return None;
        };
        crate::compile::islands::unit_test(Some(self.exe.lib(*lib)), args).then_some(())?;
        (local(&args[0])? == value).then_some(())?;
        let mut roles = [target, index, list, value, present];
        roles.sort_unstable();
        (roles.windows(2).all(|pair| pair[0] != pair[1])
            && roles.iter().all(|at| !self.shared[*at as usize]))
        .then_some(())?;
        Some(ReadSlot {
            bound,
            mark,
            target: target as usize,
            index: index as usize,
            list: list as usize,
            value: value as usize,
            present: present as usize,
            body: self.blocks.iter().position(|block| block.id == body.id)?,
        })
    }

    fn visit(&self, loop_: &Statement, body: &SourceBlock) -> Option<Visit> {
        let StatementKind::For { at: value, .. } = loop_.kind else {
            return None;
        };
        let [bound, hole, rest @ ..] = &*body.statements else {
            return None;
        };
        (!rest.is_empty()).then_some(())?;
        let (condition, yes, no) = branch(bound)?;
        let bound = self.primitive(condition, 2)?;
        let [index, length] = self.merge_locals(condition, "same")?;
        empty(no)?;
        let [stop] = plain(yes)?;
        matches!(
            stop.kind,
            StatementKind::Depart {
                continuing: false,
                ..
            }
        )
        .then_some(())?;
        let (condition, yes, no) = branch(hole)?;
        let Kind::Operator { lib, args, .. } = &unpin(condition).kind else {
            return None;
        };
        crate::compile::islands::unit_test(Some(self.exe.lib(*lib)), args).then_some(())?;
        (local(&args[0])? == value).then_some(())?;
        let mut roles = [index, length, value];
        roles.sort_unstable();
        (roles.windows(2).all(|pair| pair[0] != pair[1])
            && roles.iter().all(|at| !self.shared[*at as usize]))
        .then_some(())?;
        let block = self.blocks.iter().position(|block| block.id == body.id)?;
        let arm = |source: &SourceBlock| {
            if source.statements.is_empty() {
                Some(None)
            } else {
                Some(Some(
                    self.blocks.iter().position(|block| block.id == source.id)?,
                ))
            }
        };
        let arms = [arm(yes)?, arm(no)?];
        Some(Visit {
            bound,
            index: index as usize,
            length: length as usize,
            value: value as usize,
            body: block,
            arms,
            after: arms.map(|arm| {
                arm.map_or(self.blocks[block].starts[2], |block| {
                    self.blocks[block].starts[0]
                })
            }),
        })
    }

    fn placement(&self, loop_: &Statement, body: &SourceBlock) -> Option<Placement> {
        if let Some(place) = self.indexed_placement(loop_, body) {
            return Some(place);
        }
        let StatementKind::For { at: value, .. } = loop_.kind else {
            return None;
        };
        let [initialise, test, place, advance] = &*body.statements else {
            return None;
        };
        let (in_place, initial) = assigned(initialise, true)?;
        matches!(initial.kind, Kind::Const(Value::Bool(false))).then_some(())?;
        let (condition, yes, no) = branch(test)?;
        let list = local(condition)?;
        empty(no)?;
        let [set] = plain(yes)?;
        let (at, condition) = assigned(set, false)?;
        (at == in_place).then_some(())?;
        let [index, length] = self.merge_args(condition, "num.lt")?;
        let index = local(index)?;
        let Kind::Native { lib, args, .. } = &unpin(length).kind else {
            return None;
        };
        self.exe.lib(*lib).length_read()?;
        let [target] = &**args else {
            return None;
        };
        let target = local(target)?;
        let (condition, yes, _) = branch(place)?;
        (local(condition)? == in_place).then_some(())?;
        let [store] = plain(yes)?;
        let StatementKind::Store {
            operands,
            target: to,
            index: Some(at),
            value: from,
            field: None,
            ..
        } = &store.kind
        else {
            return None;
        };
        store.root.is_none().then_some(())?;
        let read = |reg| operands.iter().find(|expr| expr.reg == reg).and_then(local);
        (read(*to)? == target && read(*at)? == index && read(*from)? == value).then_some(())?;
        let (to, addition) = assigned(advance, false)?;
        let [from, one] = self.merge_args(addition, "num.add")?;
        let Kind::Const(increment) = one.kind else {
            return None;
        };
        let one = matches!(increment, Value::Int(1))
            || matches!(increment, Value::Float(value) if value.get() == 1.0);
        (to == index && local(from)? == index && one).then_some(())?;
        let Kind::Operator { lib, .. } = &unpin(addition).kind else {
            return None;
        };
        self.exe.lib(*lib).specialization.prepared.as_ref()?;
        let advance = *lib;
        let mut roles = [target, index, value, list, in_place];
        roles.sort_unstable();
        (roles.windows(2).all(|pair| pair[0] != pair[1])
            && roles.iter().all(|at| !self.shared[*at as usize]))
        .then_some(())?;
        Some(Placement {
            target: target as usize,
            index: index as usize,
            value: value as usize,
            list: Some(list as usize),
            body: self.blocks.iter().position(|block| block.id == body.id)?,
            statements: 4,
            advance,
            increment,
        })
    }

    fn indexed_placement(&self, loop_: &Statement, body: &SourceBlock) -> Option<Placement> {
        let StatementKind::For { at: value, .. } = loop_.kind else {
            return None;
        };
        body.declares.iter().all(|at| *at == value).then_some(())?;
        let [store, advance] = body.statements.as_ref() else {
            return None;
        };
        store.root.is_none().then_some(())?;
        let StatementKind::Store {
            operands,
            target,
            index: Some(index),
            value: item,
            field: None,
            ..
        } = &store.kind
        else {
            return None;
        };
        let read = |reg| operands.iter().find(|expr| expr.reg == reg).and_then(local);
        let (target, index) = (read(*target)?, read(*index)?);
        (read(*item)? == value).then_some(())?;
        let (to, addition) = assigned(advance, false)?;
        let (lib, args) = match &unpin(addition).kind {
            Kind::Operator { lib, args, .. } | Kind::Native { lib, args, .. } => (*lib, args),
            _ => return None,
        };
        let [from, one] = args.as_ref() else {
            return None;
        };
        (to == index && local(from)? == index && matches!(one.kind, Kind::Const(Value::Int(1))))
            .then_some(())?;
        let function = self.exe.lib(lib);
        (function.limit.is_none()
            && !function.operation()
            && matches!(function.run, LibRun::Native { .. }))
        .then_some(())?;
        let advance = &function.specialization.prepared.as_ref()?.0;
        (advance.depth() == 0
            && advance.pair()?.ints == Some(lash_kernel_doc::scalar::ScalarOp::Add))
        .then_some(())?;
        let mut roles = [target, index, value];
        roles.sort_unstable();
        (roles.windows(2).all(|pair| pair[0] != pair[1])
            && roles.iter().all(|at| !self.shared[*at as usize]))
        .then_some(())?;
        Some(Placement {
            target: target as usize,
            index: index as usize,
            value: value as usize,
            list: None,
            body: self.blocks.iter().position(|block| block.id == body.id)?,
            statements: 2,
            advance: lib,
            increment: Value::Int(1),
        })
    }

    fn merge(&self, body: &SourceBlock) -> Option<Merge> {
        let [head, start, compare, place, advance] = &*body.statements else {
            return None;
        };
        let (cond, then_, otherwise) = branch(head)?;
        let start_test = self.primitive(cond, 2)?;
        let [dest, end] = self.merge_locals(cond, "same")?;
        empty(otherwise)?;
        let [set_mid, set_end, set_left, set_right] = plain(then_)?;
        let (mid, [dest2, width, n], sum_mid, min_mid) = self.bound_sum(set_mid)?;
        let (end2, [mid2, width2, n2], sum_end, min_end) = self.bound_sum(set_end)?;
        let (left, from) = assigned(set_left, false)?;
        let (right, from2) = assigned(set_right, false)?;
        (dest2 == dest
            && end2 == end
            && mid2 == mid
            && width2 == width
            && n2 == n
            && local(from)? == dest
            && local(from2)? == mid)
            .then_some(())?;
        let (from_right, value) = assigned(start, true)?;
        matches!(value.kind, Kind::Const(Value::Bool(false))).then_some(())?;
        let (cond, outer, otherwise) = branch(compare)?;
        let left_test = self.primitive(cond, 2)?;
        (self.merge_locals(cond, "num.lt")? == [left, mid]).then_some(())?;
        sets_true(otherwise, from_right)?;
        let [inner] = plain(outer)?;
        let (cond, comparison, otherwise) = branch(inner)?;
        let right_test = self.primitive(cond, 2)?;
        (self.merge_locals(cond, "num.lt")? == [right, end]).then_some(())?;
        empty(otherwise)?;
        let [first, second, apply, coerce, test] = &*comparison.statements else {
            return None;
        };
        first.root.is_none().then_some(())?;
        second.root.is_none().then_some(())?;
        let (first_slot, first_value) = assigned(first, true)?;
        let (second_slot, second_value) = assigned(second, true)?;
        let [sorted, at] = self.merge_locals(first_value, "list.get")?;
        (at == left && self.merge_locals(second_value, "list.get")? == [sorted, right])
            .then_some(())?;
        // The callback and coercion themselves always run their original
        // instructions. Only their surrounding pure bookkeeping is fused.
        let (output, _) = assigned(apply, true)?;
        let (order, _) = assigned(coerce, true)?;
        let (positive, yes, no) = branch(test)?;
        let order_test = self.primitive(positive, 2)?;
        let [zero, ordered] = self.merge_args(positive, "num.lt")?;
        (matches!(zero.kind, Kind::Const(Value::Int(0))) && local(ordered)? == order)
            .then_some(())?;
        sets_true(yes, from_right)?;
        empty(no)?;
        let (cond, yes, no) = branch(place)?;
        (local(cond)? == from_right).then_some(())?;
        let [take_right, step_right] = plain(yes)?;
        let [take_left, step_left] = plain(no)?;
        let out = self.placed(take_right, [dest, sorted, right])?;
        let (advanced_right, advance_right) = self.increment(step_right)?;
        let (advanced_left, advance_left) = self.increment(step_left)?;
        let (advanced_dest, advance_dest) = self.increment(advance)?;
        (self.placed(take_left, [dest, sorted, left])? == out
            && advanced_right == right
            && advanced_left == left
            && advanced_dest == dest)
            .then_some(())?;
        let mut roles = vec![
            dest,
            end,
            mid,
            left,
            right,
            width,
            n,
            sorted,
            out,
            from_right,
            first_slot,
            second_slot,
            output,
            order,
        ];
        roles.sort_unstable();
        roles
            .windows(2)
            .all(|pair| pair[0] != pair[1])
            .then_some(())?;
        roles
            .iter()
            .all(|at| !self.shared[*at as usize])
            .then_some(())?;
        let block = |id| self.blocks.iter().position(|block| block.id == id);
        let compared = [block(outer.id)?, block(comparison.id)?];
        Some(Merge {
            start: start_test,
            sums: [sum_mid, sum_end],
            minimum: [min_mid, min_end],
            tests: [left_test, right_test, order_test],
            advances: [advance_right, advance_left, advance_dest],
            counters: [dest, end, mid, left, right, width, n].map(|at| at as usize),
            lists: [sorted as usize, out as usize],
            from_right: from_right as usize,
            items: [first_slot as usize, second_slot as usize],
            order: order as usize,
            body: block(body.id)?,
            compared,
            callback: self.blocks[compared[1]].starts[2],
        })
    }

    fn primitive(&self, value: &Expr, arity: usize) -> Option<LibId> {
        let lib = match &unpin(value).kind {
            Kind::Operator {
                lib, invoke: None, ..
            }
            | Kind::Native { lib, .. } => *lib,
            _ => return None,
        };
        let function = self.exe.lib(lib);
        (function.arity == arity
            && !function.operation()
            && function.limit.is_none()
            && matches!(function.run, LibRun::Native { .. })
            && matches!(
                function.specialization.prepared.as_ref(),
                Some((Entry::Native(_), _))
            ))
        .then_some(lib)
    }

    fn merge_args<'a>(&self, value: &'a Expr, name: &str) -> Option<[&'a Expr; 2]> {
        let value = unpin(value);
        let (lib, args) = match &value.kind {
            Kind::Operator { lib, args, .. } | Kind::Native { lib, args, .. } => (*lib, args),
            _ => return None,
        };
        let function = self.exe.lib(lib);
        if name == "list.get" {
            (function.item_read() == Some(lash_kernel_doc::ValueKind::List)
                && !function.operation()
                && function.limit.is_none())
            .then_some(())?;
        } else {
            self.primitive(value, 2)?;
        }
        let [a, b] = &**args else {
            return None;
        };
        Some([a, b])
    }
    fn merge_locals(&self, value: &Expr, name: &str) -> Option<[u32; 2]> {
        let [a, b] = self.merge_args(value, name)?;
        Some([local(a)?, local(b)?])
    }
    fn bound_sum(&self, statement: &Statement) -> Option<(u32, [u32; 3], LibId, LibId)> {
        let (slot, value) = assigned(statement, false)?;
        let [sum, bound] = self.merge_args(value, "num.min")?;
        let [a, b] = self.merge_locals(sum, "num.add")?;
        Some((
            slot,
            [a, b, local(bound)?],
            self.primitive(sum, 2)?,
            self.primitive(value, 2)?,
        ))
    }
    fn increment(&self, statement: &Statement) -> Option<(u32, LibId)> {
        let (slot, value) = assigned(statement, false)?;
        let [from, one] = self.merge_args(value, "num.add")?;
        (local(from)? == slot && matches!(one.kind, Kind::Const(Value::Int(1))))
            .then_some((slot, self.primitive(value, 2)?))
    }
    fn placed(&self, statement: &Statement, [dest, sorted, at]: [u32; 3]) -> Option<u32> {
        let StatementKind::Store {
            operands,
            target,
            index: Some(index),
            value,
            field: None,
            ..
        } = &statement.kind
        else {
            return None;
        };
        statement.root.is_none().then_some(())?;
        let read = |reg| operands.iter().find(|expr| expr.reg == reg);
        (local(read(*index)?)? == dest
            && self.merge_locals(read(*value)?, "list.get")? == [sorted, at])
        .then_some(())?;
        local(read(*target)?)
    }
}
fn unpin(mut value: &Expr) -> &Expr {
    while let Kind::Pin(inner) = &value.kind {
        value = inner;
    }
    value
}
fn local(value: &Expr) -> Option<u32> {
    match &unpin(value).kind {
        Kind::Local {
            local,
            scoped: false,
            ..
        } => Some(local.at),
        _ => None,
    }
}
fn assigned(statement: &Statement, binding: bool) -> Option<(u32, &Expr)> {
    let StatementKind::Write {
        at, value, bind, ..
    } = &statement.kind
    else {
        return None;
    };
    (*bind == binding).then_some((*at, statement.root.as_ref().unwrap_or(value)))
}
fn branch(statement: &Statement) -> Option<(&Expr, &SourceBlock, &SourceBlock)> {
    statement.root.is_none().then_some(())?;
    match &statement.kind {
        StatementKind::If {
            cond, then_, else_, ..
        } => Some((cond, then_, else_)),
        _ => None,
    }
}
fn plain<const N: usize>(block: &SourceBlock) -> Option<&[Statement; N]> {
    block.declares.is_empty().then_some(())?;
    block.statements.as_ref().try_into().ok()
}
fn empty(block: &SourceBlock) -> Option<()> {
    (block.declares.is_empty() && block.statements.is_empty()).then_some(())
}
fn sets_true(block: &SourceBlock, slot: u32) -> Option<()> {
    let [statement] = plain(block)?;
    let (at, value) = assigned(statement, false)?;
    (at == slot && matches!(value.kind, Kind::Const(Value::Bool(true)))).then_some(())
}
