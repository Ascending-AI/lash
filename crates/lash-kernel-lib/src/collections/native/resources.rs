//! Canonical collection allocation descriptions, evaluated from inputs.
mod traversal;
use super::{Kind, Native, Op, entries, field, integer, length, nth, object, position};
use lash_kernel_doc::{
    Args, LogicalHeldV1 as Cost, NativeError, NativeResourceContract as Contract,
    NativeResourceEntity as Entity, NativeResourceEvent as Event, NativeResourceExit as Exit,
    NativeResourceHolding as Holding, NativeResourceOrigin as Origin, NativeResourceOwner as Owner,
    Value,
};

fn holding(args: Args<'_>, value: &Value) -> Result<Holding, NativeError> {
    Contract::returned(args, value).map(|contract| contract.output)
}

fn overflow() -> NativeError {
    NativeError::Malformed("native logical cost overflow")
}

fn constructed(kind: Entity, members: Vec<Holding>) -> Contract {
    let owner = Owner(0);
    Contract {
        exit: Exit::Returned,
        output: Holding {
            bytes: Cost::REFERENCE,
            origin: Origin::Fresh(owner),
        },
        events: vec![Event::Entity {
            owner,
            kind,
            release: None,
            members,
        }],
    }
}

fn tuple(members: &[Holding]) -> Result<Holding, NativeError> {
    Ok(Holding {
        bytes: members
            .iter()
            .try_fold(Cost::REFERENCE, |bytes, member| {
                bytes.checked_add(member.bytes)
            })
            .ok_or_else(overflow)?,
        origin: Origin::Immutable,
    })
}

pub(super) fn error(args: Args<'_>) -> Result<Contract, NativeError> {
    let answer = (|| {
        let kind = field(nth(args.values(), 0)?)?;
        let message = field(nth(args.values(), 1)?)?;
        let data = args.get(2).copied().unwrap_or(Value::Absent);
        let data_bytes = Cost::native_local(args, &data)?;
        let bytes = args
            .text(kind)?
            .ok_or_else(overflow)?
            .len()
            .checked_add(args.text(message)?.ok_or_else(overflow)?.len())
            .and_then(|bytes| bytes.checked_add(Cost::REFERENCE))
            .and_then(|bytes| bytes.checked_add(data_bytes))
            .ok_or_else(overflow)?;
        Ok(Contract::value(Exit::Returned, bytes, Origin::Immutable))
    })();
    match answer {
        Err(NativeError::Refused { kind, message }) => {
            Contract::refused(kind, &message).ok_or_else(overflow)
        }
        answer => answer,
    }
}

pub(super) fn collection(native: &Native, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    match evaluate(native, args) {
        Err(NativeError::Refused { kind, message }) => Ok(Contract::refused(kind, &message)),
        answer => answer,
    }
}

