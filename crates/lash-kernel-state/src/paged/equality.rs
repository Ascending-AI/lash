//! Structural equality advances a bounded cursor through the needed prefix.
use super::directory::{Body, Entry};
use super::heap_impl::{bytes_same, required_value, text_same};
use super::tree::Tree;
use super::{ObjectKind, PagedHeap};
use lash_kernel_doc::heap::*;
use lash_kernel_doc::heap_values::integral_float;
use std::collections::BTreeSet;

enum Step {
    Pair(PageValue, PageValue),
    Sequence(PageValue, PageValue, u64, u64),
    Table(Tree<Entry>, EntityId, Option<Vec<u8>>, bool),
}
impl PagedHeap {
    pub(super) fn equal_values(&self, a: &PageValue, b: &PageValue) -> Result<bool, HeapFault> {
        let mut pending = vec![Step::Pair(a.clone(), b.clone())];
        let mut seen = BTreeSet::new();
        while let Some(step) = pending.pop() {
            let (a, b) = match step {
                Step::Sequence(a, b, index, len) => {
                    if index == len {
                        continue;
                    }
                    let get = |v: &PageValue| match v {
                        PageValue::Tuple(t) => self.tuple_get(t, index),
                        PageValue::List(id) => self.list_get(*id, index),
                        _ => Err(HeapFault::Malformed(
                            "equality sequence kind mismatch".into(),
                        )),
                    };
                    let left = required_value(get(&a)?)?;
                    let right = required_value(get(&b)?)?;
                    pending.push(Step::Sequence(a, b, index + 1, len));
                    pending.push(Step::Pair(left, right));
                    continue;
                }
                Step::Table(rows, right, after, values) => {
                    let Some((key, entry)) = rows.after(after.as_deref(), &self.access)? else {
                        continue;
                    };
                    let Some(value) = self.table_get(right, &entry.key)? else {
                        return Ok(false);
                    };
                    pending.push(Step::Table(rows, right, Some(key), values));
                    if values {
                        pending.push(Step::Pair(entry.value, value));
                    }
                    continue;
                }
                Step::Pair(a, b) => (a, b),
            };
            match (&a, &b) {
                (PageValue::Null, PageValue::Null) | (PageValue::Absent, PageValue::Absent) => (),
                (PageValue::Bool(a), PageValue::Bool(b)) if a == b => (),
                (PageValue::Int(a), PageValue::Int(b)) if self.int_equal(a, b)? => (),
                (PageValue::Float(a), PageValue::Float(b)) if a.get() == b.get() => (),
                (PageValue::Int(i), PageValue::Float(f))
                | (PageValue::Float(f), PageValue::Int(i)) => {
                    let Some(integer) = integral_float(f.get()) else {
                        return Ok(false);
                    };
                    if !self.int_equal(i, &IntRef::Inline(integer))? {
                        return Ok(false);
                    }
                }
                (PageValue::Text(a), PageValue::Text(b)) if text_same(self, a, b)? => (),
                (PageValue::Bytes(a), PageValue::Bytes(b)) if bytes_same(self, a, b)? => (),
                (PageValue::Timestamp(a), PageValue::Timestamp(b)) if a == b => (),
                (PageValue::Function(a), PageValue::Function(b)) if a == b => (),
                (PageValue::Handle(a), PageValue::Handle(b)) if a == b => (),
                (PageValue::Task(a), PageValue::Task(b)) if a == b => (),
                (PageValue::Ref(a), PageValue::Ref(b)) if a == b => (),
                (PageValue::Closure(a), PageValue::Closure(b)) if a == b => (),
                (PageValue::Tuple(a_tuple), PageValue::Tuple(b_tuple)) => {
                    if a_tuple.summary.len != b_tuple.summary.len {
                        return Ok(false);
                    }
                    let len = a_tuple.summary.len;
                    pending.push(Step::Sequence(a, b, 0, len));
                }
                (PageValue::Error(a), PageValue::Error(b)) => {
                    if !text_same(self, &a.kind, &b.kind)?
                        || !text_same(self, &a.message, &b.message)?
                    {
                        return Ok(false);
                    }
                    pending.push(Step::Pair(self.error_data(a)?, self.error_data(b)?));
                }
                (PageValue::List(left), PageValue::List(right)) => {
                    if !seen.insert((0, *left, *right)) {
                        continue;
                    }
                    let len = self.len(*left)?;
                    if len != self.len(*right)? {
                        return Ok(false);
                    }
                    pending.push(Step::Sequence(a, b, 0, len));
                }
                (PageValue::Map(left), PageValue::Map(right))
                | (PageValue::Set(left), PageValue::Set(right))
                | (PageValue::Record(left), PageValue::Record(right)) => {
                    if !seen.insert((1, *left, *right)) {
                        continue;
                    }
                    if self.len(*left)? != self.len(*right)? {
                        return Ok(false);
                    }
                    let Some(head) = self.head(*left)? else {
                        return Err(HeapFault::Malformed("table identity missing".into()));
                    };
                    let Body::Table { rows, kind, .. } = head.body else {
                        return Err(HeapFault::Malformed("table identity kind mismatch".into()));
                    };
                    pending.push(Step::Table(rows, *right, None, kind != ObjectKind::Set));
                }
                _ => return Ok(false),
            }
        }
        Ok(true)
    }

    fn int_equal(&self, a: &IntRef, b: &IntRef) -> Result<bool, HeapFault> {
        let summary = |i: &IntRef| match i {
            IntRef::Inline(i) => IntSummary {
                bits: i.bits(),
                negative: i.is_negative(),
            },
            IntRef::Paged { summary, .. } => *summary,
        };
        if summary(a) != summary(b) {
            return Ok(false);
        }
        if let (IntRef::Paged { digits: a, .. }, IntRef::Paged { digits: b, .. }) = (a, b) {
            return Ok(a.id == b.id);
        }
        Ok(self.copy_int(a)? == self.copy_int(b)?)
    }
}
