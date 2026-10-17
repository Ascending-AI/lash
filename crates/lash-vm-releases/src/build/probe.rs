//! Strict native replay over the arena a run holds, with the call's
//! reservations recorded by its nursery.
use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::heap::{EntityId, FamilyId, LogicalBytes};
use lash_kernel_doc::{
    Args, Integer, Measure, NativeCall, NativeError, Nursery, Operand, PreparedNative,
    RegisteredFunction, Retry, Timestamp, Value, WorkCounter,
};
use lash_kernel_state::arena::Arena;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
#[path = "probe/fixtures.rs"]
pub(crate) mod fixtures;
use fixtures::{RecordedObject, RecordedValue, Replay};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Input {
    pub args: Vec<RecordedValue>,
    pub objects: Vec<RecordedObject>,
    pub work: u64,
    pub room: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Answer {
    pub outcome: String,
    pub spent: u64,
    pub charge: u64,
    pub reservations: Vec<(u64, u64)>,
    pub reserved: u64,
    pub prepared_difference: Option<String>,
}
pub(crate) fn answer(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
) -> Answer {
    let replay =
        Replay::load(input).unwrap_or_else(|error| panic!("invalid native fixture: {error}"));
    answer_loaded(registered, input, prepared, replay)
}
pub(crate) fn answer_loaded(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
    replay: Replay,
) -> Answer {
    answer_replayed(registered, input, prepared, replay, false).0
}
/// What [`answer`] gives when every chunk the arguments reach starts cold,
/// each read of one answering [`Retry::Cold`] and the call run again once
/// it is warmed, as the machine runs it; and how many times it ran again.
#[cfg(test)]
#[allow(
    dead_code,
    reason = "the release crate's cold law calls it; other crates include this file"
)]
pub(crate) fn answer_cold(registered: &RegisteredFunction, input: &Input) -> (Answer, u64) {
    let replay =
        Replay::load(input).unwrap_or_else(|error| panic!("invalid native fixture: {error}"));
    answer_replayed(registered, input, None, replay, true)
}
fn answer_replayed(
    registered: &RegisteredFunction,
    input: &Input,
    prepared: Option<PreparedNative>,
    mut replay: Replay,
    cold: bool,
) -> (Answer, u64) {
    let native = registered
        .native()
        .unwrap_or_else(|| panic!("a recorded entry has no native"));
    let mut chilled = if cold {
        chill(&mut replay.arena, &replay.args)
    } else {
        Vec::new()
    };
    let mut warmed = 0u64;
    let (called, reservations, built, counter) = loop {
        let mut counter = WorkCounter::new(Some(input.work));
        let arena = &replay.arena;
        let semantics = arena.semantics();
        let mut nursery = Nursery::recording(arena.objects(), LogicalBytes(input.room));
        let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            native.call(NativeCall {
                args: Args::new(&replay.args, arena.objects(), &semantics),
                new: &mut nursery,
                counter: &mut counter,
            })
        }));
        if let Ok(Err(NativeError::Retry(Retry::Cold { object, .. }))) = &called
            && let Some(at) = chilled.iter().position(|(handle, _)| handle == object)
        {
            nursery.discard();
            let (handle, body) = chilled.swap_remove(at);
            replay
                .arena
                .thaw(handle, body)
                .unwrap_or_else(|error| panic!("native replay thaw: {error}"));
            warmed += 1;
            continue;
        }
        let reservations = nursery.reservations().to_vec();
        break (called, reservations, nursery.into_new(), counter);
    };
    // What the answer is read by reads it warm.
    for (handle, body) in chilled {
        replay
            .arena
            .thaw(handle, body)
            .unwrap_or_else(|error| panic!("native replay thaw: {error}"));
    }
    let mut next = replay.next;
    let grown = replay
        .arena
        .install(built, &mut || {
            next += 1;
            Ok(EntityId {
                family: FamilyId::PRIMARY,
                ordinal: next,
            })
        })
        .unwrap_or_else(|error| panic!("native replay install: {error}"));
    // The machine admits what a call built once it returns, its holds
    // paying first: a call whose objects outgrow its room ends at the
    // memory bound as a refused allocation would.
    let called = match called {
        Ok(Ok(_) | Err(NativeError::Raised(_))) if grown > input.room => Ok(Err(Retry::Room {
            wanted: LogicalBytes(grown),
        }
        .into())),
        called => called,
    };
    let reserved = reservations.iter().filter(|r| r.held).fold(0u64, |sum, r| {
        sum.saturating_add(r.values.saturating_mul(16))
            .saturating_add(r.bytes)
    });
    let reservations = reservations.iter().map(|r| (r.values, r.bytes)).collect();
    let mut outcome = String::new();
    let result = match called {
        Ok(Ok(value)) => {
            outcome.push_str("ok ");
            write_value(&replay, &value, &mut outcome);
            value
        }
        Ok(Err(NativeError::Raised(error))) => {
            let Ok(Body::Error(parts)) = error
                .handle()
                .ok_or(())
                .and_then(|handle| replay.arena.body(handle).map_err(|_| ()))
            else {
                panic!("a native raised a value that is no error")
            };
            let _ = write!(
                outcome,
                "raised {:?} {:?} ",
                text(&replay.arena, &parts.kind),
                text(&replay.arena, &parts.message)
            );
            write_value(&replay, &parts.data, &mut outcome);
            Value::Absent
        }
        Ok(Err(NativeError::Refused { kind, message })) => {
            let _ = write!(outcome, "raised {kind:?} {message:?} ");
            write_value(&replay, &Value::Null, &mut outcome);
            Value::Absent
        }
        Ok(Err(NativeError::Guard(guard))) => {
            let _ = write!(outcome, "guard {}", guard.limit);
            Value::Absent
        }
        Ok(Err(NativeError::Retry(Retry::Room { .. }))) => {
            outcome.push_str("memory");
            Value::Absent
        }
        Ok(Err(error @ (NativeError::Retry(Retry::Cold { .. }) | NativeError::Malformed(_)))) => {
            panic!("native replay infrastructure fault: {error}")
        }
        Ok(Err(NativeError::Decline)) => {
            outcome.push_str("declined");
            Value::Absent
        }
        Err(_) => {
            outcome.push_str("panicked");
            Value::Absent
        }
    };
    let charge = registered
        .definition()
        .charge
        .evaluate(&mut |operand, measure| {
            let value = match operand {
                Operand::Result => &result,
                Operand::Param(name) => registered
                    .definition()
                    .signature
                    .params
                    .iter()
                    .position(|p| &p.name == name)
                    .and_then(|i| replay.args.get(i))
                    .unwrap_or(&Value::Absent),
            };
            measure_value(&replay.arena, value, measure)
        });
    let prepared_difference = prepared
        .filter(|_| !outcome.starts_with("guard "))
        .and_then(|prepared| {
            let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (prepared.entry)(&replay.args)
            }));
            let mut observed = String::new();
            match called {
                Ok(Some(value)) => {
                    observed.push_str("ok ");
                    write_value(&replay, &value, &mut observed);
                }
                Ok(None) => return None,
                Err(_) => observed.push_str("panicked"),
            };
            (observed != outcome).then_some(observed)
        });
    (
        Answer {
            outcome,
            spent: counter.spent(),
            charge,
            reservations,
            reserved,
            prepared_difference,
        },
        warmed,
    )
}
/// Sets aside every resident body `args` reach.
fn chill(
    arena: &mut Arena,
    args: &[Value],
) -> Vec<(ArenaHandle, lash_kernel_state::arena::Chilled)> {
    let mut pending: Vec<ArenaHandle> = args.iter().filter_map(Value::handle).collect();
    let mut seen: Vec<ArenaHandle> = Vec::new();
    let mut order = Vec::new();
    while let Some(handle) = pending.pop() {
        if seen.contains(&handle) {
            continue;
        }
        seen.push(handle);
        if let Ok(body) = arena.body(handle) {
            lash_kernel_state::arena::each_member(body, &mut |member| {
                pending.extend(member.handle());
            });
        }
        order.push(handle);
    }
    order
        .into_iter()
        .filter_map(|handle| {
            let chilled = arena
                .chill(handle)
                .unwrap_or_else(|error| panic!("native replay chill: {error}"))?;
            Some((handle, chilled))
        })
        .collect()
}
fn text(arena: &Arena, value: &Value) -> String {
    arena
        .semantics()
        .text(value)
        .unwrap_or_else(|error| panic!("native replay text: {error}"))
        .to_owned()
}
fn integer(arena: &Arena, value: &Value) -> Integer {
    arena
        .semantics()
        .integer(value)
        .unwrap_or_else(|error| panic!("native replay integer: {error}"))
        .unwrap_or_else(|| panic!("native replay integer is no integer"))
}
fn bytes(arena: &Arena, handle: ArenaHandle) -> Vec<u8> {
    arena
        .semantics()
        .bytes(handle)
        .unwrap_or_else(|error| panic!("native replay bytes: {error}"))
        .to_vec()
}
fn items(arena: &Arena, handle: ArenaHandle) -> Vec<Value> {
    arena
        .semantics()
        .sequence(handle)
        .unwrap_or_else(|error| panic!("native replay sequence: {error}"))
        .to_vec()
}
fn pairs(arena: &Arena, handle: ArenaHandle) -> Vec<(Value, Value)> {
    arena
        .semantics()
        .entries(handle)
        .unwrap_or_else(|error| panic!("native replay table: {error}"))
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| (pair[0], pair[1]))
        .collect()
}
fn error_parts(arena: &Arena, handle: ArenaHandle) -> (Value, Value, Value) {
    match arena.body(handle) {
        Ok(Body::Error(parts)) => (parts.kind, parts.message, parts.data),
        _ => panic!("native replay error has another body"),
    }
}
fn write_value(replay: &Replay, value: &Value, out: &mut String) {
    out.push_str(&json(replay, value, &mut Vec::new()).to_string());
}
fn json(replay: &Replay, value: &Value, path: &mut Vec<ArenaHandle>) -> serde_json::Value {
    use serde_json::json;
    let arena = &replay.arena;
    match value {
        Value::Null => json!("null"),
        Value::Absent => json!("absent"),
        Value::Bool(v) => json!({"bool":v}),
        Value::Int(_) | Value::BigInt(_) => json!({"int":integer(arena,value)}),
        Value::Float(v) => json!({"float":v}),
        Value::Text(_) | Value::LongText(_) => json!({"text":text(arena,value)}),
        Value::Function(_) | Value::LongFunction(_) => json!({"function":text(arena,value)}),
        Value::Bytes(v) => json!({"bytes":lash_kernel_doc::Bytes::new(bytes(arena, *v))}),
        Value::Timestamp(_) | Value::BigTimestamp(_) => {
            let nanoseconds = arena
                .semantics()
                .nanoseconds(value)
                .unwrap_or_else(|e| panic!("native replay timestamp: {e}"))
                .unwrap_or_else(|| panic!("native replay timestamp is no timestamp"));
            json!({"timestamp":Timestamp{nanoseconds}})
        }
        Value::Handle(v) => match arena.body(*v) {
            Ok(Body::Handle(handle)) => json!({"handle":handle}),
            _ => panic!("native replay handle has another body"),
        },
        Value::Task(v) => json!({"task":v}),
        Value::TaskRef(v) => json!({"ref":{"task":v}}),
        Value::ObjectRef(v) => {
            json!({"ref":{"object":replay.ordinal(*v).unwrap_or_else(||panic!("unrecorded identity reference"))}})
        }
        Value::Tuple(v) => {
            let values: Vec<_> = items(arena, *v)
                .iter()
                .map(|v| json(replay, v, path))
                .collect();
            json!({"tuple":values})
        }
        Value::Error(v) => {
            let (kind, message, data) = error_parts(arena, *v);
            json!({"error":{"kind":text(arena,&kind),"message":text(arena,&message),"data":json(replay,&data,path)}})
        }
        Value::List(id) | Value::Set(id) | Value::Map(id) | Value::Record(id) => {
            if path.contains(id) {
                return json!({"cyclic_object":replay.ordinal(*id).unwrap_or_else(||panic!("unrecorded cyclic identity"))});
            }
            path.push(*id);
            let encoded = match value {
                Value::List(_) => {
                    let items: Vec<_> = items(arena, *id)
                        .iter()
                        .map(|v| json(replay, v, path))
                        .collect();
                    json!({"list":items})
                }
                Value::Set(_) => {
                    let items: Vec<_> = pairs(arena, *id)
                        .iter()
                        .map(|(key, _)| json(replay, key, path))
                        .collect();
                    json!({"set":items})
                }
                Value::Map(_) => {
                    let entries: Vec<_> = pairs(arena, *id)
                        .iter()
                        .map(|(key, value)| [json(replay, key, path), json(replay, value, path)])
                        .collect();
                    json!({"map":entries})
                }
                _ => {
                    let fields: Vec<_> = pairs(arena, *id)
                        .iter()
                        .map(|(name, value)| (text(arena, name), json(replay, value, path)))
                        .collect();
                    json!({"record":fields})
                }
            };
            path.pop();
            encoded
        }
        Value::Closure(_) => panic!("native corpus contains no closure fixtures"),
    }
}
fn size(arena: &Arena, value: &Value) -> u64 {
    let len = |handle: &ArenaHandle| match arena.body(*handle) {
        Ok(Body::List(sequence) | Body::Tuple(sequence)) => sequence.len,
        Ok(Body::Map(table) | Body::Set(table) | Body::Record(table)) => table.len,
        _ => panic!("native measure: a collection has another body"),
    };
    1 + match value {
        Value::Int(_) | Value::BigInt(_) => integer(arena, value).bits().div_ceil(64),
        Value::Text(_) | Value::LongText(_) => text(arena, value).len() as u64,
        Value::Bytes(v) => bytes(arena, *v).len() as u64,
        Value::Tuple(v) => len(v),
        Value::Error(v) => {
            let (kind, message, _) = error_parts(arena, *v);
            (text(arena, &kind).len() + text(arena, &message).len()) as u64
        }
        Value::Map(id) => 2 * len(id),
        Value::List(id) | Value::Set(id) | Value::Record(id) => len(id),
        _ => 0,
    }
}
fn measure_value(arena: &Arena, value: &Value, measure: Measure) -> u64 {
    match measure {
        Measure::Size => size(arena, value),
        Measure::Magnitude => match value {
            Value::Int(_) | Value::BigInt(_) => {
                let v = integer(arena, value);
                if v.is_negative() {
                    0
                } else {
                    v.to_string().parse().unwrap_or(u64::MAX)
                }
            }
            Value::Float(v) if v.get().is_finite() && v.get() >= 0.0 => v.get() as u64,
            _ => 0,
        },
        Measure::DeepSize | Measure::NestedSize => {
            let mut pending = vec![*value];
            let mut seen: Vec<ArenaHandle> = Vec::new();
            let mut total = 0u64;
            let mut root = true;
            while let Some(value) = pending.pop() {
                let nested = measure == Measure::NestedSize;
                if let Value::List(id)
                | Value::Map(id)
                | Value::Set(id)
                | Value::Record(id)
                | Value::Closure(id) = value
                {
                    if nested {
                        if !root {
                            total = total.saturating_add(1)
                        };
                        root = false;
                        continue;
                    }
                    if seen.contains(&id) {
                        continue;
                    }
                    seen.push(id);
                    match value {
                        Value::List(_) => pending.extend(items(arena, id)),
                        Value::Set(_) => {
                            pending.extend(pairs(arena, id).into_iter().map(|(k, _)| k))
                        }
                        Value::Map(_) => {
                            for (key, value) in pairs(arena, id) {
                                pending.push(key);
                                pending.push(value);
                            }
                        }
                        Value::Record(_) => {
                            pending.extend(pairs(arena, id).into_iter().map(|(_, v)| v))
                        }
                        _ => (),
                    }
                }
                if !nested || !root {
                    total = total.saturating_add(size(arena, &value));
                }
                root = false;
                match value {
                    Value::Tuple(v) => pending.extend(items(arena, v)),
                    Value::Error(v) => pending.push(error_parts(arena, v).2),
                    _ => (),
                }
            }
            total
        }
    }
}