fn evaluate(native: &Native, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    if let Some(answer) = native.read(&args) {
        return Contract::reading(args, answer).map(Some);
    }
    let xs = nth(args.values(), 0)?;
    object(native.kind, xs)?;
    let length = length(&args, xs)?;
    if matches!(native.op, Op::DeepCopy) {
        return traversal::deep(args, xs).map(Some);
    }
    let result = match native.op {
        Op::Copy if matches!(xs, Value::Tuple(_)) => return Contract::returned(args, xs).map(Some),
        Op::Copy | Op::With | Op::Without if !matches!(xs, Value::List(_) | Value::Tuple(_)) => {
            let mut pairs = entries(&args, xs)?;
            if matches!(native.op, Op::With | Op::Without) {
                let name = *field(nth(args.values(), 1)?)?;
                let mut at = None;
                for (index, (key, _)) in pairs.iter().enumerate() {
                    if args.equal(key, &name)? {
                        at = Some(index);
                        break;
                    }
                }
                if matches!(native.op, Op::Without) {
                    if let Some(at) = at {
                        pairs.remove(at);
                    }
                } else {
                    let value = *nth(args.values(), 2)?;
                    match at {
                        Some(at) => pairs[at].1 = value,
                        None => pairs.push((name, value)),
                    }
                }
            }
            let mut members = Vec::new();
            let mut names = 0_u64;
            for (key, value) in pairs {
                if matches!(native.kind, Kind::Record) {
                    names = names
                        .checked_add(args.text(&key)?.ok_or_else(overflow)?.len())
                        .ok_or_else(overflow)?;
                } else {
                    members.push(holding(args, &key)?);
                }
                members.push(holding(args, &value)?);
            }
            constructed(
                match native.kind {
                    Kind::Map => Entity::Map,
                    Kind::Set => Entity::Set,
                    _ => Entity::Record {
                        field_name_bytes: names,
                    },
                },
                members,
            )
        }
        Op::Copy | Op::Slice | Op::Concat => {
            let mut members = Vec::new();
            let (start, end) = if matches!(native.op, Op::Slice) {
                let start = position(integer(&args, nth(args.values(), 1)?)?, length, true)?;
                let end = match args.get(2) {
                    None | Some(Value::Absent) => length,
                    Some(end) => position(integer(&args, end)?, length, true)?,
                }
                .max(start);
                (start, end)
            } else {
                (0, length)
            };
            let sequence = args.sequence(xs)?.ok_or_else(overflow)?;
            for at in start..end {
                members.push(holding(
                    args,
                    sequence.get(at as u64)?.ok_or_else(overflow)?,
                )?);
            }
            if matches!(native.op, Op::Concat) {
                let other = nth(args.values(), 1)?;
                object(native.kind, other)?;
                for chunk in args.sequence(other)?.ok_or_else(overflow)?.chunks() {
                    for member in chunk? {
                        members.push(holding(args, member)?);
                    }
                }
            }
            if matches!(native.kind, Kind::Tuple) {
                let output = tuple(&members)?;
                Contract::value(Exit::Returned, output.bytes, Origin::Immutable)
            } else {
                constructed(Entity::List, members)
            }
        }
        Op::Keys | Op::Values | Op::Entries => {
            let mut members = Vec::new();
            if matches!(native.kind, Kind::List | Kind::Tuple) {
                let sequence = args.sequence(xs)?.ok_or_else(overflow)?;
                for at in 0..length {
                    let key = holding(args, &Value::Int(at as i64))?;
                    let value = holding(args, sequence.get(at as u64)?.ok_or_else(overflow)?)?;
                    members.push(match native.op {
                        Op::Keys => key,
                        Op::Values => value,
                        _ => tuple(&[key, value])?,
                    });
                }
            } else {
                for (key, value) in entries(&args, xs)? {
                    let value = if matches!(native.kind, Kind::Set) {
                        key
                    } else {
                        value
                    };
                    let key = holding(args, &key)?;
                    let value = holding(args, &value)?;
                    members.push(match native.op {
                        Op::Keys => key,
                        Op::Values => value,
                        _ => tuple(&[key, value])?,
                    });
                }
            }
            constructed(Entity::List, members)
        }
        _ => return Ok(None),
    };
    Ok(Some(result))
}

pub(super) fn holds_none(args: Args<'_>) -> Result<Contract, NativeError> {
    traversal::holds_none(args)
}
pub(super) fn identity(args: Args<'_>) -> Result<Contract, NativeError> {
    traversal::identity(args)
}

pub(super) fn index_range(args: Args<'_>) -> Result<Contract, NativeError> {
    let answer = (|| {
        let xs = nth(args.values(), 0)?;
        let (Value::List(_), Some(sequence)) = (xs, args.sequence(xs)?) else {
            return Err(super::raise("type_error", "expected list"));
        };
        let needle = nth(args.values(), 1)?;
        let (Some(start), Some(end)) = (
            integer(&args, nth(args.values(), 2)?)?.machine(),
            integer(&args, nth(args.values(), 3)?)?.machine(),
        ) else {
            return Err(super::raise(
                "index_out_of_range",
                "range requires nonnegative indices",
            ));
        };
        if start > end || end > sequence.len() as usize {
            return Err(super::raise(
                "index_out_of_range",
                "range is outside the list",
            ));
        }
        let (Value::Bool(reverse), Value::Bool(nan_equal)) =
            (nth(args.values(), 4)?, nth(args.values(), 5)?)
        else {
            return Err(super::raise(
                "type_error",
                "search options require booleans",
            ));
        };
        for offset in 0..end - start {
            let at = if *reverse {
                end - offset - 1
            } else {
                start + offset
            };
            let value = sequence
                .get(at as u64)?
                .ok_or_else(|| super::raise("index_out_of_range", "missing list element"))?;
            if args.equal(value, needle)?
                || (*nan_equal
                    && matches!((value, needle), (Value::Float(a), Value::Float(b)) if a.get().is_nan() && b.get().is_nan()))
            {
                return Ok(super::int(at));
            }
        }
        Ok(Value::Int(-1))
    })();
    Contract::reading(args, answer)
}

