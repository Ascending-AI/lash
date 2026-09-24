//! The durable parts: what a reload restores, what a capture rewrites, and
//! what a reader refuses (FIG-3605, FIG-3606).

use super::*;

fn heap_backed(state: &State) -> &HeapBackedState {
    let StateMode::HeapBacked(backed) = &state.mode else {
        panic!("expected a heap-backed state")
    };
    backed
}

fn heap_backed_mut(state: &mut State) -> &mut HeapBackedState {
    let StateMode::HeapBacked(backed) = &mut state.mode else {
        panic!("expected a heap-backed state")
    };
    backed
}

fn install(roots: Vec<(&str, Value)>, heap: Heap) -> State {
    let mut runtime_globals = Record::new();
    for (name, value) in roots {
        runtime_globals.insert(name.to_string(), value);
    }
    let mut state = State::new();
    state
        .install_runtime(runtime_globals, heap)
        .expect("install the runtime roots");
    state
}

fn complete(state: &State) -> DurableParts {
    state
        .durable_parts(&DurableBaseline::default())
        .expect("capture every fragment")
}

fn bodies(parts: &DurableParts) -> BTreeMap<String, Vec<u8>> {
    parts
        .fragments
        .iter()
        .map(|(name, fragment)| match fragment {
            DurableFragment::Changed(body) => (name.clone(), body.clone()),
            DurableFragment::Unchanged => panic!("`{name}` has no body in this capture"),
        })
        .collect()
}

fn reload(header: &[u8], bodies: &BTreeMap<String, Vec<u8>>) -> (State, DurableBaseline) {
    State::from_durable_parts(
        header,
        bodies
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_slice())),
    )
    .expect("reload the durable parts")
}

fn changed_names(parts: &DurableParts) -> Vec<&str> {
    parts
        .fragments
        .iter()
        .filter(|(_, fragment)| matches!(fragment, DurableFragment::Changed(_)))
        .map(|(name, _)| name.as_str())
        .collect()
}

fn record_keys(heap: &Heap, value: &Value) -> Vec<String> {
    let Value::Ref(id) = value else {
        panic!("expected a heap record, got {value:?}")
    };
    let HeapObject::Record(record) = heap.get(*id).expect("the record is live") else {
        panic!("expected a record object")
    };
    record.keys().map(str::to_string).collect()
}

/// Every TypeScript exotic the dialect accepts, rooted as a session global, and
/// one `Map` bound under two names. None of them has a host view, so before
/// FIG-3605 none of them reached the wire at all.
fn exotic_session() -> State {
    let mut heap = Heap::default();
    let shared = heap
        .allocate_list(vec![Value::String("shared".into())])
        .expect("shared list");
    let map = heap
        .allocate_map(vec![
            (Value::String("b".into()), Value::Number(1.0)),
            (Value::String("a".into()), shared.clone()),
        ])
        .expect("Map");
    let set = heap
        .allocate_set(vec![Value::Number(3.0), Value::Number(1.0), shared])
        .expect("Set");
    let date = heap.allocate_date(86_400_000.0).expect("Date");
    let regexp = heap
        .allocate_regexp("a+".to_string(), "g".to_string())
        .expect("RegExp");
    let Value::Ref(regexp_id) = regexp else {
        unreachable!("an allocation is a reference")
    };
    heap.set_regexp_last_index(regexp_id, 3)
        .expect("set lastIndex");
    let url = heap
        .allocate_url("https://example.test/path?q=1&r=2", None)
        .expect("URL");
    let params = heap
        .allocate_url_search_params(vec![
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string()),
        ])
        .expect("URLSearchParams");
    install(
        vec![
            ("date", date),
            ("map", map.clone()),
            ("map_alias", map),
            ("params", params),
            ("regexp", regexp),
            ("set", set),
            ("url", url),
        ],
        heap,
    )
}

