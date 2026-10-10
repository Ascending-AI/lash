//! Laws of the stored form: what a save writes, and what a load refuses.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{
    Document, KERNEL_VERSION, Name, NumberPolicy, Object, ObjectId, Site, TaskId, TaskIdentity,
    Unit, Value,
};

use crate::{
    Baseline, Call, Fragment, Held, LoadError, ParkedCall, ParkedRun, ParkedTask, Root, Run,
    SavedFragment, Task, TaskState,
};

/// A session cell parked at its first statement. `a` and `b` name one
/// list, which holds a second; `c` names a list of its own.
fn parked() -> ParkedRun {
    let document = Document::new(NumberPolicy::Float, Vec::new());
    let list = |id| Value::List(ObjectId(id));
    ParkedRun {
        run: Run {
            kernel: KERNEL_VERSION,
            document: document.identity().unwrap(),
            functions: BTreeSet::new(),
            charged: 3,
            objects_allocated: 9,
            waits_issued: 0,
            ready: vec![TaskId::MAIN].try_into().unwrap(),
            withdrawn: BTreeSet::new(),
            unreported: Vec::new(),
        },
        session: BTreeMap::from([
            (Name::new("a"), list(4)),
            (Name::new("b"), list(4)),
            (Name::new("c"), list(7)),
        ]),
        tasks: vec![ParkedTask {
            handle: Task {
                identity: TaskIdentity::Main,
                state: TaskState::Ready,
                joiners: Vec::new(),
                observed: false,
                passed: false,
                failed: false,
                occurrences: Vec::new(),
            },
            calls: vec![ParkedCall {
                call: Call {
                    statement: Site::new(Unit::Main, [0]),
                    bindings: Vec::new(),
                    loops: Vec::new(),
                    finally: Vec::new(),
                },
                held: Held::default(),
            }],
        }],
        objects: BTreeMap::from([
            (ObjectId(4), Object::List(vec![list(5)])),
            (ObjectId(5), Object::List(vec![Value::Bool(true)])),
            (ObjectId(7), Object::List(vec![list(5)])),
        ]),
    }
}

fn session(name: &str) -> Root {
    Root::Session(Name::new(name))
}

fn parts(run: &ParkedRun) -> (Vec<u8>, BTreeMap<Root, Vec<u8>>) {
    let saved = run.save(&Baseline::default()).unwrap();
    let fragments = saved
        .changed()
        .map(|(root, bytes)| (root.clone(), bytes.to_vec()))
        .collect();
    (saved.header, fragments)
}

fn load(header: &[u8], fragments: &BTreeMap<Root, Vec<u8>>) -> Result<ParkedRun, LoadError> {
    ParkedRun::load(
        header,
        fragments
            .iter()
            .map(|(root, bytes)| (root, bytes.as_slice())),
    )
    .map(|(run, _)| run)
}

fn owned(fragments: &BTreeMap<Root, Vec<u8>>, root: &Root) -> Vec<u64> {
    let fragment: Fragment = serde_json::from_slice(&fragments[root]).unwrap();
    fragment.objects.iter().map(|owned| owned.id.0).collect()
}

/// Each live object is written once, by the first root in the header's
/// order that reaches it, and the parts read back as the run they were
/// written from.
#[test]
fn an_object_is_owned_by_the_first_root_that_reaches_it() {
    let run = parked();
    let (header, fragments) = parts(&run);
    assert_eq!(fragments.len(), 5);
    assert_eq!(owned(&fragments, &session("a")), [4, 5]);
    assert_eq!(owned(&fragments, &session("b")), [0u64; 0]);
    assert_eq!(owned(&fragments, &session("c")), [7]);
    assert_eq!(load(&header, &fragments).unwrap(), run);
    // The whole document holds exactly what the parts do.
    let whole = serde_json::to_string(&run).unwrap();
    assert_eq!(serde_json::from_str::<ParkedRun>(&whole).unwrap(), run);
}

