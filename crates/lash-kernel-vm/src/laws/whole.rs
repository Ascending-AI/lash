//! The whole binary state (FIG-5887 §3, §4, §11): PC-EXACT and PC-WHOLE at
//! the machine's boundary, the kernel half of PC-ADMISSION, and the
//! terminal object table.

use std::collections::BTreeMap;

use lash_kernel_doc::graph_tokens::{ChildRef, decode_body, encode_body};
use lash_kernel_doc::{
    Datum, ErrorDatum, FunctionId, FunctionName, Integer, Name, Object, ObjectId, TaskIdentity,
    Value,
};
use lash_kernel_state::{
    AdmittedRequest, KernelWhole, Pause, PerformState, Request as SavedRequest, TaskState,
    TerminalWhole, WaitId, WholeError,
};

use super::embedder::{Embedder, FrozenStore, Setup};
use super::parked::{
    CALLBACK_IN_MAP, CANCELLED_MID_CLEANUP, CLEANUP_WITH_A_WAIT, EXPRESSIONS,
    FAN_OUT_WITH_ONE_ENDED, HELPER_FROM_TWO_PLACES, Observed, TABLE_CHURN, deliver_next, observe,
};
use crate::{
    Bindings, Bound, BoundExceeded, End, Finished, Machine, RunError, Start, Step, Target,
    TerminalError, WholeImportError, terminal_end, terminal_whole,
};

/// Aliases, a cycle through a list, a text past the inline bound, an
/// integer past 64 bits and the floats only bits tell apart, all held
/// across waits whose outcomes repeat the large text.
const ALIASES_CYCLES_AND_NUMBERS: &str = r#"
main {
  let big = "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"
  let xs = [1, big]
  set xs[list.len(xs)] = xs
  let ys = xs
  let huge = 123456789012345678901234567890
  let t = (big, huge, -0.0, 0.0, nan, -inf)
  let got = perform echo(t) as Any
  set ys[0] = 99
  let back = xs[2]
  let again = perform echo(big) as Any
  do sleep 1
  return (back[0], list.len(back), got, again, huge)
}
"#;

/// [`observe`], with the run moved at every park, before and after its
/// outcome is delivered, to a machine restored cold from its whole binary
/// state.
fn observe_whole(text: &str) -> (Observed, usize) {
    let mut embedder = Embedder::with(text, Setup::default());
    let mut frozen = FrozenStore::default();
    let mut hops = 0_u64;
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => {}
            Step::Parked(_) => {
                hops += 1;
                embedder.relay_whole(hops.is_multiple_of(2), &mut frozen);
                deliver_next(&mut embedder);
                hops += 1;
                embedder.relay_whole(hops.is_multiple_of(2), &mut frozen);
            }
        }
    };
    (
        (
            end,
            embedder.world.printed,
            embedder.requests,
            embedder.machine.meters().charged,
        ),
        frozen.len(),
    )
}

/// PC-EXACT and PC-WHOLE: a run restored cold from its whole binary state
/// at every park, before and after each outcome, runs as the run that never
/// parked: the same values, prints, effect identities and charge. Each hop
/// keeps every meter and re-exports the whole it resumed from, with no
/// earlier state to stand on. Aliases stay shared, a cycle stays a cycle,
/// holes, insertion order, cleanup blocks, native-held arguments, joins,
/// withdrawals and sleeps survive, and an integer past 64 bits and the
/// floats only their bits tell apart read back exactly.
#[test]
fn a_run_restored_cold_from_its_whole_binary_state_at_every_park_runs_the_same() {
    let programs = [
        ("expression-heavy code", EXPRESSIONS),
        (
            "an effectful helper called from two places",
            HELPER_FROM_TWO_PLACES,
        ),
        ("a callback inside map", CALLBACK_IN_MAP),
        ("a cleanup block with a wait", CLEANUP_WITH_A_WAIT),
        ("a fan-out with one member ended", FAN_OUT_WITH_ONE_ENDED),
        ("a cancelled task mid-cleanup", CANCELLED_MID_CLEANUP),
        ("live map and set iteration through removals", TABLE_CHURN),
        (
            "aliases, a cycle, large text and exact numbers",
            ALIASES_CYCLES_AND_NUMBERS,
        ),
    ];
    for (name, text) in programs {
        let straight = observe(text, u64::MAX, false);
        assert!(
            matches!(straight.0, End::Finished(_)),
            "{name}: {:?}",
            straight.0
        );
        let (restored, _) = observe_whole(text);
        assert_eq!(restored, straight, "{name}");
    }
    let (restored, frozen) = observe_whole(ALIASES_CYCLES_AND_NUMBERS);
    let End::Finished(finished) = restored.0 else {
        panic!("the run finishes");
    };
    let Datum::Tuple(result) = finished.result else {
        panic!("a tuple");
    };
    assert_eq!(
        result[0],
        Datum::Int(Integer::from(99)),
        "the alias is shared"
    );
    assert_eq!(result[1], Datum::Int(Integer::from(3)), "the cycle holds");
    // Two committed outcomes and nothing else became frozen values.
    assert_eq!(frozen, 2);
}

