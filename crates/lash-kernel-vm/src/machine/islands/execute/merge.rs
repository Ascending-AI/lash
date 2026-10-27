//! The source-certified merge keeps its variables and callback in the
//! common register frame. A cold read, tight boundary or detailed observer
//! runs the original instructions at the same coordinate.
use super::super::compiled::Merge;
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    pub(super) fn merge_step(
        &mut self,
        frame: &mut Frame<'e>,
        merge: &Merge,
        tail: bool,
    ) -> Option<Result<Next, Stop>> {
        if !self.exe.charging()
            || self.accounting
            || self.spent()
            || self.deferred_end.is_some()
            || self.arena.boundary_due()
            || self.arena.any_cold()
            || self.tight
            || (self.memory
                && lash_kernel_state::arena::VALUE_BYTES
                    > self
                        .ceiling
                        .saturating_sub(self.register_bytes)
                        .saturating_sub(self.arena.held())
                        .saturating_sub(self.transient))
            || self.projection().is_some()
        {
            return None;
        }
        #[cfg(any(test, feature = "perf-witness"))]
        if self.scratch.counting {
            return None;
        }
        #[cfg(test)]
        if self.scratch.chilled {
            return None;
        }
        let value = |at| match frame.slots[at] {
            SlotState::Value(value) => Some(value),
            _ => None,
        };
        let mut counters = [0i64; 7];
        for (to, at) in counters.iter_mut().zip(merge.counters) {
            let Value::Int(number) = value(at)? else {
                return None;
            };
            if !(0..(1 << 48)).contains(&number) {
                return None;
            }
            *to = number;
        }
        let [dest, mut end, mut mid, mut left, mut right, width, n] = counters;
        let (Value::List(sorted), Value::List(out)) =
            (value(merge.lists[0])?, value(merge.lists[1])?)
        else {
            return None;
        };
        if sorted == out
            || dest >= n
            || self.arena.len(sorted).ok()? < n as u64
            || dest as u64 > self.arena.len(out).ok()?
        {
            return None;
        }
        let from_right = if tail {
            match value(merge.order)? {
                Value::Int(order) => order > 0,
                Value::Float(order) => order.get() > 0.0,
                _ => return None,
            }
        } else {
            if dest == end {
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
                if self.arena.summary_of(&first).depth as usize > self.limits.value_depth()
                    || self.arena.summary_of(&second).depth as usize > self.limits.value_depth()
                {
                    return None;
                }
                for (at, number) in merge
                    .counters
                    .into_iter()
                    .zip([dest, end, mid, left, right, width, n])
                {
                    frame.slots[at] = SlotState::Value(Value::Int(number));
                }
                frame.slots[merge.from_right] = SlotState::Value(Value::Bool(false));
                frame.slots[merge.items[0]] = SlotState::Value(first);
                frame.slots[merge.items[1]] = SlotState::Value(second);
                frame.control.push(Position::Block(merge.body, 3));
                frame.control.push(Position::Block(merge.compared[0], 1));
                frame.control.push(Position::Block(merge.compared[1], 2));
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
        if self.arena.summary_of(&item).depth as usize > self.limits.value_depth() {
            return None;
        }
        // All reads and capacity checks precede the only fallible write.
        self.arena.list_set(out, dest as u64, item).ok()?;
        if tail {
            Self::pop_block(frame);
            Self::pop_block(frame);
        } else {
            frame.control.push(Position::Block(merge.body, 5));
        }
        if from_right {
            right += 1;
        } else {
            left += 1;
        }
        for (at, number) in
            merge
                .counters
                .into_iter()
                .zip([dest + 1, end, mid, left, right, width, n])
        {
            frame.slots[at] = SlotState::Value(Value::Int(number));
        }
        frame.slots[merge.from_right] = SlotState::Value(Value::Bool(from_right));
        Some(self.finish_statement(frame, 1, None))
    }
}
