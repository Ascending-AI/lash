//! Allocation-free classification of a depth-bounded JSON graph. Admission policy
//! (excluded fields, a lexical key interval and integer admission) belongs to the caller.

use super::Selection;
use std::borrow::Cow;

use lash_kernel_doc::{Args, Formula, NativeCall, NativeError, Operand, Value};

use lash_vocabulary::Type;

use super::{Function, arg, count_arg, definition, raise, text_arg};
pub(super) fn functions(selection: &Selection<'_>) -> Vec<Function> {
    let mut definition = definition(
        selection,
        "json.classify",
        &[
            ("value", Type::Any),
            ("depth", Type::Int),
            ("excluded", Type::Tuple(vec![Type::Text, Type::Text])),
            ("key_from", Type::Text),
            ("key_until", Type::Text),
            ("integers", Type::Bool),
        ],
        Type::Int,
        &["number_range"],
        classify,
    );
    // Work is carried in the result, rather than charging the deep size of
    // the unvisited input. Its low bit is admission; the other bits count
    // node/field visits and bytes compared. No traversal is needed to price it.
    definition.charge(|_| {
        Formula::Sum(vec![
            Formula::Constant(1),
            Formula::Magnitude(Operand::Result),
        ])
    });
    vec![definition]
}

struct Walk<'a, 'call> {
    args: &'a Args<'call>,
    depth: usize,
    excluded: [Cow<'call, str>; 2],
    key_from: Cow<'call, str>,
    key_until: Cow<'call, str>,
    integers: bool,
    work: u64,
}

impl Walk<'_, '_> {
    fn compare(work: &mut u64, key: &str, policy: &str) -> std::cmp::Ordering {
        *work += 1 + key.len().min(policy.len()) as u64;
        key.as_bytes().cmp(policy.as_bytes())
    }
    fn field(&mut self, key: &Value) -> Result<bool, NativeError> {
        let Some(key) = self.args.text(key)? else {
            return Ok(false);
        };
        let key = key.whole()?;
        for excluded in &self.excluded {
            if Self::compare(&mut self.work, &key, excluded).is_eq() {
                return Ok(false);
            }
        }
        Ok(Self::compare(&mut self.work, &key, &self.key_from).is_lt()
            || !Self::compare(&mut self.work, &key, &self.key_until).is_lt())
    }
    fn node(&mut self, value: &Value, depth: usize) -> Result<bool, NativeError> {
        self.work += 1;
        let record = match value {
            Value::Null | Value::Bool(_) | Value::Text(_) | Value::LongText(_) => return Ok(true),
            Value::Int(_) | Value::BigInt(_) => return Ok(self.integers),
            Value::Float(value) => return Ok(value.get().is_finite()),
            Value::List(_) => false,
            Value::Record(_) => true,
            _ => return Ok(false),
        };
        if depth > self.depth {
            return Ok(false);
        }
        if record {
            let Some(table) = self.args.table(value)? else {
                return Err(NativeError::Malformed("a record names another body"));
            };
            for chunk in table.chunks() {
                for pair in chunk?.as_chunks::<2>().0.iter() {
                    if !(self.field(&pair[0])? && self.node(&pair[1], depth + 1)?) {
                        return Ok(false);
                    }
                }
            }
        } else {
            let Some(sequence) = self.args.sequence(value)? else {
                return Err(NativeError::Malformed("a list names another body"));
            };
            for chunk in sequence.chunks() {
                for item in chunk? {
                    if !self.node(item, depth + 1)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

fn classify(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let args = &call.args;
    let depth = count_arg(args, 1)?;
    if !(1..=64).contains(&depth) {
        return Err(raise("number_range", "classification needs 1..64 levels"));
    }
    let excluded = arg(args.values(), 2)?;
    let names = match args.sequence(excluded)? {
        Some(names) if matches!(excluded, Value::Tuple(_)) && names.len() == 2 => names,
        _ => return Err(raise("type_error", "expected two excluded field names")),
    };
    let name = |index| -> Result<Cow<'_, str>, NativeError> {
        let Some(value) = names.get(index)? else {
            return Err(raise("type_error", "expected two excluded field names"));
        };
        match args.text(value)? {
            Some(text) => text.whole(),
            None => Err(raise("type_error", "expected two excluded field names")),
        }
    };
    let Value::Bool(integers) = arg(args.values(), 5)? else {
        return Err(raise("type_error", "expected boolean integer policy"));
    };
    let mut walk = Walk {
        args,
        depth,
        excluded: [name(0)?, name(1)?],
        key_from: text_arg(args, 3)?,
        key_until: text_arg(args, 4)?,
        integers: *integers,
        work: 0,
    };
    let admitted = walk.node(arg(args.values(), 0)?, 1)?;
    // A bounded stack, borrowed views and a small integer result need no
    // allocation or reservation, even with zero remaining memory.
    let encoded = walk
        .work
        .saturating_mul(2)
        .saturating_add(u64::from(admitted));
    Ok(Value::Int(i64::try_from(encoded).unwrap_or(i64::MAX)))
}