/// A run parked at its first wait, with that wait's argument.
fn first_park(text: &str) -> Embedder {
    let mut embedder = Embedder::with(text, Setup::default());
    assert!(matches!(embedder.run(), Step::Parked(_)));
    embedder
}

const ONE_WAIT: &str = r#"
main {
  let got = perform echo("the-request-body-is-frozen-once-in-the-admission") as Any
  return got
}
"#;

/// PC-ADMISSION, kernel half: a whole state holds a wait as its identity,
/// never its arguments. It resumes only when the embedder's admission under
/// the execution holds a request of the wait's kind for that identity, and
/// a wait not yet handed out at a park is never published.
#[test]
fn a_whole_resumes_only_from_the_admission_of_each_wait() {
    let mut embedder = first_park(ONE_WAIT);
    let mut frozen = FrozenStore::default();
    let whole = embedder
        .machine
        .export_whole(ChildRef([1; 32]), ChildRef([2; 32]), |datum| {
            frozen.freeze(datum)
        })
        .unwrap();
    let bytes = encode_body(&whole);
    let marker = b"the-request-body-is-frozen-once-in-the-admission";
    assert!(
        !bytes.windows(marker.len()).any(|window| window == marker),
        "the whole holds no request body"
    );
    let identity = whole.waits().next().unwrap().identity.clone();
    let admissions = embedder.admissions();
    let resume = |admitted: &dyn Fn() -> Option<AdmittedRequest>| {
        let (program, _) = embedder.program(true);
        crate::KernelMachine::import_whole(
            program,
            super::embedder::ROOMY,
            whole.clone(),
            |child| frozen.thaw(child),
            |_| admitted(),
        )
        .map(|_| ())
    };
    assert_eq!(
        resume(&|| None),
        Err(WholeImportError::Whole(WholeError::AdmissionMissing(
            Box::new(identity.clone())
        )))
    );
    assert_eq!(
        resume(&|| {
            Some(AdmittedRequest::Sleep {
                duration: Pause {
                    seconds: 1,
                    nanoseconds: 0,
                },
            })
        }),
        Err(WholeImportError::Whole(WholeError::AdmissionMismatch(
            Box::new(identity.clone())
        )))
    );
    assert_eq!(resume(&|| admissions.get(&identity).cloned()), Ok(()));

    // A wait requested since the last park has not been handed out: it
    // has no admission, and a state holding one is never published.
    let mut parked = embedder.machine.export().unwrap();
    for task in &mut parked.tasks {
        if let TaskState::Performing(perform) = &mut task.handle.state
            && let SavedRequest::Effect { state, .. } = &mut perform.request
        {
            *state = PerformState::Requested;
        }
    }
    assert_eq!(
        KernelWhole::capture(&parked, ChildRef([1; 32]), ChildRef([2; 32]), |datum| {
            frozen.freeze(datum)
        }),
        Err(WholeError::Unpublished(WaitId(0)))
    );
}

fn freeze_into(
    store: &mut FrozenStore,
) -> impl FnMut(
    &lash_kernel_doc::FrozenDatum,
) -> Result<ChildRef, lash_kernel_doc::graph_tokens::TokenError>
+ '_ {
    |datum| store.freeze(datum)
}

