//! Recognize a stable merge from its complete source data flow. The fast
//! steps retain the source body and callback coordinates for every exit.
use super::*;

pub(in crate::machine::islands) struct Merge {
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
                    if let Some(merge) = self.merge(body) {
                        let head = self.blocks[merge.body].starts[0] - 1;
                        if !matches!(self.ops[head].op, Op::Open(block) if block == merge.body) {
                            continue;
                        }
                        let tail = self.blocks[merge.compared[1]].starts[4];
                        let merge = std::sync::Arc::new(merge);
                        let original =
                            std::mem::replace(&mut self.ops[head].op, Op::Jump(head + 1));
                        self.ops[head].op = Op::Merge {
                            merge: merge.clone(),
                            tail: false,
                            original: Box::new(original),
                        };
                        let original =
                            std::mem::replace(&mut self.ops[tail].op, Op::Jump(tail + 1));
                        self.ops[tail].op = Op::Merge {
                            merge,
                            tail: true,
                            original: Box::new(original),
                        };
                    }
                    self.merges(body);
                }
                StatementKind::While { body, .. } => self.merges(body),
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

    fn merge(&self, body: &SourceBlock) -> Option<Merge> {
        let [head, start, compare, place, advance] = &*body.statements else {
            return None;
        };
        let (cond, then_, otherwise) = branch(head)?;
        let [dest, end] = self.merge_locals(cond, "same")?;
        empty(otherwise)?;
        let [set_mid, set_end, set_left, set_right] = plain(then_)?;
        let (mid, [dest2, width, n]) = self.bound_sum(set_mid)?;
        let (end2, [mid2, width2, n2]) = self.bound_sum(set_end)?;
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
        (self.merge_locals(cond, "num.lt")? == [left, mid]).then_some(())?;
        sets_true(otherwise, from_right)?;
        let [inner] = plain(outer)?;
        let (cond, comparison, otherwise) = branch(inner)?;
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
        (self.placed(take_left, [dest, sorted, left])? == out
            && self.increment(step_right)? == right
            && self.increment(step_left)? == left
            && self.increment(advance)? == dest)
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

    fn merge_args<'a>(&self, value: &'a Expr, name: &str) -> Option<[&'a Expr; 2]> {
        let value = unpin(value);
        let (lib, args) = match &value.kind {
            Kind::Operator { lib, args, .. } | Kind::Native { lib, args, .. } => (*lib, args),
            _ => return None,
        };
        let function = self.exe.lib(lib);
        (function.header.name.as_str() == name
            && function.arity == 2
            && !function.operation()
            && function.limit.is_none()
            && (name == "list.get" || function.prepared.is_some())
            && matches!(function.run, LibRun::Native { .. }))
        .then_some(())?;
        let [a, b] = &**args else {
            return None;
        };
        Some([a, b])
    }
    fn merge_locals(&self, value: &Expr, name: &str) -> Option<[u32; 2]> {
        let [a, b] = self.merge_args(value, name)?;
        Some([local(a)?, local(b)?])
    }
    fn bound_sum(&self, statement: &Statement) -> Option<(u32, [u32; 3])> {
        let (slot, value) = assigned(statement, false)?;
        let [sum, bound] = self.merge_args(value, "num.min")?;
        let [a, b] = self.merge_locals(sum, "num.add")?;
        Some((slot, [a, b, local(bound)?]))
    }
    fn increment(&self, statement: &Statement) -> Option<u32> {
        let (slot, value) = assigned(statement, false)?;
        let [from, one] = self.merge_args(value, "num.add")?;
        (local(from)? == slot && matches!(one.kind, Kind::Const(Value::Int(1)))).then_some(slot)
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
