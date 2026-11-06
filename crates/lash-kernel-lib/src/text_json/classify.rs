//! Allocation-free classification of a JSON graph by the boundary writer's own
//! limits. Admission policy (excluded fields, a lexical key interval and
//! integer admission) belongs to the caller.

use super::Selection;
use std::borrow::Cow;

use lash_kernel_doc::{Args, Formula, NativeCall, NativeError, Operand, Value, arena::ArenaHandle};

use lash_kernel_value::Type;

use super::{Function, arg, definition, raise, text_arg};

mod v1;
pub(super) use v1::native as v1;

/// The deepest container nesting any configured document depth allows; the
/// active path lives in a fixed array of this length.
const DEPTH_CEILING: usize = 64;

pub(super) fn functions(selection: &Selection<'_>) -> Vec<Function> {
    let mut definition = definition(
        selection,
        "json.classify",
        &[
            ("value", Type::Any),
            ("excluded", Type::Tuple(vec![Type::Text, Type::Text])),
            ("key_from", Type::Text),
            ("key_until", Type::Text),
            ("integers", Type::Bool),
        ],
        Type::Int,
        &[],
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
    /// The writer's nesting limit: a container at a greater depth is refused.
    limit: usize,
    /// The containers on the path from the root, so a cycle is refused on
    /// its first return instead of being unrolled to the depth limit.
    active: [Option<ArenaHandle>; DEPTH_CEILING],
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
    /// `depth` counts the containers from the root to `value`, `value`
    /// included, so the root container is at depth 1.
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
        if depth > self.limit {
            return Ok(false);
        }
        let identity = value.handle();
        if identity.is_some() && self.active[..depth - 1].contains(&identity) {
            return Ok(false);
        }
        self.active[depth - 1] = identity;
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

pub(super) fn classify_value(args: Args<'_>) -> Result<Value, NativeError> {
    let args = &args;
    let excluded = arg(args.values(), 1)?;
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
    let Value::Bool(integers) = arg(args.values(), 4)? else {
        return Err(raise("type_error", "expected boolean integer policy"));
    };
    let mut walk = Walk {
        args,
        limit: args.limits().document_depth().min(DEPTH_CEILING),
        active: [None; DEPTH_CEILING],
        excluded: [name(0)?, name(1)?],
        key_from: text_arg(args, 2)?,
        key_until: text_arg(args, 3)?,
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
