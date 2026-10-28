//! Independent installed-value and allocation observer shared by native laws.
use lash_kernel_doc::{
    Datum, ErrorDatum, LogicalHeldV1 as Cost, NativeCall, NativeError,
    NativeResourceContract as Contract, NativeResourceEntity as Entity,
    NativeResourceEvent as Event, NativeResourceExit as Exit, NativeResourceOrigin as Origin,
    NewObject, Value, WorkCounter,
};
use lash_kernel_state::arena::Bench;

fn rechunk(
    bench: &mut Bench,
    roots: &[Value],
    split: bool,
) -> Vec<lash_kernel_doc::arena::ArenaHandle> {
    use lash_kernel_doc::arena::{Body, Chunk, Chunks, Sequence};
    let mut pending = roots.to_vec();
    let mut seen = Vec::new();
    while let Some(value) = pending.pop() {
        let Some(handle) = value.handle() else {
            continue;
        };
        if seen.contains(&handle) {
            continue;
        }
        seen.push(handle);
        let (body, _) = bench.arena_mut().parts_mut(handle).unwrap();
        match body {
            Body::List(sequence) | Body::Tuple(sequence) => {
                let values: Vec<_> = sequence
                    .chunks
                    .iter()
                    .flat_map(|chunk| match chunk {
                        Chunk::Resident(values) => values,
                        _ => panic!("new input is resident"),
                    })
                    .copied()
                    .collect();
                pending.extend_from_slice(&values);
                sequence.chunks = if values.is_empty() || !split {
                    Chunks::whole(values)
                } else {
                    Chunks::Split(
                        values
                            .chunks(Sequence::CHUNK)
                            .map(|chunk| Chunk::Resident(chunk.to_vec()))
                            .collect(),
                    )
                };
            }
            Body::Map(table) | Body::Set(table) | Body::Record(table) => {
                for chunk in table.chunks.iter() {
                    match chunk {
                        Chunk::Resident(values) => pending.extend_from_slice(values),
                        _ => panic!("new input is resident"),
                    }
                }
            }
            Body::LongText(text) => {
                let whole: String = text
                    .chunks
                    .iter()
                    .map(|chunk| match chunk {
                        Chunk::Resident(text) => text.as_str(),
                        _ => panic!("new input is resident"),
                    })
                    .collect();
                let chars: Vec<_> = whole.chars().collect();
                text.chunks = if split {
                    chars
                        .chunks(64)
                        .map(|chunk| Chunk::Resident(chunk.iter().collect()))
                        .collect()
                } else {
                    vec![Chunk::Resident(whole)].into_boxed_slice()
                };
            }
            Body::Error(error) => pending.extend([error.kind, error.message, error.data]),
            _ => {}
        }
    }
    seen
}

fn local(value: &Datum) -> u64 {
    Cost::REFERENCE
        + match value {
            Datum::Int(value) => value.bits().div_ceil(8),
            Datum::Text(value) => value.len() as u64,
            Datum::Bytes(value) => value.as_slice().len() as u64,
            Datum::Timestamp(value) => value.nanoseconds.bits().div_ceil(8),
            Datum::Tuple(values) => values.iter().map(local).sum(),
            Datum::Error(error) => {
                (error.kind.len() + error.message.len()) as u64 + local(&error.data)
            }
            Datum::Handle(handle) => (handle.kind.len() + handle.id.len()) as u64,
            _ => 0,
        }
}

fn value_cost(bench: &Bench, value: &Value) -> u64 {
    match value {
        Value::Function(_) => Cost::REFERENCE + 64,
        Value::ObjectRef(_) | Value::TaskRef(_) | Value::Closure(_) => Cost::REFERENCE,
        _ => local(&bench.datum(value).unwrap()),
    }
}

