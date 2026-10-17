//! Signature-directed corpus generation (FIG-6182), used only to record inputs.
//!
//! At most 1024 cases per native: all imported sampler inputs (at most 64),
//! each parameter's edges independently, zipped edges, 64 seeded random calls,
//! and zero-work/zero-room calls. Recorded inputs are replayed verbatim; changing
//! this generator does not replace any release's expected answers.

use super::probe::{
    Input,
    fixtures::{RecordedError as ErrorValue, RecordedObject as Object, RecordedValue as Value},
};
use lash_kernel_doc::{Bytes, Float, FunctionDefinition, Handle, Integer, TaskId, Timestamp, Type};

use super::corpus::MAX_CASES;
const WORK: u64 = 100_000;
const ROOM: u64 = 1 << 20;
const RANDOM_CALLS: usize = 64;
const EDGES: usize = 64;

#[cfg_attr(
    not(test),
    expect(
        clippy::expect_used,
        reason = "registered signatures and generated acyclic values have valid identities and JSON"
    )
)]
pub(crate) fn generate(definition: &FunctionDefinition, seeds: &[Input]) -> Vec<Input> {
    let mut cases = seeds.to_vec();
    let mut heap = RecordedObjects::default();
    let choices: Vec<_> = definition
        .signature
        .params
        .iter()
        .map(|param| {
            let mut values = edges(&param.ty, &mut heap, 16);
            if param.optional {
                values.push(Value::Absent);
            }
            values
        })
        .collect();
    let base: Vec<_> = choices.iter().map(|values| values[0].clone()).collect();
    let mut append = |args: Vec<Value>, work, room| {
        cases.push(Input {
            args,
            objects: heap.objects.clone(),
            work,
            room,
        })
    };
    append(base.clone(), WORK, ROOM);
    // Every argument position gets every edge; no prefix of a Cartesian
    // product can starve a later parameter as the old sampler did.
    for (position, values) in choices.iter().enumerate() {
        for value in values {
            let mut args = base.clone();
            args[position] = value.clone();
            append(args, WORK, ROOM);
        }
    }
    for index in 0..choices.iter().map(Vec::len).max().unwrap_or(1) {
        let args = choices
            .iter()
            .map(|values| values[index % values.len()].clone())
            .collect::<Vec<_>>();
        append(args.clone(), WORK, ROOM);
        append(args.clone(), 0, ROOM);
        append(args, WORK, 0);
    }
    let identity = definition.identity();
    let mut seed = [0_u8; 8];
    seed.copy_from_slice(&identity.as_bytes()[..8]);
    let mut random = Random(u64::from_le_bytes(seed) | 1);
    for _ in 0..RANDOM_CALLS {
        let mut heap = RecordedObjects::default();
        let args = definition
            .signature
            .params
            .iter()
            .map(|param| {
                if param.optional && random.next().is_multiple_of(8) {
                    Value::Absent
                } else {
                    random_value(&param.ty, &mut heap, &mut random, 4)
                }
            })
            .collect();
        cases.push(Input {
            args,
            objects: heap.objects,
            work: WORK,
            room: ROOM,
        });
    }
    // Deep maps/records and every Any value kind are standalone inputs,
    // so they cannot be truncated by the per-type choice bound.
    for (position, param) in definition.signature.params.iter().enumerate() {
        for family in 0..3 {
            let mut deep_heap = RecordedObjects {
                objects: heap.objects.clone(),
            };
            if let Some(value) = deep_value(&param.ty, &mut deep_heap, 65, family) {
                let mut args = base.clone();
                args[position] = value;
                cases.push(Input {
                    args,
                    objects: deep_heap.objects,
                    work: WORK,
                    room: ROOM,
                });
            }
        }
        if param.ty == Type::Any {
            let extra = [
                Value::text("🦀".repeat(4096)),
                Value::Bytes(Bytes::new(Vec::new())),
                Value::Timestamp(Timestamp {
                    nanoseconds: Integer::from(i64::MAX),
                }),
                Value::Tuple(Vec::new()),
                Value::Error(std::sync::Arc::new(ErrorValue::new("type_error", "corpus"))),
                Value::Task(TaskId(0)),
                Value::Function(lash_kernel_doc::Name::new("corpus")),
                Value::Handle(std::sync::Arc::new(Handle {
                    kind: "corpus".into(),
                    id: "🦀".into(),
                })),
            ];
            for value in extra {
                let mut args = base.clone();
                args[position] = value;
                cases.push(Input {
                    args,
                    objects: heap.objects.clone(),
                    work: WORK,
                    room: ROOM,
                });
            }
        }
    }
    // Key lookups must exercise aliases, not only a key compared with the
    // same spelling. Use a value distinct from an Any fallback for map.get.
    let aliases = [
        (Value::Int(Integer::from(0)), Value::Float(Float::new(-0.0))),
        (Value::Float(Float::new(-0.0)), Value::Int(Integer::from(0))),
        (Value::Int(Integer::from(1)), Value::Float(Float::new(1.0))),
        (
            Value::Float(Float::new(f64::NAN)),
            Value::Float(Float::new(f64::NAN)),
        ),
        (
            Value::Int(Integer::new(1_i128 << 70)),
            Value::Float(Float::new((1_u128 << 70) as f64)),
        ),
    ];
    for (collection, param) in definition.signature.params.iter().enumerate() {
        let key_type = match &param.ty {
            Type::Map(map) => &map.key,
            Type::Set(item) => item.as_ref(),
            _ => continue,
        };
        for (query, other) in definition.signature.params.iter().enumerate() {
            if collection == query {
                continue;
            }
            for (stored, queried) in &aliases {
                if !key_fits(key_type, stored) || !key_fits(&other.ty, queried) {
                    continue;
                }
                let mut alias_heap = RecordedObjects {
                    objects: heap.objects.clone(),
                };
                let value = match &param.ty {
                    Type::Map(map) => {
                        let member = if map.value == Type::Any {
                            Value::Bool(true)
                        } else {
                            edges(&map.value, &mut alias_heap, 0)
                                .into_iter()
                                .next()
                                .unwrap_or(Value::Null)
                        };
                        Value::Map(alias_heap.object(Object::Map(vec![(stored.clone(), member)])))
                    }
                    _ => Value::Set(alias_heap.object(Object::Set(vec![stored.clone()]))),
                };
                let mut args = base.clone();
                args[collection] = value;
                args[query] = queried.clone();
                cases.push(Input {
                    args,
                    objects: alias_heap.objects,
                    work: WORK,
                    room: ROOM,
                });
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    cases.retain(|input| seen.insert(serde_json::to_vec(input).expect("input JSON")));
    assert!(
        cases.len() <= MAX_CASES,
        "{}: {} corpus cases exceed {MAX_CASES}; split the edge policy",
        definition.name,
        cases.len()
    );
    // Fixture rows are not a second heap grammar: every reachable collection
    // must be constructible by the same heap that replays it. In particular,
    // an Any edge is not necessarily a legal map key or set member.
    for input in &cases {
        super::probe::fixtures::Replay::load(input).unwrap_or_else(|error| {
            panic!("{}: invalid generated fixture: {error}", definition.name)
        });
    }
    cases
}

#[cfg_attr(
    not(test),
    expect(
        clippy::expect_used,
        reason = "registered signatures and generated acyclic values have valid identities and JSON"
    )
)]
fn edges(ty_outer: &Type, heap: &mut RecordedObjects, depth: usize) -> Vec<Value> {
    let int = |value: i64| Value::Int(Integer::from(value));
    let float = |value: f64| Value::Float(Float::new(value));
    let mut values = match ty_outer {
        Type::Null => vec![Value::Null],
        Type::Absent => vec![Value::Absent],
        Type::Bool => vec![Value::Bool(false), Value::Bool(true)],
        Type::Int => vec![
            int(0),
            int(1),
            int(-1),
            int(2),
            int(-7),
            int(31),
            int(32),
            int(63),
            int(64),
            int(i64::MIN),
            int(i64::MAX),
            Value::Int(Integer::new(i128::from(i64::MAX) + 1)),
            Value::Int(Integer::new(i128::from(i64::MIN) - 1)),
            Value::Int(Integer::new(1_i128 << 70)),
            Value::Int(Integer::parse(&"9".repeat(1000)).expect("big integer")),
            Value::Int(Integer::parse(&format!("-{}", "9".repeat(1000))).expect("big integer")),
        ],
        Type::Float => vec![
            float(0.0),
            float(-0.0),
            float(f64::NAN),
            float(f64::INFINITY),
            float(f64::NEG_INFINITY),
            float(1.0),
            float(-1.0),
            float(-1.5),
            float(3.25),
            float(f64::MIN_POSITIVE),
            float(f64::from_bits(1)),
            float(f64::MAX),
            float(-f64::MAX),
            float(1e300),
            float(9007199254740992.0),
        ],
        Type::Number => {
            let mut values = edges(&Type::Int, heap, depth);
            values.extend(edges(&Type::Float, heap, depth));
            values
        }
        Type::Text => [
            "",
            "a",
            "ab",
            "Hello, Wörld",
            "12.5",
            "0",
            "-0",
            "NaN",
            "Infinity",
            "é",
            "e\u{301}",
            "🦀𝄞",
            "\0\n\r\t",
            "[]",
            "{}",
            "null",
            "true",
            "[",
            "(",
            "a+",
            "g",
            "https://example.org/?x=🦀",
            "2024-02-29",
            "not a date",
        ]
        .into_iter()
        .map(Value::text)
        .chain([Value::text("🦀".repeat(4096))])
        .collect(),
        Type::Bytes => vec![
            Value::Bytes(Bytes::new(Vec::new())),
            Value::Bytes(Bytes::new(vec![0, 0x61, 0xff])),
            Value::Bytes(Bytes::new(vec![0xff; 16384])),
        ],
        Type::Timestamp => [i64::MIN, -1, 0, 1, i64::MAX]
            .into_iter()
            .map(|value| {
                Value::Timestamp(Timestamp {
                    nanoseconds: Integer::from(value),
                })
            })
            .collect(),
        Type::Enum(texts) => texts
            .iter()
            .map(|text| Value::text(text.as_str()))
            .collect(),
        Type::Error => vec![Value::Error(std::sync::Arc::new(ErrorValue::new(
            "type_error",
            "corpus 🦀",
        )))],
        Type::Function(_) => vec![Value::Function(lash_kernel_doc::Name::new("corpus"))],
        Type::Task(_) => vec![Value::Task(TaskId(0)), Value::Task(TaskId(u64::MAX))],
        Type::Handle(kind) => vec![Value::Handle(std::sync::Arc::new(Handle {
            kind: kind.clone(),
            id: "corpus 🦀".into(),
        }))],
        Type::Any => {
            let mut values = vec![
                Value::Null,
                Value::Absent,
                Value::Bool(false),
                Value::Bool(true),
            ];
            values.extend(edges(&Type::Number, heap, depth));
            values.extend(edges(&Type::Text, heap, depth).into_iter().take(12));
            values.push(Value::List(heap.object(Object::List(vec![]))));
            values.push(Value::Map(heap.object(Object::Map(vec![]))));
            values.push(Value::Set(heap.object(Object::Set(vec![]))));
            values.push(Value::Record(heap.object(Object::Record(vec![]))));
            if depth > 0 {
                for layers in [16, 65] {
                    let mut value = int(1);
                    for _ in 0..layers {
                        value = Value::List(heap.object(Object::List(vec![value])));
                    }
                    values.push(value);
                }
            }
            values
        }
        Type::Union(types) => types.iter().flat_map(|ty| edges(ty, heap, depth)).collect(),
        Type::Tuple(types) => {
            let choices: Vec<_> = types
                .iter()
                .map(|ty| edges(ty, heap, depth.saturating_sub(1)))
                .collect();
            (0..choices.iter().map(Vec::len).max().unwrap_or(1))
                .map(|index| {
                    Value::Tuple(
                        choices
                            .iter()
                            .map(|values| values[index % values.len()].clone())
                            .collect(),
                    )
                })
                .collect()
        }
        Type::List(ty) | Type::Set(ty) => {
            let items = if depth == 0 {
                vec![Value::Null]
            } else {
                edges(ty, heap, depth - 1)
            };
            let contents = [
                Vec::new(),
                items.iter().take(3).cloned().collect(),
                (0..256)
                    .map(|index| items[index % items.len()].clone())
                    .collect(),
            ];
            contents
                .into_iter()
                .map(|mut items| {
                    if matches!(ty_outer, Type::Set(_)) {
                        let mut seen = std::collections::HashSet::new();
                        items.retain(|value| {
                            heap.key(value)
                                .unwrap_or_else(|error| panic!("fixture key: {error}"))
                                .is_some_and(|key| seen.insert(key))
                        });
                        Value::Set(heap.object(Object::Set(items)))
                    } else {
                        Value::List(heap.object(Object::List(items)))
                    }
                })
                .collect()
        }

        Type::Map(map) => {
            let keys: Vec<_> = edges(&map.key, heap, depth.saturating_sub(1))
                .into_iter()
                .filter(|key| {
                    heap.key(key)
                        .unwrap_or_else(|error| panic!("fixture key: {error}"))
                        .is_some()
                })
                .collect();
            let values = edges(&map.value, heap, depth.saturating_sub(1));
            let mut maps = vec![Value::Map(heap.object(Object::Map(vec![])))];
            for index in 0..keys.len().max(values.len()) {
                if keys.is_empty() {
                    break;
                }
                maps.push(Value::Map(heap.object(Object::Map(vec![(
                    keys[index % keys.len()].clone(),
                    values[index % values.len()].clone(),
                )]))));
            }
            maps
        }
        Type::Record(record) => {
            let choices: Vec<_> = record
                .fields
                .iter()
                .map(|field| edges(&field.ty, heap, depth.saturating_sub(1)))
                .collect();
            let rest = record
                .rest
                .as_ref()
                .map(|ty| edges(ty, heap, depth.saturating_sub(1)));
            let count = choices
                .iter()
                .map(Vec::len)
                .chain(rest.iter().map(Vec::len))
                .max()
                .unwrap_or(1);
            (0..=count)
                .map(|index| {
                    let mut fields: Vec<_> = record
                        .fields
                        .iter()
                        .zip(&choices)
                        .filter(|(field, _)| index > 0 || !field.optional)
                        .map(|(field, values)| {
                            (field.name.clone(), values[index % values.len()].clone())
                        })
                        .collect();
                    if index > 0
                        && let Some(values) = &rest
                    {
                        fields.push(("corpus".into(), values[index % values.len()].clone()));
                    }
                    Value::Record(heap.object(Object::Record(fields)))
                })
                .collect()
        }
    };
    values.truncate(EDGES);
    if values.is_empty() {
        values.push(Value::Null);
    }
    values
}

struct Random(u64);
impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn random_value(ty: &Type, heap: &mut RecordedObjects, random: &mut Random, depth: usize) -> Value {
    match ty {
        Type::Int => Value::Int(Integer::from(random.next() as i64)),
        Type::Float => Value::Float(Float::new(f64::from_bits(random.next()))),
        Type::Number => random_value(
            if random.next().is_multiple_of(2) {
                &Type::Int
            } else {
                &Type::Float
            },
            heap,
            random,
            depth,
        ),
        Type::Text => Value::text(
            (0..random.next() % 128)
                .map(|_| {
                    let code = random.next() % 0x110000;
                    char::from_u32(code as u32).unwrap_or('🦀')
                })
                .collect::<String>(),
        ),
        Type::Bytes => Value::Bytes(Bytes::new(
            (0..random.next() % 128)
                .map(|_| random.next() as u8)
                .collect::<Vec<_>>(),
        )),
        Type::List(ty) if depth > 0 => {
            let values = (0..random.next() % 8)
                .map(|_| random_value(ty, heap, random, depth - 1))
                .collect();
            Value::List(heap.object(Object::List(values)))
        }
        Type::Tuple(types) => Value::Tuple(
            types
                .iter()
                .map(|ty| random_value(ty, heap, random, depth.saturating_sub(1)))
                .collect(),
        ),
        Type::Union(types) => random_value(
            &types[random.next() as usize % types.len()],
            heap,
            random,
            depth,
        ),
        Type::Any if depth > 0 => {
            let types = [
                Type::Null,
                Type::Bool,
                Type::Int,
                Type::Float,
                Type::Text,
                Type::List(Box::new(Type::Any)),
            ];
            random_value(
                &types[random.next() as usize % types.len()],
                heap,
                random,
                depth - 1,
            )
        }
        _ => {
            let values = edges(ty, heap, 0);
            values[random.next() as usize % values.len()].clone()
        }
    }
}

fn deep_value(
    ty: &Type,
    heap: &mut RecordedObjects,
    layers: usize,
    family: usize,
) -> Option<Value> {
    if layers == 0 {
        return Some(Value::Null);
    }
    Some(match ty {
        Type::Any => {
            let value = deep_value(ty, heap, layers - 1, family)?;
            match family {
                0 => Value::List(heap.object(Object::List(vec![value]))),
                1 => Value::Map(heap.object(Object::Map(vec![(Value::text("x"), value)]))),
                _ => Value::Record(heap.object(Object::Record(vec![("x".into(), value)]))),
            }
        }
        Type::List(item) => {
            let value = deep_value(item, heap, layers - 1, family)?;
            Value::List(heap.object(Object::List(vec![value])))
        }
        Type::Map(map) => {
            let key = edges(&map.key, heap, 0).into_iter().next()?;
            let value = deep_value(&map.value, heap, layers - 1, family)?;
            Value::Map(heap.object(Object::Map(vec![(key, value)])))
        }
        Type::Set(item) if **item == Type::Any => {
            let mut value = Value::Null;
            for _ in 0..layers {
                value = Value::Tuple(vec![value]);
            }
            Value::Set(heap.object(Object::Set(vec![value])))
        }
        Type::Record(record) => {
            let mut nested = false;
            let mut fields = Vec::new();
            for field in &record.fields {
                let value = if let Some(value) = deep_value(&field.ty, heap, layers - 1, family) {
                    nested = true;
                    value
                } else {
                    edges(&field.ty, heap, 0).into_iter().next()?
                };
                fields.push((field.name.clone(), value));
            }
            if let Some(rest) = &record.rest {
                fields.push(("corpus".into(), deep_value(rest, heap, layers - 1, family)?));
                nested = true;
            }
            if !nested {
                return None;
            }
            Value::Record(heap.object(Object::Record(fields)))
        }
        Type::Union(types) => {
            return types
                .iter()
                .find_map(|ty| deep_value(ty, heap, layers, family));
        }
        _ => return None,
    })
}

fn key_fits(ty: &Type, value: &Value) -> bool {
    match ty {
        Type::Any => true,
        Type::Int => matches!(value, Value::Int(_)),
        Type::Float => matches!(value, Value::Float(_)),
        Type::Number => matches!(value, Value::Int(_) | Value::Float(_)),
        Type::Union(types) => types.iter().any(|ty| key_fits(ty, value)),
        _ => false,
    }
}

impl ErrorValue {
    pub(crate) fn new(kind: &str, message: &str) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            data: Value::Null,
        }
    }
}
impl Value {
    pub(crate) fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
}

#[derive(Default)]
pub(crate) struct RecordedObjects {
    pub(crate) objects: Vec<Object>,
}
impl RecordedObjects {
    pub(crate) fn object(&mut self, object: Object) -> u64 {
        let keys: Vec<_> = match &object {
            Object::Set(values) => values.iter().collect(),
            Object::Map(entries) => entries.iter().map(|(key, _)| key).collect(),
            _ => Vec::new(),
        };
        for key in keys {
            assert!(
                self.key(key)
                    .unwrap_or_else(|error| panic!("fixture key: {error}"))
                    .is_some(),
                "the generator must refuse an illegal fixture key"
            );
        }
        let id = self.objects.len() as u64;
        self.objects.push(object);
        id
    }
    pub(crate) fn key(
        &self,
        value: &Value,
    ) -> Result<Option<lash_kernel_doc::heap::NormalKey>, String> {
        let replay = super::probe::fixtures::Replay::load(&Input {
            args: vec![value.clone()],
            objects: self.objects.clone(),
            work: 0,
            room: 1 << 30,
        })?;
        replay
            .arena
            .semantics()
            .normal_key(&replay.args[0])
            .map_err(|e| e.to_string())
    }
}
