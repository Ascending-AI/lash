use std::path::Path;

use lashlang::{ExecutionEnvironment, Snapshot, State, Value, Vm, VmContinuation, VmRunOutcome};
use serde::Deserialize;

use super::baseline::{Host, compile_cell, run_cell};

pub struct Case {
    pub id: String,
    pub source: String,
    pub snapshot: Snapshot,
    pub continuation: VmContinuation,
    pub program: lashlang::CompiledProgram,
}

fn capture(id: String, source: &str, state: &State, snapshot: Snapshot, host: &Host) -> Case {
    let program = compile_cell(source, state);
    let environment = ExecutionEnvironment::new(host);
    let mut input = state.clone();
    let mut vm = Vm::from_state(&program, &mut input, &environment).expect("corpus VM");
    assert_eq!(
        futures::executor::block_on(vm.run_process_until_effect()).expect("corpus checkpoint"),
        VmRunOutcome::EffectCompleted
    );
    let continuation = vm.suspend().expect("corpus continuation");
    let bytes = serde_json::to_vec(&continuation).expect("continuation encode");
    let decoder = lashlang::VmInstance::pristine();
    let restored = decoder
        .open_continuation(&bytes)
        .expect("continuation decode");
    assert_eq!(
        serde_json::to_vec(&restored).expect("restored encode"),
        bytes
    );
    let mut resumed =
        Vm::resume_from(restored, &program, &environment).expect("continuation resume");
    let outcome =
        futures::executor::block_on(resumed.run_for_mode()).expect("resumed program completes");
    let expected = run_cell(source, &mut state.clone(), host);
    assert_eq!(outcome, expected);
    let snapshot_bytes = snapshot.to_canonical_bytes().expect("snapshot encode");
    assert_eq!(
        decoder
            .open_snapshot(&snapshot_bytes)
            .expect("snapshot decode")
            .to_canonical_bytes()
            .expect("snapshot reencode"),
        snapshot_bytes
    );
    Case {
        id,
        source: source.to_string(),
        snapshot,
        continuation,
        program,
    }
}

#[derive(Deserialize)]
struct Session {
    id: String,
    cells: Vec<Cell>,
}
#[derive(Deserialize)]
struct Cell {
    source: String,
}

pub fn load(root: &Path) -> Vec<Case> {
    let host = Host::default();
    let mut cases = Vec::new();
    // Fixed accepted rows cover scalar coercion, nested arrays and callbacks.
    for (shard, index) in [
        ("sol", 1),
        ("sol", 44),
        ("sol", 116),
        ("FIG-3714", 7),
        ("FIG-3714", 10),
        ("FIG-3787", 35),
    ] {
        let table = std::fs::read_to_string(root.join(format!(
            "crates/lash-typescript/tests/differential/expectations/{shard}.tsv"
        )))
        .expect("conformance table");
        let fields: Vec<&str> = table
            .lines()
            .find(|line| line.split('\t').nth(1) == Some(&index.to_string()))
            .expect("conformance row")
            .split('\t')
            .collect();
        assert_eq!(fields[2], "accept");
        let expression: String = serde_json::from_str(fields[3]).expect("expression");
        let source = format!("const sample = {expression}; console.log(sample); finish(sample);");
        let mut state = State::new();
        let _ = run_cell(&source, &mut state, &host);
        cases.push(capture(
            format!("conformance-{shard}-{index}"),
            &source,
            &State::new(),
            state.snapshot(),
            &host,
        ));
    }
    for name in [
        "mutation-through-session-globals",
        "exotic-globals",
        "spread-built-object-fields",
    ] {
        let path = root.join(format!(
            "crates/lash-typescript/tests/differential/sessions/expectations/{name}.json"
        ));
        let session: Session =
            serde_json::from_str(&std::fs::read_to_string(path).expect("session file"))
                .expect("session JSON");
        let mut state = State::new();
        for (index, cell) in session.cells.iter().enumerate() {
            let entry = state.clone();
            let _ = run_cell(&cell.source, &mut state, &host);
            // Pure cells need a completed effect to expose the public checkpoint API.
            let checkpoint_source = format!("{}\nconsole.log('__checkpoint');", cell.source);
            cases.push(capture(
                format!("session-{}-{index}", session.id),
                &checkpoint_source,
                &entry,
                state.snapshot(),
                &host,
            ));
        }
    }
    for size in [8192, 1024 * 1024] {
        // Seed state directly: the current front end caps source at 64 KiB.
        let source = "console.log(payload.length);";
        let mut state = State::new();
        state
            .insert_global("payload", Value::String("x".repeat(size).into()))
            .expect("large binding");
        let _ = run_cell(source, &mut state, &host);
        cases.push(capture(
            format!("stress-{size}"),
            source,
            &state,
            state.snapshot(),
            &host,
        ));
    }
    let shell = "const payload = ''; console.log(payload.length);";
    let source = format!(
        "const payload = '{}'; console.log(payload.length);",
        "x".repeat(65536 - shell.len())
    );
    assert_eq!(source.len(), 65536);
    let mut state = State::new();
    let _ = run_cell(&source, &mut state, &host);
    cases.push(capture(
        "stress-source-limit".into(),
        &source,
        &State::new(),
        state.snapshot(),
        &host,
    ));
    let source = format!(
        "const payload = [{}]; console.log(payload.length);",
        vec!["0"; 10000].join(",")
    );
    let mut state = State::new();
    let _ = run_cell(&source, &mut state, &host);
    cases.push(capture(
        "stress-10000-nodes".into(),
        &source,
        &State::new(),
        state.snapshot(),
        &host,
    ));
    cases
}