#[test]
fn every_typescript_exotic_survives_a_durable_reload() {
    let state = exotic_session();
    assert_eq!(
        state.globals().len(),
        0,
        "precondition: the host view carries none of these bindings"
    );
    assert_eq!(
        state.binding_names().collect::<Vec<_>>(),
        ["date", "map", "map_alias", "params", "regexp", "set", "url"],
        "the roots own every one of them"
    );

    let parts = complete(&state);
    let (reloaded, _) = reload(&parts.header, &bodies(&parts));
    assert_eq!(reloaded, state, "a reload is the heap the live run had");
    let backed = heap_backed(&reloaded);
    assert_eq!(
        backed.runtime_globals.get("map"),
        backed.runtime_globals.get("map_alias"),
        "two bindings to one Map still name one object"
    );
    let Some(Value::Ref(regexp_id)) = backed.runtime_globals.get("regexp") else {
        panic!("the RegExp binding is a reference")
    };
    let HeapObject::RegExp(regexp) = backed.heap.get(*regexp_id).expect("the RegExp is live")
    else {
        panic!("the RegExp binding names a RegExp")
    };
    assert_eq!(regexp.last_index, 3, "lastIndex is durable state");
    assert_eq!(
        complete(&reloaded).header,
        parts.header,
        "the heap counters come back exactly"
    );
}

#[test]
fn property_order_survives_a_durable_reload_in_both_state_modes() {
    let order = |names: &[&str]| {
        let mut record = Record::new();
        for (index, name) in names.iter().enumerate() {
            record.insert((*name).to_string(), Value::Number(index as f64));
        }
        record
    };

    // Plain: a state that has never executed holds its records inline.
    let mut plain = State::new();
    plain
        .insert_global(
            "ordered",
            Value::Record(Arc::new(order(&["zeta", "alpha", "mid"]))),
        )
        .expect("seed an inline record");
    let parts = complete(&plain);
    let (reloaded, _) = reload(&parts.header, &bodies(&parts));
    assert_eq!(
        reloaded.globals()["ordered"]
            .as_record()
            .expect("the record reloads")
            .keys()
            .collect::<Vec<_>>(),
        ["zeta", "alpha", "mid"]
    );

    // Heap-backed: the record is a heap object, nested under another one.
    let mut heap = Heap::default();
    let inner = heap
        .allocate_record(order(&["y", "x"]))
        .expect("inner record");
    let mut outer = Record::new();
    outer.insert("b".to_string(), inner);
    outer.insert("a".to_string(), Value::Number(1.0));
    let outer = heap.allocate_record(outer).expect("outer record");
    let state = install(vec![("nested", outer)], heap);
    let parts = complete(&state);
    let (reloaded, _) = reload(&parts.header, &bodies(&parts));
    let backed = heap_backed(&reloaded);
    let outer = &backed.runtime_globals["nested"];
    assert_eq!(record_keys(&backed.heap, outer), ["b", "a"]);
    let Value::Ref(outer_id) = outer else {
        unreachable!("checked above")
    };
    let HeapObject::Record(outer_record) = backed.heap.get(*outer_id).expect("outer is live")
    else {
        unreachable!("checked above")
    };
    assert_eq!(record_keys(&backed.heap, &outer_record["b"]), ["y", "x"]);
}

