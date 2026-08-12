pub(crate) struct IterState {
    cursor: IterCursor,
    binding: usize,
    restore: LoopRestore,
}

enum IterCursor {
    List { values: ListValue, index: usize },
    Range { next: i64, end: i64, step: i64 },
}

impl IterCursor {
    fn next_value(&mut self) -> Option<Value> {
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

struct LoopRestore {
    previous: Option<Value>,
}

pub(super) fn range_has_next(start: i64, end: i64, step: i64) -> bool {
    (step > 0 && start < end) || (step < 0 && start > end)
}
