use super::*;

#[derive(Clone)]
pub(crate) struct IterState {
    pub(super) cursor: IterCursor,
    pub(super) binding: usize,
    pub(super) restore: LoopRestore,
    /// Whether this iterator's captured values have already been imported into
    /// the heap.
    ///
    /// A cursor's values are written exactly once, when the iterator is created
    /// or restored from a continuation; stepping only advances an index. Once
    /// the values have been heapified they can never regress to inline
    /// compounds, so later instructions skip them instead of rescanning the
    /// whole sequence — which is what made iterating a long list quadratic.
    pub(super) heapified: bool,
}

#[derive(Clone)]
pub(super) enum IterCursor {
    List { values: ListValue, index: usize },
    Range { next: i64, end: i64, step: i64 },
}

impl IterCursor {
    pub(super) fn next_value(&mut self) -> Option<Value> {
        match self {
            Self::List { values, index } => {
                let value = values.get(*index)?.clone();
                *index += 1;
                Some(value)
            }
            Self::Range { next, end, step } => {
                if (*step > 0 && *next >= *end) || (*step < 0 && *next <= *end) {
                    return None;
                }
                let value = *next;
                *next = (*next).saturating_add(*step);
                Some(Value::Number(value as f64))
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct LoopRestore {
    pub(super) previous: Option<Value>,
}

pub(super) fn range_has_next(start: i64, end: i64, step: i64) -> bool {
    (step > 0 && start < end) || (step < 0 && start > end)
}

impl<'a, H: ExecutionHost> Vm<'a, H> {
    pub(super) fn deep_copy_loop_binding(&mut self, binding: usize) -> Result<(), RuntimeError> {
        let source = self.load_slot(binding)?.clone();
        self.slots
            .assign_loop_binding(binding, self.heap.isolate_value(&source)?);
        Ok(())
    }
}
