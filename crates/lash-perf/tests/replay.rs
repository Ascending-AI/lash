//! REPLAY-GATE: the per-merge counted replay gate (FIG-5934). The fast
//! subset of the standard replay corpus runs end to end through the facade
//! on SQLite file stores, and its counted per-turn costs and depth slopes
//! stay within `workloads/replay/budgets.json` and the blessed baseline.
//! Wall time is never asserted. A breach prints the budget and its owner;
//! `just perf-replay-bless` re-blesses an intended baseline move.
#![cfg(not(feature = "dhat-heap"))]

use std::process::Command;

#[test]
fn the_replay_corpus_subset_stays_within_its_counted_budgets() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("replay-receipt.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay-gate")
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("{stdout}");
    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        output.status.success(),
        "the replay gate failed: {}",
        stdout
            .lines()
            .filter(|line| line.starts_with("replay gate: VIOLATION"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(receipt.exists());
}

/// FIG-6750 / FIG-6681: a retained megabyte scalar cannot starve cleanup
/// of retired reply families. Twenty-four turns distinguish sustained
/// history growth from the prompt prefix's bounded initial tree growth.
#[test]
fn a_large_retained_scalar_keeps_history_turn_sql_writes_flat() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("receipt.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .args(["replay", "py-codemode", "--turns", "24"])
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    let summary = &receipt["workloads"][0]["summary"];
    for (metric, ceiling) in [("sql_rows_written", 0.5), ("sql_bytes_written", 512.0)] {
        let slope = summary[metric]["robust_slope_per_history_turn"]
            .as_f64()
            .expect("the repeated turns have a counted history slope");
        eprintln!("retained scalar: {metric} history slope {slope}, ceiling {ceiling}");
        assert!(slope <= ceiling, "{metric} grows with history: {slope}");
    }
}

/// TRAFFIC-CONSERVATION: replay exports complete per-kind transport and
/// physical SQLite attribution from the same cuts as its existing totals.
#[test]
fn traffic_conservation_replay_exports_only_complete_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("receipt.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .args(["replay", "ts-codemode", "--turns", "1", "--traffic"])
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    let round = &value["workloads"][0]["rounds"][0];
    assert_eq!(round["traffic"]["sqlite"]["complete"], true);
    let body: u64 = round["traffic"]["sqlite"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["kind"] == "wal_body")
        .map(|row| row["bytes"].as_u64().unwrap())
        .sum();
    let ipc = &round["traffic"]["ipc"];
    let bins: Vec<_> = ipc["sent"]
        .as_object()
        .unwrap()
        .values()
        .chain(ipc["received"].as_object().unwrap().values())
        .collect();
    let frames: u64 = bins.iter().map(|r| r["frames"].as_u64().unwrap()).sum();
    let bytes: u64 = bins.iter().map(|r| r["bytes"].as_u64().unwrap()).sum();
    assert!(frames > 0);
    assert_eq!(
        frames as f64,
        round["metrics"]["ipc_frames"].as_f64().unwrap()
    );
    assert_eq!(
        bytes as f64,
        round["metrics"]["ipc_bytes"].as_f64().unwrap()
    );
    assert!(body > 0);
    assert_eq!(
        body as f64,
        round["metrics"]["store_bytes_written"].as_f64().unwrap()
    );
}

/// FIG-6467: a fresh code-mode turn must not retain and release its helper
/// declarations a few SQL statements apiece. The pre-heap replay used 570
/// statements; allow half that count again for persistent-heap leases and
/// publication.
#[test]
fn a_first_cell_turn_stays_within_the_sql_statement_budget() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("first-turn.json");
    std::fs::write(
        &script,
        serde_json::to_vec(&serde_json::json!({
            "format": "lash-perf-replay/1",
            "name": "first-cell-turn",
            "description": "Fresh-process host JSON turn SQL budget (FIG-6467)",
            "protocol": "typescript",
            "sessions": 1,
            "turns": [{
                "user": "Read and print a host value",
                "replies": [
                    {"cell": [
                        "const result = await tools.fixtureLookup({key: \"json\"});",
                        "console.log(JSON.stringify(result));"
                    ]},
                    {"text": "done"}
                ]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let receipt = dir.path().join("receipt.json");
    let shapes = dir.path().join("shapes.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay")
        .arg(&script)
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .arg("--sql-shapes")
        .arg(&shapes)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(receipt).unwrap()).unwrap();
    let shapes: serde_json::Value =
        serde_json::from_slice(&std::fs::read(shapes).unwrap()).unwrap();
    eprintln!("SQL attribution: {shapes}");
    let phases = shapes["rounds"].as_array().unwrap();
    let startup = phases
        .iter()
        .find(|phase| phase["phase"] == "node_start")
        .expect("package installation has its own startup receipt");
    let installs = |phase: &serde_json::Value| -> u64 {
        phase["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| {
                row["shape"]
                    .as_str()
                    .unwrap()
                    .starts_with("insert into main.package_definitions ")
            })
            .map(|row| row["statements"].as_u64().unwrap())
            .sum()
    };
    assert!(
        installs(startup) > 0,
        "startup accounts for definition-index statements"
    );
    for phase in phases.iter().filter(|phase| phase["phase"] != "node_start") {
        assert_eq!(installs(phase), 0, "package installation escaped startup");
    }
    let statements = receipt["workloads"][0]["rounds"][0]["metrics"]["sql_statements"]
        .as_f64()
        .unwrap();
    assert!(
        statements <= 855.0,
        "first turn issued {statements} SQL statements, budget is 855 (1.5 × pre-heap 570)"
    );
}

/// FIG-6617: one coding turn on SQLite reads each fenced fact once per
/// scope. A member that captured no wake reads no capture buffer, an owner
/// commit reads its open turn and that turn's cancel in one statement, and an
/// advance takes the phase roots its commit's fenced turn read already saw.
/// The turn (four tool calls, then an answer) issued 54 SELECTs of these
/// shapes before; the bound is the measured count after.
#[test]
fn a_coding_turn_reads_its_fenced_facts_once_per_scope() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("coding-turn.json");
    let reads: Vec<_> = (0..4)
        .map(|index| {
            serde_json::json!({"tool_calls": [{
                "tool": "fixture_large",
                "args": {"key": format!("read-{{turn}}-{index}"), "bytes": 643}
            }]})
        })
        .chain([serde_json::json!({"text": "done"})])
        .collect();
    std::fs::write(
        &script,
        serde_json::to_vec(&serde_json::json!({
            "format": "lash-perf-replay/1",
            "name": "coding-turn-reads",
            "description": "One coding turn's repeated fenced reads (FIG-6617)",
            "protocol": "standard",
            "sessions": 1,
            "turns": [{"user": "Read four files, then answer", "replies": reads}]
        }))
        .unwrap(),
    )
    .unwrap();
    let receipt = dir.path().join("receipt.json");
    let shapes = dir.path().join("shapes.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay")
        .arg(&script)
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .arg("--sql-shapes")
        .arg(&shapes)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shapes: serde_json::Value =
        serde_json::from_slice(&std::fs::read(shapes).unwrap()).unwrap();
    let fixed = [
        "from temp.lash_woken",
        "select request_id, origin, reason, disposition, mode from",
        "select checkpoint_ref, answer_ref from",
    ];
    let turn = shapes["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|round| round["phase"] == "turn" && round["turn"] == 1)
        .expect("SQL shapes retain the measured turn separately from session start");
    let rows = turn["rows"].as_array().unwrap();
    let mut counted = std::collections::BTreeMap::new();
    for row in rows {
        let shape = row["shape"].as_str().unwrap();
        if let Some(fact) = fixed.iter().find(|fact| shape.contains(**fact)) {
            *counted.entry(*fact).or_insert(0) += row["selects"].as_u64().unwrap();
        }
    }
    let selects: u64 = counted.values().sum();
    eprintln!("FIG-6617 fenced-fact SELECTs: {selects} {counted:?}");
    assert!(
        selects <= BOUND,
        "the coding turn issued {selects} SELECTs of its fenced facts {counted:?}, bound {BOUND}"
    );
}

/// The measured count after FIG-6617.
const BOUND: u64 = 2;

/// FIG-6634: one coding turn on SQLite loads its committed window from the
/// store only where nothing it holds is that window. The owner's head and
/// the runtime's open each read the head; preparation and every environment
/// sync then take the prelude's base window from the head the turn already
/// holds instead of reading it again with its state version. The turn (four
/// tool calls, then an answer) issued the head-meta and state-version
/// SELECTs counted here before; the bound is the measured count after.
#[test]
fn a_coding_turn_loads_its_window_only_where_it_holds_none() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("coding-turn.json");
    let reads: Vec<_> = (0..4)
        .map(|index| {
            serde_json::json!({"tool_calls": [{
                "tool": "fixture_large",
                "args": {"key": format!("read-{{turn}}-{index}"), "bytes": 643}
            }]})
        })
        .chain([serde_json::json!({"text": "done"})])
        .collect();
    std::fs::write(
        &script,
        serde_json::to_vec(&serde_json::json!({
            "format": "lash-perf-replay/1",
            "name": "coding-turn-window",
            "description": "One coding turn's committed-window loads (FIG-6634)",
            "protocol": "standard",
            "sessions": 1,
            "turns": [{"user": "Read four files, then answer", "replies": reads}]
        }))
        .unwrap(),
    )
    .unwrap();
    let receipt = dir.path().join("receipt.json");
    let shapes = dir.path().join("shapes.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay")
        .arg(&script)
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&receipt)
        .arg("--sql-shapes")
        .arg(&shapes)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shapes: serde_json::Value =
        serde_json::from_slice(&std::fs::read(shapes).unwrap()).unwrap();
    let window = [
        "select revision.config_ref, head.head_revision, revision.leaf_node_id",
        "select session_state_version from session_meta",
    ];
    let turn = shapes["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|round| round["phase"] == "turn" && round["turn"] == 1)
        .expect("SQL shapes retain the measured turn separately from session start");
    let rows = turn["rows"].as_array().unwrap();
    let mut counted = std::collections::BTreeMap::new();
    for row in rows {
        let shape = row["shape"].as_str().unwrap();
        if let Some(read) = window.iter().find(|read| shape.contains(**read)) {
            *counted.entry(*read).or_insert(0) += row["selects"].as_u64().unwrap();
        }
    }
    let selects: u64 = counted.values().sum();
    eprintln!("FIG-6634 window-load SELECTs: {selects} {counted:?}");
    assert!(
        selects <= WINDOW_BOUND,
        "the coding turn issued {selects} SELECTs of its window loads {counted:?}, bound {WINDOW_BOUND}"
    );
}

/// The measured count after FIG-6634.
const WINDOW_BOUND: u64 = 15;

/// REPLAY-RECORD typed capture: a committed TypeScript session records
/// back as its cells, each turn's cells in order with their source, then its
/// text answer. The reader decodes the session's own cell records; neither a
/// cell's companion assistant context nor its fenced reply text becomes a
/// second reply.
#[test]
fn a_recorded_code_mode_session_keeps_its_cells() {
    use lash_perf::replay::recorder::redact_code;
    use lash_perf::replay::script::{Protocol, Reply, Source, Workload};

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("cells.json");
    let first = "const result = await tools.fixtureLookup({key: \"json\"});\nconsole.log(JSON.stringify(result));";
    let second = "console.log(\"bob@corp.io\".length);";
    std::fs::write(
        &script,
        serde_json::to_vec(&serde_json::json!({
            "format": "lash-perf-replay/1",
            "name": "recorded-cells",
            "description": "A code-mode session the recorder captures back",
            "protocol": "typescript",
            "sessions": 1,
            "turns": [
                {"user": "Read a value", "replies": [{"cell": first}, {"text": "done"}]},
                {"user": "Count it", "replies": [{"cell": first}, {"cell": second}, {"text": "ok"}]}
            ]
        }))
        .unwrap(),
    )
    .unwrap();
    let stores = dir.path().join("stores");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay")
        .arg(&script)
        .arg("--store-dir")
        .arg(&stores)
        .arg("--out")
        .arg(dir.path().join("receipt.json"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let captured = dir.path().join("captured.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay-record")
        .arg("--db")
        .arg(stores.join("recorded-cells").join("lash.db"))
        .args(["--session", "replay-recorded-cells-0", "--name", "captured"])
        .args(["--protocol", "typescript", "--out"])
        .arg(&captured)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let workload = Workload::parse(&std::fs::read_to_string(&captured).unwrap()).unwrap();
    let replies: Vec<_> = workload
        .turns
        .iter()
        .map(|turn| turn.replies.clone())
        .collect();
    let cell = |code: &str| Reply::Cell(Source(redact_code(Protocol::Typescript, code)));
    let text = |bytes| Reply::Text(lash_perf::replay::provider::large_text("reply", bytes));
    assert_eq!(
        replies,
        vec![
            vec![cell(first), text(4)],
            vec![cell(first), cell(second), text(2)],
        ],
        "{}",
        std::fs::read_to_string(&captured).unwrap()
    );
}

/// FIG-7084: five sequential code calls must not add a pair of standalone
/// root-lease commits per call. Publication and capture already own those
/// boundaries; counting COMMITs also includes their transaction envelopes.
#[test]
fn code_call_root_leases_share_the_checkpoint_transactions() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("leases.json");
    let mut cell: Vec<String> = (0..5)
        .map(|i| format!("await tools.fixtureLookup({{key: \"{i}\", i: {i}}});"))
        .collect();
    cell.push("await control.finish(\"ok\");".to_owned());
    std::fs::write(
        &script,
        serde_json::to_vec(&serde_json::json!({
            "format": "lash-perf-replay/1",
            "name": "cell-sequential-5",
            "description": "FIG-7084 root-lease commit budget",
            "protocol": "typescript",
            "turns": [{"repeat": 2, "user": "Turn {turn}: go", "replies": [{"cell": cell}]}]
        }))
        .unwrap(),
    )
    .unwrap();
    let shapes = dir.path().join("shapes.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .arg("replay")
        .arg(script)
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(dir.path().join("receipt.json"))
        .arg("--sql-shapes")
        .arg(&shapes)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shapes: serde_json::Value =
        serde_json::from_slice(&std::fs::read(shapes).unwrap()).unwrap();
    let turn = shapes["rounds"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["phase"] == "turn")
        .nth(1)
        .unwrap();
    let commits: u64 = turn["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["shape"] == "commit")
        .map(|row| row["statements"].as_u64().unwrap())
        .sum();
    eprintln!("five code calls: {commits} SQLite write commits (ceiling 24)");
    assert!(
        commits <= 24,
        "standalone root leases added write commits: {commits} > 24"
    );
}
