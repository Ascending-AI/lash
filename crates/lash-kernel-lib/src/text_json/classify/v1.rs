//! json.classify native version 1, retained while archived definitions bind
//! it (ADR 0149). Its caller supplies a container depth in 1..64, and a cycle
//! is refused only when it reaches that depth.

use std::borrow::Cow;
use std::sync::Arc;

use lash_kernel_doc::{
    Args, NativeCall, NativeError, NativeFunction, NativeResourceContract, Value,
};

use super::super::{arg, count_arg, raise, text_arg};

pub(in crate::text_json) fn native() -> Arc<dyn NativeFunction> {
    Arc::new(super::super::Native {
        function: classify,
        arity: 6..=6,
        prepared: None,
        text_append: false,
        resources: Some(|args| NativeResourceContract::reading(args, classify_value(args))),
    })
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
    classify_value(call.args)
}

fn classify_value(args: Args<'_>) -> Result<Value, NativeError> {
    let args = &args;
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
