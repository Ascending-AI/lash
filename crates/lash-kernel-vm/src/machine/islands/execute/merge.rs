//! The source-certified merge keeps its variables and callback in the
//! common register frame. A cold read, tight boundary or detailed observer
//! runs the original instructions at the same coordinate.
use super::super::compiled::{
    CollectionAppend, CollectionStep, Merge, Placement, ReadSlot, Reduction, Visit,
};
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(super) fn collection_step(
        &mut self,
        frame: &mut Frame<'e>,
        step: &CollectionStep,
        tail: bool,
    ) -> Option<Result<Next, Stop>> {
        if !self.collection_ready() {
            return None;
        }
        match step {
            CollectionStep::Merge(merge) => self.merge_step(frame, merge, tail),
            CollectionStep::Place(place) => self.place_step(frame, place),
            CollectionStep::Visit(visit) => self.visit_step(frame, visit),
            CollectionStep::ReadSlot(read) => self.read_slot_step(frame, read),
            CollectionStep::Reduce(reduction) => self.reduce_step(frame, reduction),
            CollectionStep::Append(append) => self.append_step(frame, append),
        }
    }

    fn append_step(
        &mut self,
        frame: &mut Frame<'e>,
        append: &CollectionAppend,
    ) -> Option<Result<Next, Stop>> {
        let (SlotState::Value(target @ Value::List(list)), SlotState::Value(value)) =
            (frame.slots[append.target], frame.slots[append.item])
        else {
            return None;
        };
        let length = i64::try_from(self.arena.len(list).ok()?).ok()?;
        self.resident_room(lash_kernel_state::arena::VALUE_BYTES)
            .then_some(())?;
        self.store(None, None, target, Value::Int(length), value)?;
        self.end_statement(frame);
        frame.control.push(Position::Block(append.body, 1));
        frame.statement = None;
        frame.pc = frame.callee.compiled.blocks[append.body].end;
        if self.arena.boundary_due() {
            Some(Err(Stop))
        } else {
            Some(Ok(Next::Continue))
        }
    }

    fn reduce_step(
        &mut self,
        frame: &mut Frame<'e>,
        reduction: &Reduction,
    ) -> Option<Result<Next, Stop>> {
        if !self.depth_fits(reduction.depth) {
            return None;
        }
        let number = |at| match frame.slots[at] {
            SlotState::Value(value @ (Value::Int(_) | Value::Float(_))) => Some(value),
            _ => None,
        };
        let (accumulator, item) = (number(reduction.accumulator)?, number(reduction.item)?);
        let values = if reduction.reverse {
            [item, accumulator]
        } else {
            [accumulator, item]
        };
        let signature = reduction
            .checked
            .map(|lib| &self.exe.lib(lib).header.signature);
        if signature.is_some_and(|signature| {
            !signature
                .params
                .iter()
                .zip(values)
                .all(|(param, value)| crate::compile::import_result_fits(&param.ty, value))
        }) {
            return None;
        }
        let paired = reduction.pair.and_then(|pair| match values {
            [Value::Int(left), Value::Int(right)] => {
                lash_kernel_doc::scalar::ints(pair.ints?, left, right)
            }
            [Value::Float(left), Value::Float(right)] => {
                lash_kernel_doc::scalar::floats(pair.floats?, left, right)
            }
            _ => None,
        });
        let answer = paired.or_else(|| reduction.entry.call(&values))?;
        if !matches!(answer, Value::Int(_) | Value::Float(_)) {
            return None;
        }
        if signature
            .is_some_and(|signature| !crate::compile::import_result_fits(&signature.result, answer))
        {
            return None;
        }
        self.end_statement(frame);
        frame.slots[reduction.accumulator] = SlotState::Value(answer);
        frame.control.push(Position::Block(reduction.body, 1));
        frame.statement = None;
        frame.pc = frame.callee.compiled.blocks[reduction.body].end;
        Some(Ok(Next::Continue))
    }

    fn visit_step(&mut self, frame: &mut Frame<'e>, visit: &Visit) -> Option<Result<Next, Stop>> {
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        let (index, length, item) = (
            value(visit.index)?,
            value(visit.length)?,
            value(visit.value)?,
        );
        if !matches!(index, Value::Int(_) | Value::Float(_))
            || !matches!(length, Value::Int(_) | Value::Float(_))
            || index.same(&length)
        {
            return None;
        }
        // Keep the source hole test's empty operand and its held bytes.
        // Everything after it is infallible until the next source statement.
        let hole = match item {
            Value::Tuple(tuple) => self.arena.len(tuple).ok()? == 0,
            _ => false,
        };
        if self.memory || self.projection().is_some() || matches!(item, Value::Tuple(_)) {
            self.empty_tuple()?;
        }
        self.end_statement(frame);
        frame.control.push(Position::Block(visit.body, 2));
        let arm = usize::from(!hole);
        if let Some(block) = visit.arms[arm] {
            frame.control.push(Position::Block(block, 0));
        }
        frame.statement = None;
        frame.pc = visit.after[arm];
        if self.arena.boundary_due() {
            Some(Err(Stop))
        } else {
            Some(Ok(Next::Continue))
        }
    }

    fn read_slot_step(
        &mut self,
        frame: &mut Frame<'e>,
        read: &ReadSlot,
    ) -> Option<Result<Next, Stop>> {
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        let Value::Bool(true) = value(read.list)? else {
            return None;
        };
        let Value::List(list) = value(read.target)? else {
            return None;
        };
        let at = crate::compile::position(&value(read.index)?)?;
        let item = self.arena.item(list, at).ok()??;
        if self.arena.summary_of(&item).depth as usize > self.limits.value_depth() {
            return None;
        }
        let hole = match item {
            Value::Tuple(tuple) => self.arena.len(tuple).ok()? == 0,
            _ => false,
        };
        if self.memory || self.projection().is_some() || matches!(item, Value::Tuple(_)) {
            self.empty_tuple()?;
        }
        self.end_statement(frame);
        frame.slots[read.value] = SlotState::Value(item);
        frame.slots[read.present] = SlotState::Value(Value::Bool(!hole));
        frame.control.push(Position::Block(read.body, 3));
        frame.statement = None;
        frame.pc = frame.callee.compiled.blocks[read.body].starts[3];
        if self.arena.boundary_due() {
            Some(Err(Stop))
        } else {
            Some(Ok(Next::Continue))
        }
    }

    fn collection_ready(&mut self) -> bool {
        let ready = self.exe.charging()
            && !self.accounting
            && !self.spent()
            && self.deferred_end.is_none()
            && !self.arena.boundary_due()
            && !self.arena.any_cold()
            && !self.tight
            && (!self.memory
                || lash_kernel_state::arena::VALUE_BYTES
                    <= self
                        .ceiling
                        .saturating_sub(self.register_bytes)
                        .saturating_sub(self.arena.held())
                        .saturating_sub(self.transient))
            && self.projection().is_none();
        #[cfg(any(test, feature = "perf-witness"))]
        if self.scratch.counting {
            return false;
        }
        #[cfg(test)]
        if self.scratch.chilled {
            return false;
        }
        ready
    }

    fn place_step(
        &mut self,
        frame: &mut Frame<'e>,
        place: &Placement,
    ) -> Option<Result<Next, Stop>> {
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        if let Some(list) = place.list
            && !matches!(value(list)?, Value::Bool(true))
        {
            return None;
        }
        let Value::List(target) = value(place.target)? else {
            return None;
        };
        let index = value(place.index)?;
        let at = crate::compile::position(&index)?;
        if at >= self.arena.len(target).ok()? {
            return None;
        }
        let item = value(place.value)?;
        if self.arena.summary_of(&item).depth as usize > self.limits.value_depth() {
            return None;
        }
        let next = place.advance.call(&[index, place.increment])?;
        self.arena.list_set(target, at, item).ok()?;
        frame.slots[place.index] = SlotState::Value(next);
        frame
            .control
            .push(Position::Block(place.body, place.statements));
        Some(self.finish_statement(frame, 1, None))
    }

    pub(super) fn merge_step(
        &mut self,
        frame: &mut Frame<'e>,
        merge: &Merge,
        tail: bool,
    ) -> Option<Result<Next, Stop>> {
        if tail {
            return self.merge_tail(frame, merge);
        }
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        let number = |at| {
            let Value::Int(number) = value(at)? else {
                return None;
            };
            Some(number)
        };
        let dest = number(merge.counters[0])?;
        let mut end = number(merge.counters[1])?;
        let mut mid = number(merge.counters[2])?;
        let mut left = number(merge.counters[3])?;
        let mut right = number(merge.counters[4])?;
        let n = number(merge.counters[6])?;
        // A union has an upper or sign bit exactly when one counter has it.
        if !(0..(1 << 48)).contains(&(dest | end | mid | left | right | n)) {
            return None;
        }
        let Value::List(sorted) = value(merge.lists[0])? else {
            return None;
        };
        if dest >= n {
            return None;
        }
        let starts_run = dest == end;
        let from_right = {
            if starts_run {
                let width = number(merge.counters[5])?;
                if !(0..(1 << 48)).contains(&width) {
                    return None;
                }
                mid = (dest + width).min(n);
                end = (mid + width).min(n);
                left = dest;
                right = mid;
            }
            if left > mid || mid > end || right > end || end > n {
                return None;
            }
            if left < mid && right < end {
                let first = self.arena.item(sorted, left as u64).ok()??;
                let second = self.arena.item(sorted, right as u64).ok()??;
                if !self.merge_value_fits(&first) || !self.merge_value_fits(&second) {
                    return None;
                }
                if starts_run {
                    for (at, number) in merge.counters[1..5].iter().zip([end, mid, left, right]) {
                        frame.slots[*at] = SlotState::Value(Value::Int(number));
                    }
                }
                frame.slots[merge.from_right] = SlotState::Value(Value::Bool(false));
                frame.slots[merge.items[0]] = SlotState::Value(first);
                frame.slots[merge.items[1]] = SlotState::Value(second);
                frame.control.extend([
                    Position::Block(merge.body, 3),
                    Position::Block(merge.compared[0], 1),
                    Position::Block(merge.compared[1], 2),
                ]);
                frame.statement = None;
                frame.pc = merge.callback;
                return Some(Ok(Next::Continue));
            }
            left >= mid
        };
        let index = if from_right { right } else { left };
        if index < 0 || index >= n {
            return None;
        }
        let item = self.arena.item(sorted, index as u64).ok()??;
        if !self.merge_value_fits(&item) {
            return None;
        }
        let Value::List(out) = value(merge.lists[1])? else {
            return None;
        };
        if sorted == out {
            return None;
        }
        // All reads and capacity checks precede the only fallible write.
        self.arena.list_set(out, dest as u64, item).ok()?;
        frame.control.push(Position::Block(merge.body, 5));
        if from_right {
            right += 1;
        } else {
            left += 1;
        }
        for (at, number) in merge.counters[..5]
            .iter()
            .zip([dest + 1, end, mid, left, right])
        {
            frame.slots[*at] = SlotState::Value(Value::Int(number));
        }
        frame.slots[merge.from_right] = SlotState::Value(Value::Bool(from_right));
        Some(self.finish_statement(frame, 1, None))
    }

    fn merge_tail(&mut self, frame: &mut Frame<'e>, merge: &Merge) -> Option<Result<Next, Stop>> {
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        let from_right = match value(merge.order)? {
            Value::Int(order) => order > 0,
            Value::Float(order) => order.get() > 0.0,
            _ => return None,
        };
        let selected = merge.counters[if from_right { 4 } else { 3 }];
        let (Value::Int(dest), Value::Int(index)) = (value(merge.counters[0])?, value(selected)?)
        else {
            return None;
        };
        if !(0..(1 << 48)).contains(&(dest | index)) {
            return None;
        }
        let (Value::List(sorted), Value::List(out)) =
            (value(merge.lists[0])?, value(merge.lists[1])?)
        else {
            return None;
        };
        if sorted == out {
            return None;
        }
        // The item read and list write check their own bounds. The other
        // merge counters are unchanged by this source suffix.
        let item = self.arena.item(sorted, index as u64).ok()??;
        if !self.merge_value_fits(&item) {
            return None;
        }
        self.arena.list_set(out, dest as u64, item).ok()?;
        Self::pop_block(frame);
        Self::pop_block(frame);
        frame.slots[merge.counters[0]] = SlotState::Value(Value::Int(dest + 1));
        frame.slots[selected] = SlotState::Value(Value::Int(index + 1));
        frame.slots[merge.from_right] = SlotState::Value(Value::Bool(from_right));
        Some(self.finish_statement(frame, 1, None))
    }

    #[inline]
    fn merge_value_fits(&self, value: &Value) -> bool {
        // Only tuples and errors carry immutable nesting depth (K-VAL-034).
        !matches!(value, Value::Tuple(_) | Value::Error(_))
            || self.arena.summary_of(value).depth as usize <= self.limits.value_depth()
    }
}