/// FIG-5887 §4: an end keeps every detail in one object table. A finished
/// cell's carried and closure bindings share an object once, the result is
/// a frozen datum apart from every binding, and every other end reads back
/// as the machine reported it.
#[test]
fn an_end_writes_one_object_table_and_reads_back_exactly() {
    let mut objects = BTreeMap::new();
    objects.insert(
        ObjectId(7),
        Object::List(vec![Value::Int(Integer::from(1))]),
    );
    let mut variables = BTreeMap::new();
    variables.insert(Name::new("xs"), Value::List(ObjectId(7)));
    let mut embedder = Embedder::with(
        r#"
main {
  let alias = xs
  let callback = fn() { return alias }
  let nested = {keep: alias, call: callback}
  return list.len(xs)
}"#,
        Setup {
            start: Start {
                target: Target::Main,
                args: Vec::new(),
                bindings: Bindings { variables, objects },
            },
            ..Setup::default()
        },
    );
    let Step::Ended(end) = embedder.run() else {
        panic!("the cell finishes");
    };
    let End::Finished(finished) = &end else {
        panic!("finished");
    };
    let mut store = FrozenStore::default();
    let whole = terminal_whole(&end, freeze_into(&mut store))
        .unwrap()
        .expect("a finished run records its end");
    let shared = finished.bindings.variables[&Name::new("xs")]
        .object()
        .unwrap();
    assert!(finished.closures.objects.contains_key(&shared));
    assert!(finished.bindings.objects.contains_key(&shared));
    assert_eq!(
        whole.objects.len(),
        finished
            .bindings
            .objects
            .keys()
            .chain(finished.closures.objects.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        "one table, each object once"
    );
    let read: TerminalWhole = decode_body(&encode_body(&whole)).unwrap();
    assert_eq!(read, whole);
    assert_eq!(terminal_end(read, |child| store.thaw(child)).unwrap(), end);

    // Two bodies under one identity are refused, not merged.
    let mut conflicting = finished.clone();
    conflicting
        .closures
        .objects
        .insert(shared, Object::List(Vec::new()));
    assert_eq!(
        terminal_whole(&End::Finished(conflicting), freeze_into(&mut store)),
        Err(TerminalError::ObjectConflict(shared))
    );

    let error = Datum::Error(Box::new(ErrorDatum {
        kind: "boom".into(),
        message: "m".into(),
        data: Datum::Null,
    }));
    let main = TaskIdentity::Main;
    for end in [
        End::Failed(Datum::Text("reason".into())),
        End::Error(RunError::Uncaught(error)),
        End::Error(RunError::TasksOutstanding {
            unfinished: vec![main.clone()],
            unobserved: Vec::new(),
        }),
        End::Error(RunError::Deadlock {
            waiting: vec![main],
        }),
        End::Error(RunError::Bound(BoundExceeded {
            bound: Bound::Guard {
                function: FunctionId::from_bytes([3; 32]),
            },
            limit: 7,
            function: Some(FunctionName::new("regex.exec").unwrap()),
        })),
        End::Error(RunError::Bound(BoundExceeded {
            bound: Bound::Memory,
            limit: 1,
            function: None,
        })),
        End::Finished(Finished {
            result: Datum::Null,
            finish: true,
            bindings: Bindings::default(),
            not_carried: Vec::new(),
            closures: Bindings::default(),
        }),
    ] {
        let whole = terminal_whole(&end, freeze_into(&mut store))
            .unwrap()
            .unwrap();
        let read: TerminalWhole = decode_body(&encode_body(&whole)).unwrap();
        assert_eq!(terminal_end(read, |child| store.thaw(child)).unwrap(), end);
    }
    assert_eq!(
        terminal_whole(&End::Cancelled, freeze_into(&mut store)),
        Ok(None)
    );
}

/// WHOLE-PHASE-WORK: capture and cold import have separate raw receipts;
/// a relay reports the complete objects/tasks/calls that actually cross it.
#[test]
fn whole_capture_and_import_report_each_real_phase_and_structure() {
    let mut embedder = Embedder::with(ALIASES_CYCLES_AND_NUMBERS, Setup::default());
    assert!(matches!(embedder.run(), Step::Parked(_)));
    let collector = crate::perf_witness::Collector::install().unwrap();
    assert!(crate::perf_witness::Collector::install().is_none());
    embedder.relay_whole(false, &mut FrozenStore::default());
    let receipts = collector.snapshot();
    assert_eq!(
        receipts.iter().map(|r| r.phase).collect::<Vec<_>>(),
        [
            "machine.export",
            "whole.capture",
            "whole.restore",
            "machine.import",
            "machine.export",
            "whole.capture"
        ]
    );
    let expected = receipts[0].geometry.unwrap();
    assert!(expected.objects > 0 && expected.tasks > 0 && expected.active_calls > 0);
    for receipt in &receipts {
        assert_eq!(receipt.geometry, Some(expected));
        eprintln!(
            "KERNEL_WHOLE {}",
            serde_json::json!({"phase": receipt.phase,
            "elapsed_nanos": receipt.elapsed_nanos, "tasks": expected.tasks,
            "active_calls": expected.active_calls, "objects": expected.objects,
            "scope": "actual synchronous phase call; structure cardinalities, not bytes or charge"})
        );
    }
}
