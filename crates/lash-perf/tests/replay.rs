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
    let evidence = std::env::var_os("TEST_UNDECLARED_OUTPUTS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dir.path().to_path_buf());
    let receipt = evidence.join("replay-receipt.json");
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

/// FIG-6467: a fresh code-mode turn batches package retention instead of
/// spending a few SQL statements per helper. Its first environment is
/// prepared and published in the turn. The pre-heap replay used 570
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
        .expect("node registration has its own startup receipt");
    let writes = |phase: &serde_json::Value, prefix: &str| -> u64 {
        phase["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["shape"].as_str().unwrap().starts_with(prefix))
            .map(|row| row["statements"].as_u64().unwrap())
            .sum()
    };
    assert_eq!(
        writes(startup, "insert into main.nodes "),
        1,
        "startup accounts for the serving node's registration"
    );
    // Package definitions are no longer installed in a node-wide index.
    // The first model boundary prepares the environment's version owner,
    // then publishes it with its head; both costs belong to this turn.
    for prefix in [
        "insert into main.environment_owners ",
        "insert into main.environment_heads ",
        "update main.environment_owners set state = ",
    ] {
        for phase in phases {
            let expected = u64::from(phase["phase"] == "turn");
            assert_eq!(
                writes(phase, prefix),
                expected,
                "environment publication {prefix} in phase {}",
                phase["phase"]
            );
        }
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

/// FIG-7084 / FIG-7314: five sequential code calls share root-lease commits
/// with checkpoints and acquire all seven execution package holds together.
/// Warm turn: admission + model.start 1, model.answer 1,
/// model.done+cell.snapshot+admit 1, cell.snapshot+admit 5, round.outcome 6,
/// turn.commit 1, pages.lease 2, package batch 1, ingress 1: exactly 19 commits.
/// The session reader's two standalone lease commits remain pinned separately.
/// Labeled pruning, reap and heartbeat work belongs to maintenance, not this
/// turn-work population. WAKE-MAINTENANCE laws independently pin zero empty
/// commits and one commit per nonempty expiry pass.
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
    let work: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("turn-sql-work.json")).unwrap())
            .unwrap();
    let mut operations = std::collections::BTreeMap::<&str, u64>::new();
    for row in work["work"]["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| {
            row["started_in"]["phase"] == "turn"
                && row["started_in"]["turn"] == 2
                && row["shape"] == "commit"
        })
    {
        *operations
            .entry(row["operation"].as_str().unwrap_or("package/ingress"))
            .or_default() += row["statements"].as_u64().unwrap();
    }
    let maintenance =
        |operation: &str| matches!(operation, "node_wakes.prune" | "reap" | "heartbeat");
    let turn_commits: u64 = operations
        .iter()
        .filter(|(operation, _)| !maintenance(operation))
        .map(|(_, count)| count)
        .sum();
    eprintln!(
        "five code calls: {commits} raw SQLite write commits, {turn_commits} turn commits (exactly 19): {operations:?}"
    );
    assert_eq!(
        operations.get("pages.lease"),
        Some(&2),
        "only the session reader acquires and releases a standalone root lease"
    );
    assert_eq!(
        turn_commits, 19,
        "the seven package holds must share one transaction"
    );
}