/// Change detection has to see a write through any alias. `a` and `b` share
/// one list, which `a` carries because it is first in name order; a write
/// through `b`'s path rewrites `a`'s fragment and nothing else.
#[test]
fn a_capture_rewrites_exactly_the_fragments_whose_objects_changed() {
    let mut heap = Heap::default();
    let shared = heap
        .allocate_list(vec![Value::Number(1.0)])
        .expect("shared list");
    let mut holder = Record::new();
    holder.insert("list".to_string(), shared.clone());
    let holder = heap.allocate_record(holder).expect("holder");
    let untouched = heap
        .allocate_list(vec![Value::String("still".into())])
        .expect("untouched list");
    let mut state = install(
        vec![
            ("a", shared.clone()),
            ("b", holder.clone()),
            ("c", untouched),
        ],
        heap,
    );
    let first = complete(&state);
    assert_eq!(changed_names(&first), ["a", "b", "c"]);

    let quiet = state.durable_parts(&first.baseline).expect("capture again");
    assert!(
        changed_names(&quiet).is_empty(),
        "nothing changed, so nothing is rewritten"
    );

    heap_backed_mut(&mut state)
        .heap
        .push_list(&shared, Value::Number(2.0))
        .expect("push through the alias");
    let after_push = state
        .durable_parts(&quiet.baseline)
        .expect("capture the push");
    assert_eq!(
        changed_names(&after_push),
        ["a"],
        "the list is carried by `a`, however it was reached"
    );

    let Value::Ref(holder_id) = holder else {
        unreachable!("an allocation is a reference")
    };
    let mut replaced = Record::new();
    replaced.insert("list".to_string(), shared);
    replaced.insert("extra".to_string(), Value::Bool(true));
    heap_backed_mut(&mut state)
        .heap
        .replace_object(holder_id, HeapObject::Record(Box::new(replaced)))
        .expect("write the holder");
    let after_write = state
        .durable_parts(&after_push.baseline)
        .expect("capture the write");
    assert_eq!(changed_names(&after_write), ["b"]);

    state
        .insert_global("a", Value::Number(0.0))
        .expect("rebind `a`");
    let after_rebind = state
        .durable_parts(&after_write.baseline)
        .expect("capture the rebind");
    assert_eq!(
        changed_names(&after_rebind),
        ["a", "b"],
        "`b` now carries the list `a` let go of"
    );

    // What was captured incrementally reloads to the live state.
    let mut stored = bodies(&first);
    for parts in [&after_push, &after_write, &after_rebind] {
        for (name, fragment) in &parts.fragments {
            if let DurableFragment::Changed(body) = fragment {
                stored.insert(name.clone(), body.clone());
            }
        }
    }
    stored.retain(|name, _| after_rebind.fragments.contains_key(name));
    let (reloaded, _) = reload(&after_rebind.header, &stored);
    let live = complete(&state);
    let again = complete(&reloaded);
    assert_eq!(again.header, live.header);
    assert_eq!(bodies(&again), bodies(&live));
}

/// A heap rebuilt from a wire — a reload, or a parked continuation resuming —
/// must not read as unchanged against a capture taken before it was rebuilt,
/// or a write made before the rebuild would never be captured.
#[test]
fn a_rebuilt_heap_is_unlike_every_capture_taken_before_it() {
    let state = exotic_session();
    let parts = complete(&state);
    let (reloaded, reload_baseline) = reload(&parts.header, &bodies(&parts));
    assert!(
        changed_names(
            &reloaded
                .durable_parts(&reload_baseline)
                .expect("capture the reload")
        )
        .is_empty(),
        "a reload is clean against the baseline it returns"
    );
    let against_old = reloaded
        .durable_parts(&parts.baseline)
        .expect("capture against the pre-reload baseline");
    assert_eq!(
        changed_names(&against_old),
        ["date", "map", "params", "regexp", "set", "url"],
        "every fragment that carries an object reads as changed; `map_alias` \
         carries none, so its body still stands"
    );
}

/// Each root's body as a capture leaves it: the body it rewrote, or the body
/// the capture it was diffed against recorded.
fn resolved_bodies(
    parts: &DurableParts,
    prior: &BTreeMap<String, Vec<u8>>,
) -> BTreeMap<String, Vec<u8>> {
    parts
        .fragments
        .iter()
        .map(|(name, fragment)| match fragment {
            DurableFragment::Changed(body) => (name.clone(), body.clone()),
            DurableFragment::Unchanged => (
                name.clone(),
                prior
                    .get(name)
                    .unwrap_or_else(|| panic!("`{name}` is unchanged from a body never recorded"))
                    .clone(),
            ),
        })
        .collect()
}

