//! Allocation-free classification of a bounded JSON graph. Admission policy
//! (excluded fields and a lexical key interval) belongs to the caller.

use std::ops::ControlFlow;

use lash_kernel_doc::{
    Element, Formula, Integer, NativeCall, NativeError, NativeHeap, Operand, Type, Value,
};

use super::{Function, arg, count_arg, definition, raise, text_arg};

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
    excluded: [&'a str; 2],
    key_from: &'a str,
    key_until: &'a str,
    work: u64,
}

impl Walk<'_> {
    fn compare(&mut self, key: &str, policy: &str) -> std::cmp::Ordering {
        // Includes the worst-case shared prefix, not the size of unrelated
        // text leaves or the suffix after the first differing byte.
        self.work += 1 + key.len().min(policy.len()) as u64;
        key.cmp(policy)
    }

    fn field(&mut self, key: &str) -> bool {
        for excluded in self.excluded {
            if self.compare(key, excluded).is_eq() {
                return false;
            }
        }
        self.compare(key, self.key_from).is_lt() || !self.compare(key, self.key_until).is_lt()
    }

    fn node(&mut self, value: &Value, depth: usize) -> bool {
        self.work += 1;
        let (id, record) = match value {
            Value::Null | Value::Bool(_) | Value::Text(_) | Value::Int(_) => return true,
            Value::Float(value) => return value.get().is_finite(),
            Value::List(id) => (*id, false),
            Value::Record(id) => (*id, true),
            _ => return false,
        };
        if depth > self.depth {
            return false;
        }
        let length = self.heap.len(id);
        if length > self.remaining {
            return false;
        }
        self.remaining -= length;
        let mut admitted = true;
        self.heap.visit(id, &mut |element| {
            admitted = match element {
                Element::Field { name, value } if record => {
                    self.field(name) && self.node(value, depth + 1)
                }
                Element::Item(value) if !record => self.node(value, depth + 1),
                _ => false,
            };
            if admitted {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        });
        admitted
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
    let [Value::Text(first), Value::Text(second)] = excluded.as_ref() else {
        return Err(raise("type_error", "expected two excluded field names"));
    };
    let mut walk = Walk {
        heap: call.heap,
        remaining: visits - 1,
        depth,
        excluded: [first, second],
        key_from: text_arg(call.args, 4)?,
        key_until: text_arg(call.args, 5)?,
        work: 0,
    };
    let admitted = walk.node(arg(call.args, 0)?, 1);
    // A bounded stack, borrowed heap visits and a small integer result need
    // no heap allocation or reservation, even with zero remaining memory.
    let encoded = walk
        .work
        .saturating_mul(2)
        .saturating_add(u64::from(admitted));
    Ok(Value::Int(Integer::from(
        i64::try_from(encoded).unwrap_or(i64::MAX),
    )))
}
