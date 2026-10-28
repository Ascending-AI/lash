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
            Body::Bytes(bytes) => {
                let whole: Vec<_> = bytes
                    .chunks
                    .iter()
                    .flat_map(|chunk| match chunk {
                        Chunk::Resident(bytes) => bytes.as_ref(),
                        _ => panic!("resident bytes"),
                    })
                    .copied()
                    .collect();
                bytes.chunks = if split {
                    whole
                        .chunks(64)
                        .map(|piece| Chunk::Resident(piece.to_vec().into_boxed_slice()))
                        .collect()
                } else {
                    vec![Chunk::Resident(whole.into_boxed_slice())].into_boxed_slice()
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
        Value::ObjectRef(_)
        | Value::TaskRef(_)
        | Value::Closure(_)
        | Value::List(_)
        | Value::Map(_)
        | Value::Set(_)
        | Value::Record(_) => Cost::REFERENCE,
        Value::Task(_) => Cost::REFERENCE,
        Value::Tuple(id) => {
            let lash_kernel_doc::arena::Body::Tuple(sequence) = bench.arena().body(*id).unwrap()
            else {
                panic!("tuple body")
            };
            Cost::REFERENCE
                + sequence
                    .chunks
                    .iter()
                    .map(|chunk| match chunk {
                        lash_kernel_doc::arena::Chunk::Resident(values) => {
                            values.iter().map(|v| value_cost(bench, v)).sum::<u64>()
                        }
                        _ => panic!("resident tuple"),
                    })
                    .sum::<u64>()
        }
        Value::Error(id) => {
            let lash_kernel_doc::arena::Body::Error(error) = bench.arena().body(*id).unwrap()
            else {
                panic!("error body")
            };
            value_cost(bench, &error.kind)
                + value_cost(bench, &error.message)
                + value_cost(bench, &error.data)
                - Cost::REFERENCE
        }
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
    check_values(native, name, bench, values, reset, peak)
}

pub(crate) fn check_values(
    native: &dyn lash_kernel_doc::NativeFunction,
    name: &str,
    mut bench: Bench,
    values: Vec<Value>,
    reset: impl Fn(),
    peak: impl Fn() -> u64,
) -> Option<Contract> {
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
    let actual_scratch = peak();
    let verify = |contract: &Contract| -> Result<(), String> {
        let fail = |what: &str| Err(format!("{name}: {what}"));
        if contract.exit != exit {
            return fail("exit");
        }
        if contract.output.bytes != output {
            return Err(format!(
                "{name}: output occurrence declared {} observed {output}",
                contract.output.bytes
            ));
        }
        let events: Vec<_> = contract
            .events
            .iter()
            .filter_map(|e| match e {
                Event::Entity {
                    owner,
                    kind,
                    members,
                    release,
                } => Some((owner, kind, members, release)),
                _ => None,
            })
            .collect();
        if events.len() != actual.len() {
            return fail("fresh identities");
        }
        let origin = |value: &Value| {
            if !matches!(
                value,
                Value::List(_)
                    | Value::Map(_)
                    | Value::Set(_)
                    | Value::Record(_)
                    | Value::Closure(_)
            ) {
                return Origin::Immutable;
            }
            match actual
                .iter()
                .position(|(handle, _, _)| Some(*handle) == value.handle())
            {
                Some(at) => Origin::Fresh(*events[at].0),
                None => Origin::Alias,
            }
        };
        let mut reachable = Vec::new();
        let mut pending: Vec<_> = output_value.into_iter().copied().collect();
        while let Some(value) = pending.pop() {
            match value {
                Value::Tuple(handle) => {
                    let lash_kernel_doc::arena::Body::Tuple(sequence) =
                        bench.arena().body(handle).unwrap()
                    else {
                        panic!("tuple body")
                    };
                    for chunk in sequence.chunks.iter() {
                        match chunk {
                            lash_kernel_doc::arena::Chunk::Resident(values) => {
                                pending.extend(values)
                            }
                            _ => panic!("resident tuple"),
                        }
                    }
                }
                Value::Error(handle) => {
                    let lash_kernel_doc::arena::Body::Error(error) =
                        bench.arena().body(handle).unwrap()
                    else {
                        panic!("error body")
                    };
                    pending.extend([error.kind, error.message, error.data]);
                }
                _ => {}
            }
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
        for ((_, kind, members, release), (handle, actual_kind, actual_members)) in
            events.iter().zip(&actual)
        {
            if *kind != actual_kind || members.len() != actual_members.len() {
                return fail("entity kind or direct members");
            }
            for (member, value) in members.iter().zip(actual_members) {
                if member.bytes != value_cost(&bench, value) || member.origin != origin(value) {
                    return fail("member occurrence or identity");
                }
            }
            if release.is_none() != reachable.contains(handle) {
                return fail("entity lifetime");
            }
            if reachable.contains(handle) {
                retained +=
                    Cost::object(actual_members.iter().map(|v| value_cost(&bench, v)).sum());
                if let Entity::Record { field_name_bytes } = actual_kind {
                    retained += field_name_bytes;
                }
            }
        }
        if contract.retained_bytes() != Some(retained) {
            return fail("retained holdings or invalid schedule");
        }
        if let Some(value) = output_value
            && contract.output.origin != origin(value)
        {
            return fail("output identity");
        }
        if contract
            .peak_scratch_bytes()
            .is_none_or(|bound| actual_scratch > bound)
        {
            return Err(format!(
                "{name}: observed scratch {actual_scratch} exceeds {:?}",
                contract.peak_scratch_bytes()
            ));
        }
        Ok(())
    };
    verify(&contract).unwrap_or_else(|error| panic!("{error}: {contract:?}"));
    if actual_scratch > 0 {
        let mut bad = contract.clone();
        bad.events
            .retain(|event| !matches!(event, Event::Scratch { .. }));
        assert!(
            verify(&bad).is_err(),
            "{name}: omitted observed scratch was accepted"
        );
    }
    if !actual.is_empty() {
        let mut bad = contract.clone();
        let at = bad
            .events
            .iter()
            .position(|event| matches!(event, Event::Entity { .. }))
            .unwrap();
        bad.events.remove(at);
        assert!(
            verify(&bad).is_err(),
            "{name}: omitted observed entity was accepted"
        );
    }
    let mut understated = contract.clone();
    understated.output.bytes -= 1;
    verify(&understated).expect_err("actual-event oracle accepted an understated output");
    Some(contract)
}
