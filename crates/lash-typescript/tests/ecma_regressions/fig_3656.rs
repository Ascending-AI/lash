//! FIG-3656: built-in objects are first-class values — member semantics, prototypes, expandos and lastIndex reads.

use super::*;
use lashlang::{DurableBaseline, DurableFragment, Snapshot, Vm, VmRunOutcome};
use std::collections::BTreeSet;

/// A built-in object is a first-class value: a missing member reads
/// `undefined`, a write lands an expando, a read-only constant write throws
/// Node's TypeError, and `Ctor.prototype` reads the prototype object
/// (FIG-3656).
#[test]
fn builtin_member_semantics_match_node() {
    assert_eq!(
        finished("finish(Math.extra);"),
        Value::Undefined,
        "Math.extra reads undefined, as Node answers"
    );
    assert_eq!(
        finished("Math.extra = 1; finish(Math.extra);"),
        Value::Number(1.0),
        "an expando on a built-in lands and reads back"
    );
    assert_eq!(
        finished(
            "try { Math.PI = 2; finish('no throw'); } catch (e) { finish([e instanceof TypeError, e.message]); }"
        ),
        Value::List(
            vec![
                Value::Bool(true),
                Value::String("Cannot assign to read only property 'PI' of object 'Math'".into()),
            ]
            .into()
        )
    );
    for (source, expected) in [
        ("finish(typeof String.prototype);", "object"),
        ("finish(typeof String.prototype.trim);", "function"),
        ("finish(typeof Number.prototype);", "object"),
    ] {
        assert_eq!(finished(source), Value::String(expected.into()));
    }
    assert_eq!(
        finished("finish(String.prototype === String.prototype);"),
        Value::Bool(true),
        "a built-in's prototype has one canonical object"
    );
    assert_eq!(
        finished("finish(Math.prototype);"),
        Value::Undefined,
        "Math has no `prototype` own property, as Node answers"
    );
}

/// `lastIndex` stores the raw property and coerces only when used.
#[test]
fn last_index_reads_back_the_written_value() {
    for (source, expected) in [
        (
            "const r = /a/g; r.lastIndex = -1; finish(r.lastIndex);",
            -1.0,
        ),
        (
            "const r = /a/g; r.lastIndex = 2.7; finish(r.lastIndex);",
            2.7,
        ),
        (
            "const r = /a/g; r.lastIndex = Infinity; finish(r.lastIndex);",
            f64::INFINITY,
        ),
    ] {
        assert_eq!(finished(source), Value::Number(expected), "{source}");
    }
    let Value::Number(value) = finished("const r = /a/g; r.lastIndex = NaN; finish(r.lastIndex);")
    else {
        panic!("lastIndex reads back the written NaN");
    };
    assert!(value.is_nan());
}

const RAW_LAST_INDICES: [&str; 7] = ["-1", "2.7", "NaN", "Infinity", "-0", "'2'", "1e16"];
const OBSERVE_LAST_INDEX: &str =
    "finish([typeof r.lastIndex, String(r.lastIndex), String(1 / r.lastIndex)].join('|'));";

fn run_cell(source: &str, state: &mut State) -> Value {
    let globals = state
        .binding_names()
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    let program = lash_typescript::parse_with_globals(source, &globals).expect("parse cell");
    let program = lashlang::testing::harness::try_compile_program(&program).expect("compile cell");
    match futures::executor::block_on(lashlang::execute(&program, state, &Host))
        .expect("execute cell")
    {
        ExecutionOutcome::Finished(value) => value,
        other => panic!("expected finish, got {other:?}"),
    }
}

fn snapshot_reload(state: &State) -> State {
    let bytes = state
        .snapshot()
        .to_canonical_bytes()
        .expect("encode snapshot");
    let snapshot = Snapshot::from_canonical_bytes(&bytes).expect("every emitted snapshot decodes");
    assert_eq!(
        snapshot.to_canonical_bytes().expect("re-encode snapshot"),
        bytes
    );
    State::from_snapshot(snapshot)
}

fn fragment_reload(state: &State) -> State {
    let parts = state
        .durable_parts(
            &DurableBaseline::default(),
            lash_core_execution::FleetFormat::current(),
        )
        .expect("capture fragments");
    let bodies = parts.fragments.iter().map(|(name, fragment)| {
        let DurableFragment::Changed(body) = fragment else {
            panic!("full capture has bodies")
        };
        (name.as_str(), body.as_slice())
    });
    State::from_durable_parts(
        &parts.header,
        bodies,
        lash_core_execution::FleetFormat::current(),
    )
    .expect("every emitted fragment decodes")
    .0
}

fn state_round_trip(reload: fn(&State) -> State) {
    for raw in RAW_LAST_INDICES {
        let mut state = State::new();
        run_cell(
            &format!("const r = /a/g; r.lastIndex = {raw}; finish(0);"),
            &mut state,
        );
        let expected = finished(&format!(
            "const raw = {raw}; finish([typeof raw, String(raw), String(1 / raw)].join('|'));"
        ));
        assert_eq!(
            run_cell(OBSERVE_LAST_INDEX, &mut state),
            expected,
            "raw assignment = {raw}"
        );
        let mut restored = reload(&state);
        assert_eq!(
            run_cell(OBSERVE_LAST_INDEX, &mut restored),
            expected,
            "lastIndex = {raw}"
        );
    }
}

#[test]
fn last_index_raw_snapshot_round_trip() {
    state_round_trip(snapshot_reload);
}