pub(super) fn sort(args: Args<'_>, utf16: bool) -> Result<Contract, NativeError> {
    use super::items;
    let mut scratch = 0_u64;
    let answer = (|| {
        let (listed @ Value::List(_), ranks @ Value::List(_), Value::Bool(descending)) = (
            nth(args.values(), 0)?,
            nth(args.values(), 1)?,
            nth(args.values(), 2)?,
        ) else {
            return Err(super::raise(
                "type_error",
                "sort_by_keys takes two lists and a bool",
            ));
        };
        let listed = items(&args, listed)?;
        let ranks = items(&args, ranks)?;
        if listed.len() != ranks.len() {
            return Err(super::raise(
                "type_error",
                "every item needs exactly one key",
            ));
        }
        // Item/key occurrences and two permutations of logical integer
        // positions. Cached-key sorting also owns each cached key/index tuple.
        for value in listed.iter().chain(&ranks) {
            scratch = scratch
                .checked_add(Cost::native_local(args, value)?)
                .ok_or_else(overflow)?;
        }
        let index_payload =
            u64::from(usize::BITS - listed.len().saturating_sub(1).leading_zeros()).div_ceil(8);
        let positions = (listed.len() as u64)
            .checked_mul(Cost::REFERENCE + index_payload)
            .and_then(|n| n.checked_mul(2))
            .ok_or_else(overflow)?;
        scratch = scratch.checked_add(positions).ok_or_else(overflow)?;
        if !utf16 && ranks.iter().all(|rank| matches!(rank, Value::Int(_))) {
            for rank in &ranks {
                scratch = scratch
                    .checked_add(Cost::native_local(args, rank)?)
                    .and_then(|n| n.checked_add(Cost::REFERENCE))
                    .ok_or_else(overflow)?;
            }
        }
        if !utf16
            && !ranks.iter().all(|rank| matches!(rank, Value::Int(_)))
            && !ranks.iter().all(|rank| matches!(rank, Value::Text(_)))
        {
            fn nodes(
                args: Args<'_>,
                value: &Value,
                seen: &mut Vec<lash_kernel_doc::arena::ArenaHandle>,
            ) -> Result<u64, NativeError> {
                let mut pending = vec![*value];
                let mut count = 0_u64;
                while let Some(value) = pending.pop() {
                    count = count.checked_add(1).ok_or_else(overflow)?;
                    if matches!(value, Value::List(_) | Value::Tuple(_)) {
                        let id = value.handle().ok_or_else(overflow)?;
                        if seen.contains(&id) {
                            continue;
                        }
                        seen.push(id);
                        for chunk in args.sequence(&value)?.ok_or_else(overflow)?.chunks() {
                            pending.extend_from_slice(chunk?);
                        }
                    }
                }
                Ok(count)
            }
            let mut count = 0_u64;
            let mut seen = vec![];
            for rank in &ranks {
                count = count
                    .checked_add(nodes(args, rank, &mut seen)?)
                    .ok_or_else(overflow)?;
            }
            // A structural comparison's active path contains no repeated pair
            // of nodes. Each pair needs at most two operand slots, two member
            // cursors, their continuation, and two active-identity slots.
            scratch = scratch
                .checked_add(
                    count
                        .checked_mul(count)
                        .and_then(|n| n.checked_mul(8 * Cost::REFERENCE))
                        .ok_or_else(overflow)?,
                )
                .ok_or_else(overflow)?;
        }
        let mut sorted: Vec<_> = (0..listed.len()).collect();
        let mut out = vec![0; sorted.len()];
        let mut width = 1;
        while width < sorted.len() {
            for start in (0..sorted.len()).step_by(2 * width) {
                let mid = (start + width).min(sorted.len());
                let end = (mid + width).min(sorted.len());
                let (mut left, mut right, mut dest) = (start, mid, start);
                while left < mid && right < end {
                    let (before, after) = (sorted[left], sorted[right]);
                    let (a, b) = if *descending {
                        (before, after)
                    } else {
                        (after, before)
                    };
                    let order = if utf16 {
                        Some(crate::comparison::utf16_compare(
                            &ranks[a], &ranks[b], &args,
                        )?)
                    } else {
                        crate::comparison::compare_reading(&ranks[a], &ranks[b], &args, |a, b| {
                            let numeric =
                                |value: &Value| -> Result<crate::numeric::Numeric, NativeError> {
                                    match value {
                                        Value::Float(f) => Ok(crate::numeric::Numeric::Float(*f)),
                                        _ => Ok(crate::numeric::Numeric::Int(
                                            args.integer(value)?.ok_or_else(overflow)?.into_owned(),
                                        )),
                                    }
                                };
                            crate::numeric::scalar_cmp(&numeric(a)?, &numeric(b)?)
                        })?
                    };
                    if order == Some(std::cmp::Ordering::Less) {
                        out[dest] = after;
                        right += 1;
                    } else {
                        out[dest] = before;
                        left += 1;
                    }
                    dest += 1;
                }
                out[dest..dest + mid - left].copy_from_slice(&sorted[left..mid]);
                dest += mid - left;
                out[dest..dest + end - right].copy_from_slice(&sorted[right..end]);
            }
            std::mem::swap(&mut sorted, &mut out);
            width *= 2;
        }
        Ok(constructed(
            Entity::List,
            sorted
                .into_iter()
                .map(|at| holding(args, &listed[at]))
                .collect::<Result<_, _>>()?,
        ))
    })();
    let mut contract = match answer {
        Ok(contract) => contract,
        Err(NativeError::Refused { kind, message }) => {
            Contract::refused(kind, &message).ok_or_else(overflow)?
        }
        Err(error) => return Err(error),
    };
    crate::resource_contracts::scratch(&mut contract, scratch)?;
    Ok(contract)
}