/// A save against the baseline of the last one names exactly the
/// fragments whose content changed, and the roots that are gone.
#[test]
fn a_save_names_exactly_what_changed_since_its_baseline() {
    let mut run = parked();
    let first = run.save(&Baseline::default()).unwrap();
    let again = run.save(&first.baseline).unwrap();
    assert_eq!(again.changed().count(), 0);
    assert!(again.removed.is_empty());

    run.session.remove(&Name::new("a"));
    let second = run.save(&first.baseline).unwrap();
    // `b` now owns what `a` did; `c` and the task are as they were.
    let changed: Vec<&Root> = second.changed().map(|(root, _)| root).collect();
    assert_eq!(changed, [&session("b")]);
    assert_eq!(second.removed, [session("a")]);
    assert_eq!(second.fragments[&session("c")], SavedFragment::Unchanged);
}

/// A load takes only the bytes this writer produces: a missing fragment,
/// an object in the fragment of a root that does not own it, an object no
/// root reaches and a state of another kernel version are each refused.
#[test]
fn a_load_refuses_parts_this_writer_would_not_have_written() {
    let (header, fragments) = parts(&parked());

    let mut missing = fragments.clone();
    missing.remove(&session("c"));
    assert_eq!(
        load(&header, &missing),
        Err(LoadError::Missing { root: session("c") })
    );

    // Object 5 moved from `a`, which reaches it first, to `c`.
    let moved = |fragments: &BTreeMap<Root, Vec<u8>>, from: &str, to: &str| {
        let mut moved = fragments.clone();
        let mut giver: Fragment = serde_json::from_slice(&fragments[&session(from)]).unwrap();
        let mut taker: Fragment = serde_json::from_slice(&fragments[&session(to)]).unwrap();
        taker.objects.insert(0, giver.objects.remove(1));
        moved.insert(session(from), serde_json::to_vec(&giver).unwrap());
        moved.insert(session(to), serde_json::to_vec(&taker).unwrap());
        moved
    };
    assert!(matches!(
        load(&header, &moved(&fragments, "a", "c")),
        Err(LoadError::NotCanonical { .. })
    ));

    let mut unreached = fragments.clone();
    let mut fragment: Fragment = serde_json::from_slice(&fragments[&session("c")]).unwrap();
    let mut extra = fragment.objects[0].clone();
    extra.id = ObjectId(8);
    fragment.objects.push(extra);
    unreached.insert(session("c"), serde_json::to_vec(&fragment).unwrap());
    assert!(matches!(
        load(&header, &unreached),
        Err(LoadError::NotCanonical { .. })
    ));

    let mut next: serde_json::Value = serde_json::from_slice(&header).unwrap();
    next["run"]["kernel"] = (KERNEL_VERSION + 1).into();
    next["run"]["a_field_of_the_next_version"] = true.into();
    assert_eq!(
        load(&serde_json::to_vec(&next).unwrap(), &fragments),
        Err(LoadError::KernelVersion {
            parked: KERNEL_VERSION + 1,
            supported: KERNEL_VERSION,
        })
    );
}

/// V22: an effect cannot elapse, and a sleep cannot complete or fail.
#[test]
fn a_stored_wait_admits_only_outcomes_of_its_request_kind() {
    let identity = serde_json::json!({"task": "main", "site": {"unit": "main", "path": [0, 0]}, "occurrence": 0, "loops": []});
    let effect =
        serde_json::json!({"effect": {"identity": identity, "effect": "echo", "args": []}});
    let sleep = serde_json::json!({"sleep": {"identity": identity, "duration": {"seconds": 0, "nanoseconds": 0}}});
    let states = [
        serde_json::json!("requested"),
        serde_json::json!("admitted"),
        serde_json::json!({"committed": {"completed": "null"}}),
        serde_json::json!({"committed": {"failed": {"kind": "bug", "message": "bug", "data": "null"}}}),
        serde_json::json!({"committed": "elapsed"}),
    ];
    for (kind, request) in [("effect", effect), ("sleep", sleep)] {
        for (index, state) in states.iter().enumerate() {
            let mut request = request.clone();
            request[kind]["state"] = state.clone();
            let encoded = serde_json::json!({"wait": 0, "request": request});
            let valid =
                index < 2 || (kind == "effect" && index < 4) || (kind == "sleep" && index == 4);
            assert_eq!(
                serde_json::from_value::<crate::Perform>(encoded).is_ok(),
                valid,
                "{kind}: {state}"
            );
        }
    }
}