/// The write stamps are drawn from one process-wide clock, so which fragments a
/// capture re-encodes depends on the process's history: a worker that kept the
/// heap since the last capture rewrites only what was written, and a worker
/// that rebuilt it from the wire rewrites every fragment that carries an
/// object. What the capture persists does not: every fragment it rewrites
/// re-encodes to the canonical bytes the unchanged ones already hold, so warm
/// and cold workers leave byte-identical state behind (FIG-3672 CR17).
#[test]
fn warm_and_cold_captures_persist_byte_identical_state() {
    let mut heap = Heap::default();
    let list = heap
        .allocate_list(vec![Value::Number(1.0)])
        .expect("written list");
    let untouched = heap
        .allocate_list(vec![Value::String("still".into())])
        .expect("untouched list");
    let mut warm = install(vec![("list", list), ("untouched", untouched)], heap);
    let first = complete(&warm);
    let stored = bodies(&first);
    let (mut cold, cold_baseline) = reload(&first.header, &stored);

    for state in [&mut warm, &mut cold] {
        let Some(Value::Ref(list)) = heap_backed(state).runtime_globals.get("list").cloned() else {
            panic!("`list` is a heap list")
        };
        heap_backed_mut(state)
            .heap
            .push_list(&Value::Ref(list), Value::Number(2.0))
            .expect("push onto the list");
    }
    let warm_parts = warm.durable_parts(&first.baseline).expect("warm capture");
    let cold_parts = cold.durable_parts(&cold_baseline).expect("cold capture");
    // A cold worker diffing against the capture a warm worker took before the
    // heap was rebuilt rewrites more, and persists the same bytes.
    let rebuilt_parts = cold
        .durable_parts(&first.baseline)
        .expect("rebuilt capture");
    assert!(
        changed_names(&rebuilt_parts).len() > changed_names(&warm_parts).len(),
        "the rebuilt heap re-encodes fragments the warm heap did not"
    );

    assert_eq!(warm_parts.header, cold_parts.header);
    assert_eq!(warm_parts.header, rebuilt_parts.header);
    let persisted = resolved_bodies(&warm_parts, &stored);
    assert_eq!(persisted, resolved_bodies(&cold_parts, &stored));
    assert_eq!(persisted, resolved_bodies(&rebuilt_parts, &stored));
    assert_ne!(persisted, stored, "the push is persisted");
}

#[test]
fn a_reader_refuses_parts_this_writer_would_not_produce() {
    // `a` and `b` share one list; the writer puts it in `a`'s fragment.
    let mut heap = Heap::default();
    let shared = heap
        .allocate_list(vec![Value::Number(1.0)])
        .expect("shared list");
    let state = install(vec![("a", shared.clone()), ("b", shared)], heap);
    let parts = complete(&state);
    let good = bodies(&parts);
    reload(&parts.header, &good);

    let fragment = |value: CanonicalValue, objects: Vec<CanonicalHeapEntry>| {
        rmp_serde::to_vec_named(&CanonicalFragment { value, objects }).expect("encode")
    };
    let decoded: CanonicalFragment =
        rmp_serde::from_slice(&good["a"]).expect("decode `a`'s fragment");
    let carried = decoded.objects.clone();
    let reference = decoded.value.clone();

    // The shared list carried by the second root instead of the first.
    let mut moved = good.clone();
    moved.insert("a".to_string(), fragment(reference.clone(), Vec::new()));
    moved.insert(
        "b".to_string(),
        fragment(reference.clone(), carried.clone()),
    );
    let error = State::from_durable_parts(
        &parts.header,
        moved
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_slice())),
    )
    .expect_err("an object carried by the wrong root is refused");
    assert!(
        matches!(&error, SnapshotDecodeError::NonCanonicalEncoding { location, .. } if location.starts_with("roots.")),
        "{error:?}"
    );

    // The list carried twice.
    let mut twice = good.clone();
    twice.insert("b".to_string(), fragment(reference.clone(), carried));
    assert!(
        State::from_durable_parts(
            &parts.header,
            twice
                .iter()
                .map(|(name, body)| (name.as_str(), body.as_slice())),
        )
        .is_err(),
        "an object carried twice is refused"
    );

    // The list carried by nobody: `b` names an object no fragment holds.
    let mut dangling = good.clone();
    dangling.insert("a".to_string(), fragment(reference, Vec::new()));
    assert!(
        State::from_durable_parts(
            &parts.header,
            dangling
                .iter()
                .map(|(name, body)| (name.as_str(), body.as_slice())),
        )
        .is_err(),
        "a reference to an object no fragment carries is refused"
    );

    // A fragment missing entirely leaves `b` naming an object nothing carries.
    let mut missing = good.clone();
    missing.remove("a");
    assert!(
        State::from_durable_parts(
            &parts.header,
            missing
                .iter()
                .map(|(name, body)| (name.as_str(), body.as_slice())),
        )
        .is_err(),
        "a partial set of fragments is refused"
    );
}