pub(crate) fn check_case(
    native: &dyn lash_kernel_doc::NativeFunction,
    name: &str,
    input: Vec<Datum>,
    reset: impl Fn(),
    peak: impl Fn() -> u64,
) -> Option<Contract> {
    let mut bench = Bench::new();
    let values: Vec<_> = input
        .into_iter()
        .map(|value| bench.build(&value).unwrap())
        .collect();
    let (prediction, _) = bench
        .run(&values, u64::MAX, |args, _| native.resource_contract(args))
        .unwrap();
    let contract = match prediction {
        Ok(Some(contract)) => contract,
        Ok(None) | Err(NativeError::Decline) => return None,
        Err(error) => panic!("{name}: prediction {error:?}"),
    };
    rechunk(&mut bench, &values, true);
    let (chunked, _) = bench
        .run(&values, u64::MAX, |args, _| native.resource_contract(args))
        .unwrap();
    match chunked {
        Ok(chunked) => assert_eq!(chunked, Some(contract.clone()), "{name} chunk geometry"),
        Err(NativeError::Retry(lash_kernel_doc::Retry::Cold { .. })) => {
            // Equality can request a flat read even for resident chunks.
            rechunk(&mut bench, &values, false);
            let (warmed, _) = bench
                .run(&values, u64::MAX, |args, _| native.resource_contract(args))
                .unwrap();
            assert_eq!(
                warmed.unwrap(),
                Some(contract.clone()),
                "{name} after read retry"
            );
        }
        Err(error) => panic!("{name} chunked prediction: {error:?}"),
    }
    // Force the same input through the real cold-body protocol. Chilled keeps
    // its exact identity and summary; each Retry::Cold restores only that body.
    let handles = rechunk(&mut bench, &values, true);
    let mut chilled: Vec<_> = handles
        .into_iter()
        .filter_map(|handle| {
            bench
                .arena_mut()
                .chill(handle)
                .unwrap()
                .map(|body| (handle, body))
        })
        .collect();
    loop {
        let (cold, _) = bench
            .run(&values, u64::MAX, |args, _| native.resource_contract(args))
            .unwrap();
        match cold {
            Err(NativeError::Retry(lash_kernel_doc::Retry::Cold { object, .. })) => {
                if let Some(at) = chilled.iter().position(|(handle, _)| *handle == object) {
                    let (handle, body) = chilled.remove(at);
                    bench.arena_mut().thaw(handle, body).unwrap();
                } else {
                    assert!(
                        bench.arena_mut().join_chunks(object).unwrap(),
                        "{name} cold read made no progress"
                    );
                }
            }
            answer => {
                assert_eq!(
                    answer.unwrap(),
                    Some(contract.clone()),
                    "{name} cold-page contract"
                );
                break;
            }
        }
    }
    for (handle, body) in chilled {
        bench.arena_mut().thaw(handle, body).unwrap();
    }
    reset();
    let ((answer, built), _) = bench
        .run(&values, u64::MAX, |args, new| {
            let answer = native.call(NativeCall {
                args,
                new,
                counter: &mut WorkCounter::new(None),
            });
            (answer, new.built().to_vec())
        })
        .unwrap();
    // The data decoder needs whole views. Normalize only after execution,
    // without changing values or identities, before reading its oracle.
    rechunk(&mut bench, &values, false);
    let (exit, output, output_value) = match &answer {
        Ok(value) => (Exit::Returned, value_cost(&bench, value), Some(value)),
        Err(NativeError::Raised(value)) => (Exit::Raised, value_cost(&bench, value), Some(value)),
        Err(NativeError::Refused { kind, message }) => (
            Exit::Raised,
            local(&Datum::Error(Box::new(ErrorDatum {
                kind: (*kind).into(),
                message: message.clone(),
                data: Datum::Null,
            }))),
            None,
        ),
        Err(error) => panic!("{name}: execution {error:?}"),
    };
    assert_eq!(contract.exit, exit, "{name} exit");
    assert_eq!(contract.output.bytes, output, "{name} output occurrence");
    let mut actual = Vec::new();
    for (handle, object) in &built {
        let (kind, members) = match object {
            NewObject::List(values) => (Entity::List, values.clone()),
            NewObject::Map(pairs) => (
                Entity::Map,
                pairs
                    .iter()
                    .flat_map(|(key, value)| [*key, *value])
                    .collect(),
            ),
            NewObject::Set(values) => (
                Entity::Set,
                values
                    .iter()
                    .flat_map(|value| [*value, Value::Absent])
                    .collect(),
            ),
            NewObject::Record(fields) => (
                Entity::Record {
                    field_name_bytes: fields
                        .iter()
                        .map(|(key, _)| match bench.datum(key).unwrap() {
                            Datum::Text(text) => text.len() as u64,
                            _ => panic!("record key"),
                        })
                        .sum(),
                },
                fields.iter().map(|(_, value)| *value).collect(),
            ),
            _ => continue,
        };
        actual.push((*handle, kind, members));
    }
    let events: Vec<_> = contract
        .events
        .iter()
        .filter_map(|event| match event {
            Event::Entity {
                owner,
                kind,
                members,
                release,
            } => Some((owner, kind, members, release)),
            _ => None,
        })
        .collect();
    assert_eq!(events.len(), actual.len(), "{name} fresh identities");
    let mut reachable = Vec::new();
    let mut pending: Vec<_> = output_value.into_iter().copied().collect();
    while let Some(value) = pending.pop() {
        if let Some((handle, _, members)) = actual
            .iter()
            .find(|(handle, _, _)| Some(*handle) == value.handle())
            && !reachable.contains(handle)
        {
            reachable.push(*handle);
            pending.extend(members);
        }
    }
    let mut retained = output;
    for ((_, expected_kind, expected_members, release), (handle, kind, members)) in
        events.iter().zip(&actual)
    {
        assert_eq!(
            release.is_none(),
            reachable.contains(handle),
            "{name} entity exit lifetime"
        );
        assert_eq!(**expected_kind, *kind, "{name} entity kind/names");
        assert_eq!(
            expected_members.len(),
            members.len(),
            "{name} direct members"
        );
        let bytes: u64 = members.iter().map(|value| value_cost(&bench, value)).sum();
        for (expected, value) in expected_members.iter().zip(members) {
            assert_eq!(
                expected.bytes,
                value_cost(&bench, value),
                "{name} member occurrence"
            );
            let mutable = matches!(
                value,
                Value::List(_)
                    | Value::Map(_)
                    | Value::Set(_)
                    | Value::Record(_)
                    | Value::Closure(_)
            );
            let fresh = actual
                .iter()
                .any(|(handle, _, _)| Some(*handle) == value.handle());
            assert_eq!(
                matches!(expected.origin, Origin::Alias),
                mutable && !fresh,
                "{name} member alias"
            );
            if let Origin::Fresh(owner) = expected.origin {
                let target = events
                    .iter()
                    .zip(&actual)
                    .find(|((candidate, _, _, _), _)| **candidate == owner)
                    .unwrap()
                    .1
                    .0;
                assert_eq!(value.handle(), Some(target), "{name} fresh member target");
            }
            assert_eq!(
                matches!(expected.origin, Origin::Fresh(_)),
                mutable && fresh,
                "{name} member identity"
            );
        }
        if reachable.contains(handle) {
            retained += Cost::object(bytes);
            if let Entity::Record { field_name_bytes } = kind {
                retained += field_name_bytes;
            }
        }
    }
    assert_eq!(
        contract.retained_bytes(),
        Some(retained),
        "{name} retained holdings"
    );
    if let Some(value) = output_value {
        if let Origin::Fresh(owner) = contract.output.origin {
            let target = events
                .iter()
                .zip(&actual)
                .find(|((candidate, _, _, _), _)| **candidate == owner)
                .unwrap()
                .1
                .0;
            assert_eq!(value.handle(), Some(target), "{name} output target");
        }
        let fresh = actual
            .iter()
            .any(|(handle, _, _)| Some(*handle) == value.handle());
        let mutable = matches!(
            value,
            Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) | Value::Closure(_)
        );
        assert_eq!(
            matches!(contract.output.origin, Origin::Fresh(_)),
            fresh,
            "{name} output identity"
        );
        assert_eq!(
            matches!(contract.output.origin, Origin::Alias),
            mutable && !fresh,
            "{name} output alias"
        );
    }
    let actual_scratch = peak();
    assert!(
        actual_scratch <= contract.peak_scratch_bytes().unwrap(),
        "{name}: actual scratch {actual_scratch} exceeds {contract:?}"
    );
    Some(contract)
}
