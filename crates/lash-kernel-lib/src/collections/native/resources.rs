//! Canonical shallow-collection allocation descriptions, evaluated from inputs.
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
    if matches!(native.op, Op::DeepCopy) {
        return Ok(None);
    }
    let xs = nth(args.values(), 0)?;
    object(native.kind, xs)?;
    let length = length(&args, xs)?;
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