#[test]
fn a_duplicated_record_field_is_refused_not_collapsed() {
    let mut heap = Heap::default();
    let mut record = Record::new();
    record.insert("a".to_string(), Value::Number(1.0));
    let record = heap.allocate_record(record).expect("record");
    let state = install(vec![("r", record)], heap);
    let parts = complete(&state);
    let mut decoded: CanonicalFragment =
        rmp_serde::from_slice(&bodies(&parts)["r"]).expect("decode the fragment");
    let CanonicalHeapObject::Record { fields } = &mut decoded.objects[0].object else {
        panic!("the fragment carries the record")
    };
    fields.push(CanonicalBinding {
        name: "a".to_string(),
        value: CanonicalValue::Number { value: 2.0 },
    });
    let body = rmp_serde::to_vec_named(&decoded).expect("encode the duplicate");
    let error = State::from_durable_parts(&parts.header, [("r", body.as_slice())])
        .expect_err("a record cannot hold one key twice");
    assert!(
        matches!(&error, SnapshotDecodeError::NonCanonicalEncoding { .. }),
        "{error:?}"
    );
}

/// The header's version is read before anything else, so a predecessor's
/// bytes are refused as a version boundary rather than as a shape the reader
/// happens not to recognize. The predecessor here is a real v7 value body: the
/// one-binding snapshot each global was persisted as before this format.
#[test]
fn a_predecessor_header_is_refused_by_its_version() {
    let v7_value_body = [
        0x82, 0xa7, b'v', b'e', b'r', b's', b'i', b'o', b'n', 0x07, 0xa7, b'g', b'l', b'o', b'b',
        b'a', b'l', b's', 0x91, 0x82, 0xa4, b'n', b'a', b'm', b'e', 0xa5, b'v', b'a', b'l', b'u',
        b'e', 0xa5, b'v', b'a', b'l', b'u', b'e', 0x81, 0xa4, b'k', b'i', b'n', b'd', 0xa4, b'n',
        b'u', b'l', b'l',
    ];
    let error = State::from_durable_parts(&v7_value_body, [("value", &v7_value_body[..])])
        .expect_err("a v7 body is not a current header");
    assert_eq!(
        error,
        SnapshotDecodeError::VersionMismatch {
            expected: LASHLANG_SNAPSHOT_VERSION,
            found: 7,
        }
    );
}

fn closure(heap: &mut Heap) -> Value {
    heap.allocate(HeapObject::Closure {
        function: 0,
        captures: Vec::new(),
        name: Some(Value::String("f".into())),
        length: Some(Value::Number(0.0)),
    })
    .expect("allocate a closure")
}

