//! Allocation-free classification of a bounded JSON graph. Admission policy
//! (excluded fields and a lexical key interval) belongs to the caller.

use std::ops::ControlFlow;

use lash_kernel_doc::{
    Element, Formula, Integer, NativeCall, NativeError, NativeHeap, Operand, Type, Value,
};

use super::{Function, arg, count_arg, definition, raise};
use crate::comparison::payload_order;
use lash_kernel_doc::heap::{HeapFault, IntRef, TextRef};

pub(super) fn functions() -> Vec<Function> {
    let (mut definition, native) = definition(
        "json.classify",
        &[
            ("value", Type::Any),
            ("visits", Type::Int),
            ("depth", Type::Int),
            ("excluded", Type::Tuple(vec![Type::Text, Type::Text])),
            ("key_from", Type::Text),
            ("key_until", Type::Text),
        ],
        Type::Int,
        &["number_range"],
        classify,
    );
    // Work is carried in the result, rather than charging the deep size of
    // the unvisited input. Its low bit is admission; the other bits count
    // node/field visits and bytes compared. No traversal is needed to price it.
    definition.charge = Formula::Sum(vec![
        Formula::Constant(1),
        Formula::Magnitude(Operand::Result),
    ]);
    vec![(definition, native)]
}

struct Walk<'a> {
    heap: &'a dyn NativeHeap,
    remaining: usize,
    depth: usize,
    excluded: [TextRef; 2],
    key_from: TextRef,
    key_until: TextRef,
    work: u64,
}

impl Walk<'_> {
    fn compare(
        &mut self,
        key: &TextRef,
        policy: &TextRef,
    ) -> Result<std::cmp::Ordering, HeapFault> {
        self.work += 1 + key.summary.utf8_bytes.min(policy.summary.utf8_bytes);
        payload_order(
            &Value::Text(key.clone()),
            &Value::Text(policy.clone()),
            self.heap,
        )
    }
    fn field(&mut self, key: &TextRef) -> Result<bool, HeapFault> {
        for excluded in self.excluded.clone() {
            if self.compare(key, &excluded)?.is_eq() {
                return Ok(false);
            }
        }
        Ok(self.compare(key, &self.key_from.clone())?.is_lt()
            || !self.compare(key, &self.key_until.clone())?.is_lt())
    }
    fn node(&mut self, value: &Value, depth: usize) -> Result<bool, HeapFault> {
        self.work += 1;
        let (id, record) = match value {
            Value::Null | Value::Bool(_) | Value::Text(_) | Value::Int(_) => return Ok(true),
            Value::Float(value) => return Ok(value.get().is_finite()),
            Value::List(id) => (*id, false),
            Value::Record(id) => (*id, true),
            _ => return Ok(false),
        };
        if depth > self.depth {
            return Ok(false);
        }
        let length = self.heap.len(id)?;
        if length > self.remaining as u64 {
            return Ok(false);
        }
        self.remaining -= length as usize;
        let mut admitted = true;
        let mut fault = None;
        self.heap.visit(id, &mut |element| {
            let result = match element {
                Element::Field { name, value } if record => self.field(name).and_then(|allowed| {
                    if allowed {
                        self.node(value, depth + 1)
                    } else {
                        Ok(false)
                    }
                }),
                Element::Item(value) if !record => self.node(value, depth + 1),
                _ => Ok(false),
            };
            match result {
                Ok(ok) => admitted = ok,
                Err(error) => {
                    fault = Some(error);
                    admitted = false;
                }
            }
            if admitted {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        })?;
        fault.map_or(Ok(admitted), Err)
    }
}

fn classify(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let visits = count_arg(call.args, 1)?;
    let depth = count_arg(call.args, 2)?;
    if !(1..=128).contains(&visits) || !(1..=64).contains(&depth) {
        return Err(raise(
            "number_range",
            "classification needs 1..128 visits and 1..64 levels",
        ));
    }
    let Value::Tuple(excluded) = arg(call.args, 3)? else {
        return Err(raise("type_error", "expected two excluded field names"));
    };
    if excluded.summary.len != 2 {
        return Err(raise("type_error", "expected two excluded field names"));
    }
    let Some(Value::Text(first)) = call.heap.tuple_get(excluded, 0)? else {
        return Err(raise("type_error", "expected two excluded field names"));
    };
    let Some(Value::Text(second)) = call.heap.tuple_get(excluded, 1)? else {
        return Err(raise("type_error", "expected two excluded field names"));
    };
    let text = |index| match arg(call.args, index)? {
        Value::Text(t) => Ok(t.clone()),
        _ => Err(raise("type_error", "expected text")),
    };
    let mut walk = Walk {
        heap: call.heap,
        remaining: visits - 1,
        depth,
        excluded: [first, second],
        key_from: text(4)?,
        key_until: text(5)?,
        work: 0,
    };
    let admitted = walk.node(arg(call.args, 0)?, 1)?;
    // A bounded stack, borrowed heap visits and a small integer result need
    // no heap allocation or reservation, even with zero remaining memory.
    let encoded = walk
        .work
        .saturating_mul(2)
        .saturating_add(u64::from(admitted));
    Ok(Value::Int(IntRef::Inline(Integer::from(
        i64::try_from(encoded).unwrap_or(i64::MAX),
    ))))
}