/// L-PIN / FIG-7186: the chat-short version owner's package facts outlive
/// turns and a cold reopen without rewriting their closure or cleanup, and
/// disappear once after their session's retained graph retires.
#[tokio::test]
async fn chat_short_reuses_session_package_facts_without_turn_writes() {
    use lash_core_store::store::AttachmentReferrers;
    use lash_durable::DurableStore;
    use lash_perf::replay::{provider, script};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chat.db");
    let workload = script::resolve("chat-short").unwrap();
    let mut book = provider::ScriptBook::new();
    let inputs: Vec<_> = workload
        .expand(0, Some(4))
        .iter()
        .map(|turn| book.register(0, turn))
        .collect();
    let book = Arc::new(book);
    let id = lash::SessionId::fixture("session-package-facts");
    let mut initial_obligation_writes = None;
    for half in 0..2 {
        let stores =
            lash::sqlite::SqliteStoreSet::open(&path, lash::sqlite::SqliteSynchronous::Normal)
                .await
                .unwrap();
        let catalog = stores.session_store_factory();
        let pages = stores.durable_store().pages();
        let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
            .build()
            .unwrap();
        let core = lash::LashCore::standard_builder(backend)
            .serve_sessions(true)
            .commit_budget(lash::CommitBudget::bounded(64 * 1024 * 1024, 4096))
            .data_retention(lash::DataRetention::standard())
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .execution_budgets(lash::ExecutionBudgets::recommended())
            .delta_coalescing(lash::DeltaCoalescing::recommended())
            .serve_test_llm_profile(provider::model(book.clone()), provider::metadata().unwrap())
            .plugin(Arc::new(
                lash_perf::perf_support::tool_plugin(
                    "tools",
                    lash::tool_provider_key!("tools"),
                    provider::fixtures().unwrap(),
                )
                .unwrap(),
            ))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                lash::persistence::LeaseOwnerId::new("package-facts"),
                lash::persistence::LeaseIncarnationId::new(format!("half-{half}")),
            ))
            .unwrap();
        let cleanup = lash::testing::quiesce_artifact_cleanup(&core).await;
        if half == 0 {
            core.session(id.clone())
                .create(lash::SessionCreation::root(lash::SessionSpec::new(
                    provider::MODEL,
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1_000_000),
                    lash_core::NoProgressBudget::bounded(12),
                )))
                .await
                .unwrap();
        }
        let session = core.session(id.clone()).open().await.unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.busy_timeout(std::time::Duration::from_secs(30)).unwrap();
        if half == 0 {
            db.execute_batch("CREATE TABLE fact_hashes (reference TEXT PRIMARY KEY, hash BLOB NOT NULL);
                CREATE TABLE fact_writes (family TEXT, verb TEXT, identity TEXT, owner TEXT);
                CREATE TABLE blob_writes (verb TEXT, hash BLOB);
                CREATE TRIGGER fact_pointer_insert AFTER INSERT ON artifact_refs
                WHEN new.namespace = 'kernel_document' AND (new.artifact_ref LIKE 'package:v1:%' OR new.artifact_ref LIKE 'publication:v1:%')
                BEGIN
                    INSERT OR IGNORE INTO fact_hashes VALUES (new.artifact_ref, new.blob_ref);
                    INSERT INTO fact_writes VALUES ('pointer', 'insert', new.artifact_ref, NULL);
                END;
                CREATE TRIGGER fact_pointer_delete AFTER DELETE ON artifact_refs
                WHEN old.artifact_ref IN (SELECT reference FROM fact_hashes)
                BEGIN INSERT INTO fact_writes VALUES ('pointer', 'delete', old.artifact_ref, NULL); END;
                CREATE TRIGGER fact_pointer_update AFTER UPDATE ON artifact_refs
                WHEN old.artifact_ref IN (SELECT reference FROM fact_hashes)
                BEGIN INSERT INTO fact_writes VALUES ('pointer', 'update', old.artifact_ref, NULL); END;
                CREATE TRIGGER fact_edge_insert AFTER INSERT ON artifact_referrer_edges
                WHEN new.artifact_ref IN (SELECT reference FROM fact_hashes)
                BEGIN INSERT INTO fact_writes VALUES ('edge', 'insert', new.artifact_ref, new.referrer_kind); END;
                CREATE TRIGGER fact_edge_delete AFTER DELETE ON artifact_referrer_edges
                WHEN old.artifact_ref IN (SELECT reference FROM fact_hashes)
                BEGIN INSERT INTO fact_writes VALUES ('edge', 'delete', old.artifact_ref, old.referrer_kind); END;
                CREATE TRIGGER fact_edge_update AFTER UPDATE ON artifact_referrer_edges
                WHEN old.artifact_ref IN (SELECT reference FROM fact_hashes)
                BEGIN INSERT INTO fact_writes VALUES ('edge', 'update', old.artifact_ref, old.referrer_kind); END;
                CREATE TRIGGER fact_version_obligation_insert AFTER INSERT ON artifact_cleanup_obligations
                WHEN new.referrer_kind = 'environment_version'
                BEGIN INSERT INTO fact_writes VALUES ('obligation', 'insert', new.referrer_id, 'environment_version'); END;
                CREATE TRIGGER fact_version_obligation_update AFTER UPDATE ON artifact_cleanup_obligations
                WHEN new.referrer_kind = 'environment_version'
                BEGIN INSERT INTO fact_writes VALUES ('obligation', 'update', new.referrer_id, 'environment_version'); END;
                CREATE TRIGGER fact_version_obligation_delete AFTER DELETE ON artifact_cleanup_obligations
                WHEN old.referrer_kind = 'environment_version'
                BEGIN INSERT INTO fact_writes VALUES ('obligation', 'delete', old.referrer_id, 'environment_version'); END;
                CREATE TRIGGER fact_blob_insert AFTER INSERT ON blobs
                BEGIN INSERT INTO blob_writes VALUES ('insert', new.hash); END;
                CREATE TRIGGER fact_blob_delete AFTER DELETE ON blobs
                BEGIN INSERT INTO blob_writes VALUES ('delete', old.hash); END;
                CREATE TRIGGER fact_blob_update AFTER UPDATE ON blobs
                BEGIN INSERT INTO blob_writes VALUES ('update', old.hash); END;").unwrap();
        }
        let count = |sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap() };
        for (turn, input) in inputs.iter().enumerate().skip(half * 2).take(2) {
            let handle = session
                .send(lash::TurnInput::text(input.clone()))
                .into_future()
                .await
                .unwrap();
            let outcome = handle.outcome().await.unwrap();
            assert!(outcome.output().unwrap().is_success(), "{outcome:?}");
            cleanup.settle().await.unwrap();
            let pointers = count(
                "SELECT count(*) FROM fact_writes WHERE family = 'pointer' AND verb = 'insert'",
            );
            let edges = count(
                "SELECT count(*) FROM fact_writes WHERE family = 'edge' AND verb = 'insert' AND owner = 'environment_version'",
            );
            let blobs = count(
                "SELECT count(*) FROM blob_writes WHERE verb = 'insert' AND hash IN (SELECT hash FROM fact_hashes)",
            );
            eprintln!(
                "chat-short turn {} cumulative package writes: blobs={blobs} pointers={pointers} version_edges={edges}",
                turn + 1
            );
            assert_eq!((blobs, pointers, edges), (6, 6, 6));
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM artifact_referrer_edges AS edge
                     JOIN environment_heads AS head
                       ON edge.referrer_id = json_extract(head.head_json, '$.owner')
                     WHERE head.session_id = ?1 AND edge.referrer_kind = 'environment_version'
                       AND edge.artifact_ref IN (SELECT reference FROM fact_hashes)",
                    [id.as_str()],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap(),
                6,
                "the unchanged head's version owner retains the complete package closure"
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM fact_writes WHERE family = 'obligation' AND verb = 'insert'"
                ),
                1,
                "preparation arms exactly one version cleanup obligation"
            );
            let obligation_writes =
                count("SELECT count(*) FROM fact_writes WHERE family = 'obligation'");
            assert_eq!(
                obligation_writes,
                *initial_obligation_writes.get_or_insert(obligation_writes),
                "unchanged package retention rewrote version cleanup"
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM fact_writes WHERE family != 'obligation' AND verb != 'insert'"
                ),
                0
            );
            assert_eq!(
                count(
                    "SELECT count(*) FROM blob_writes WHERE verb != 'insert' AND hash IN (SELECT hash FROM fact_hashes)"
                ),
                0
            );
        }
        let administration = session.session_administration();
        session.close().await.unwrap();
        if half == 1 {
            lash::LashCore::delete_session(administration.delete_context(id.as_str()).unwrap())
                .await
                .unwrap();
            core.await_session_deletion(&id).await.unwrap();
            let pages = pages.unwrap();
            while pages
                .retire_step(lash_durable::domain::pages::RetirementBudget {
                    nodes: lash_durable::RetirementSettings::MAX_BATCH_NODES,
                })
                .await
                .unwrap()
                != lash_durable::domain::pages::RetirementProgress::default()
            {}
            assert_eq!(
                catalog.session_referrer_state(&id).await.unwrap(),
                lash_core_store::store::SessionReferrerState::DeletedRetired
            );
            cleanup.settle().await.unwrap();
            for family in ["pointer", "edge"] {
                assert_eq!(
                    db.query_row(
                        "SELECT count(*) FROM fact_writes WHERE family = ?1 AND verb = 'delete'",
                        [family],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                    6
                );
            }
            assert_eq!(
                count(
                    "SELECT count(*) FROM blob_writes WHERE verb = 'delete' AND hash IN (SELECT hash FROM fact_hashes)"
                ),
                6
            );
            cleanup.settle().await.unwrap();
            assert_eq!(
                count(
                    "SELECT count(*) FROM fact_writes WHERE verb = 'delete' AND family != 'obligation'"
                ),
                12
            );
        }
        drop(cleanup);
        core.shutdown().await.unwrap();
    }
}