/// A binding that reaches a function is dropped at the boundary and its name
/// is kept, whether the function is the value or sits inside it; a binding the
/// run left alone is untouched (FIG-3608).
#[test]
fn a_boundary_remembers_the_functions_it_drops() {
    let mut heap = Heap::default();
    let direct = closure(&mut heap);
    let nested_closure = closure(&mut heap);
    let mut holder = Record::new();
    holder.insert("run".to_string(), nested_closure);
    let nested = heap
        .allocate(HeapObject::Record(Box::new(holder)))
        .expect("allocate a record holding a closure");
    let state = install(
        vec![
            ("helper", direct),
            ("box", nested),
            ("count", Value::Number(1.0)),
        ],
        heap,
    );
    assert_eq!(
        state.expired_functions().iter().collect::<Vec<_>>(),
        ["box", "helper"]
    );
    assert!(state.binding_names().eq(["count"]));
}

/// A name bound again, by a later run or by a host write, is live again and
/// no longer refused.
#[test]
fn a_rebound_name_is_no_longer_expired() {
    let mut heap = Heap::default();
    let helper = closure(&mut heap);
    let mut state = install(vec![("helper", helper), ("other", Value::Null)], heap);
    assert!(state.expired_functions().contains("helper"));

    let (mut roots, heap) = state.take_runtime();
    roots.insert("helper".to_string(), Value::Number(2.0));
    state
        .install_runtime(roots, heap)
        .expect("install the rebound roots");
    assert!(state.expired_functions().is_empty());

    let mut heap = Heap::default();
    let again = closure(&mut heap);
    let mut state = install(vec![("again", again)], heap);
    state
        .insert_global("again", Value::Bool(true))
        .expect("a host rebinds the name");
    assert!(state.expired_functions().is_empty());
}

/// The dropped names survive a durable reload and a whole-snapshot round
/// trip, so a later cell is refused the same way live and reloaded.
#[test]
fn the_dropped_functions_survive_a_reload() {
    let mut heap = Heap::default();
    let helper = closure(&mut heap);
    let list = heap
        .allocate(HeapObject::List(vec![Value::Number(1.0)]))
        .expect("allocate a list");
    let state = install(vec![("helper", helper), ("items", list)], heap);
    let parts = complete(&state);
    let (reloaded, _) = reload(&parts.header, &bodies(&parts));
    assert_eq!(reloaded.expired_functions(), state.expired_functions());

    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode the whole snapshot");
    let decoded = State::from_snapshot(
        Snapshot::from_canonical_bytes(&bytes).expect("decode the whole snapshot"),
    );
    assert_eq!(decoded.expired_functions(), state.expired_functions());

    // A plain state keeps the names too.
    let mut plain = State::new();
    plain.expired_functions.insert("gone".to_string());
    let parts = complete(&plain);
    let (reloaded, _) = reload(&parts.header, &bodies(&parts));
    assert!(reloaded.expired_functions().contains("gone"));
}

/// A name cannot be both live and dropped, so a wire claiming both is refused.
#[test]
fn a_dropped_name_that_is_also_bound_is_refused() {
    let mut state = install(vec![("both", Value::Number(1.0))], Heap::default());
    state.expired_functions.insert("both".to_string());
    let parts = complete(&state);
    let error = State::from_durable_parts(
        &parts.header,
        bodies(&parts)
            .iter()
            .map(|(name, body)| (name.as_str(), body.as_slice())),
    )
    .expect_err("a live binding cannot also be a dropped function");
    assert!(
        matches!(&error, SnapshotDecodeError::InvalidEncoding(reason) if reason.contains("`both`")),
        "{error:?}"
    );
}

/// A v8 capture predates the dropped-function names: it is refused by its
/// version before anything is restored, not read as a session that never
/// dropped one.
#[test]
fn a_v8_header_is_refused_by_its_version() {
    let v8_header = [0x81, 0xa7, b'v', b'e', b'r', b's', b'i', b'o', b'n', 0x08];
    let error = State::from_durable_parts(&v8_header, std::iter::empty())
        .expect_err("a v8 header is not a v9 one");
    assert_eq!(
        error,
        SnapshotDecodeError::VersionMismatch {
            expected: LASHLANG_SNAPSHOT_VERSION,
            found: 8,
        }
    );
}