#[test]
fn last_index_raw_fragment_round_trip() {
    state_round_trip(fragment_reload);
}

fn continuation_run(source: &str, restore: bool) -> Value {
    futures::executor::block_on(async {
        let program = lash_typescript::testing::compile(source).expect("compile process");
        let mut state = State::new();
        let mut vm = Vm::from_state(&program, &mut state, &Host).expect("install VM");
        assert_eq!(
            vm.run_process_until_effect().await.expect("run to print"),
            VmRunOutcome::EffectCompleted
        );
        if restore {
            let continuation = vm.suspend().expect("capture raw lastIndex");
            let bytes = serde_json::to_vec(&continuation).expect("encode continuation");
            let restored =
                serde_json::from_slice(&bytes).expect("every emitted continuation decodes");
            vm = Vm::resume_from(restored, &program, &Host).expect("resume VM");
        }
        loop {
            match vm.run_process_until_effect().await.expect("finish process") {
                VmRunOutcome::EffectCompleted => {}
                VmRunOutcome::Complete(ExecutionOutcome::Finished(value)) => break value,
                other => panic!("expected finish, got {other:?}"),
            }
        }
    })
}

#[test]
fn last_index_raw_continuation_round_trip() {
    for raw in RAW_LAST_INDICES {
        let source = format!("const r = /a/g; r.lastIndex = {raw}; print(0); {OBSERVE_LAST_INDEX}");
        let expected = finished(&format!(
            "const raw = {raw}; finish([typeof raw, String(raw), String(1 / raw)].join('|'));"
        ));
        assert_eq!(
            continuation_run(&source, false),
            expected,
            "raw assignment = {raw}"
        );
        assert_eq!(
            continuation_run(&source, true),
            expected,
            "lastIndex = {raw}"
        );
    }
}

#[test]
fn last_index_coercion_and_updates_agree_across_all_wires() {
    for raw in RAW_LAST_INDICES.into_iter().chain(["[2]"]) {
        for flags in ["", "g", "y", "gy"] {
            for operation in [
                "r.exec('baaa')",
                "'baaa'.match(r)",
                "'baaa'.search(r)",
                "'baaa'.replace(r, 'x')",
                "'baaa'.split(r)",
            ] {
                let setup = format!("const r = /a/{flags}; r.lastIndex = {raw};");
                let observe = format!(
                    "const result = {operation}; finish(JSON.stringify([result, typeof r.lastIndex, String(r.lastIndex), String(1 / r.lastIndex)]));"
                );
                let process = format!("{setup} print(0); {observe}");
                let expected = continuation_run(&process, false);
                assert_eq!(continuation_run(&process, true), expected, "{process}");
                for reload in [snapshot_reload, fragment_reload] {
                    let mut state = State::new();
                    run_cell(&format!("{setup} finish(0);"), &mut state);
                    let mut restored = reload(&state);
                    assert_eq!(run_cell(&observe, &mut restored), expected, "{process}");
                }
            }
        }
    }
    // A continuation retains the program that owns the guest coercion hook.
    let source = "const r = /a/y; r.lastIndex = { valueOf() { return 2; } }; print(0); const result = r.exec('baa'); finish([r.lastIndex, result[0]].join('|'));";
    assert_eq!(
        continuation_run(source, true),
        continuation_run(source, false)
    );
}

#[test]
fn last_index_references_survive_collection_and_incremental_capture() {
    let mut state = State::new();
    run_cell("const r = /a/g; r.lastIndex = [1]; finish(0);", &mut state);
    let fleet = lash_core_execution::FleetFormat::current();
    let first = state
        .durable_parts(&DurableBaseline::default(), fleet)
        .expect("initial capture");
    run_cell("r.lastIndex[0] = 2; finish(0);", &mut state);
    let delta = state
        .durable_parts(&first.baseline, fleet)
        .expect("capture child mutation");
    assert!(
        matches!(delta.fragments["r"], DurableFragment::Changed(_)),
        "the property's child mutation invalidates its fragment"
    );
    assert_ne!(first.fragments["r"], delta.fragments["r"]);
    let mut persisted = first.fragments;
    for (name, fragment) in &delta.fragments {
        if let DurableFragment::Changed(_) = fragment {
            persisted.insert(name.clone(), fragment.clone());
        }
    }
    let bodies = persisted.iter().map(|(name, fragment)| {
        let DurableFragment::Changed(body) = fragment else {
            panic!("stored fragment body")
        };
        (name.as_str(), body.as_slice())
    });
    let (mut restored, baseline) = State::from_durable_parts(&delta.header, bodies, fleet)
        .expect("reload the incremental capture");
    assert!(matches!(
        restored
            .durable_parts(&baseline, fleet)
            .expect("unchanged capture")
            .fragments["r"],
        DurableFragment::Unchanged
    ));
    assert_eq!(
        run_cell("finish(r.lastIndex[0]);", &mut restored),
        Value::Number(2.0)
    );
    let mut restored = snapshot_reload(&state);
    assert_eq!(
        run_cell("finish(r.lastIndex[0]);", &mut restored),
        Value::Number(2.0)
    );
    let source = "const r = /a/y; r.lastIndex = [2]; print(0); const garbage = [9]; const result = r.exec('baa'); finish([r.lastIndex, result[0]].join('|'));";
    assert_eq!(continuation_run(source, true), Value::String("3|a".into()));
}
