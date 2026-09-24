//! T14 (FIG-3586): committed cell journals replay on every change.
//!
//! The corpus is one SQLite effect journal recorded by a real run of each
//! scenario's cells — loops, aggregates, a race against a timer, sleeps,
//! journaled runtime values, the deferred resolution every cell links under,
//! and two cells of one turn sharing their interpreter — together with the
//! answer each cell gave. Every run of this suite re-executes the scenarios
//! against a copy of that journal and requires the recorded answers with
//! nothing dispatched and nothing journaled.
//!
//! A change to how cells lower, or to the order their commands leave the VM,
//! that would re-key or re-order a committed journal fails here, the way a
//! workflow replayer fails a history the current code cannot reproduce. Such a
//! change must move `LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION` (and refuse older
//! journals at the cutover) before the corpus is re-recorded:
//!
//! ```text
//! LASH_REGENERATE_CELL_REPLAY_CORPUS=1 kiln run \
//!   //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test -- \
//!   executor::tests::replay_corpus::regenerate_cell_replay_corpus --ignored --exact
//! ```

// FIG-2971: test code; ambient fs/env access is sanctioned here.
#![allow(clippy::disallowed_methods)]

use super::replay_ordinals::{AppTools, CellAddress, Journal, run_cell};
use super::*;

const SESSION: &str = "cell-replay-corpus";
const REGENERATE_ENV: &str = "LASH_REGENERATE_CELL_REPLAY_CORPUS";
const JOURNAL: &[u8] = include_bytes!("fixtures/cell_replay_corpus/journal.sqlite");
const MANIFEST: &str = include_str!("fixtures/cell_replay_corpus/manifest.json");

struct Scenario {
    name: &'static str,
    cells: &'static [&'static str],
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "scalar-calls-and-runtime-values",
        cells: &[r#"
            const first = await app.a({ n: 1 });
            const now = Date.now();
            const roll = Math.random();
            const second = await app.b({ n: 2, after: first.args.n });
            finish({ first: first, now: now, roll: roll, second: second });
        "#],
    },
    Scenario {
        name: "loop",
        cells: &[r#"
            const seen = [];
            for (let i = 0; i < 3; i++) {
                const reply = await app.a({ n: i });
                seen.push(reply.args.n);
            }
            finish(seen);
        "#],
    },
    Scenario {
        name: "aggregates",
        cells: &[r#"
            const all = await Promise.all([app.a({ n: 1 }), app.b({ n: 2 })]);
            const settled = await Promise.allSettled([app.a({ n: 3 }), app.b({ n: 4 })]);
            const raced = await Promise.race([app.b({ n: 5 }), sleep(60000)]);
            finish({ all: all, settled: settled, raced: raced });
        "#],
    },
    Scenario {
        name: "sleeps",
        cells: &[r#"
            await sleep(0);
            const reply = await app.a({ n: 1 });
            await sleep(0);
            finish(reply);
        "#],
    },
    Scenario {
        name: "two-cells-one-turn",
        cells: &[
            r#"
            const carried = await app.a({ n: 1 });
            print("first cell");
            "#,
            r#"
            const next = await app.b({ n: carried.args.n + 1 });
            finish({ carried: carried, next: next });
            "#,
        ],
    },
];

fn cell(scenario: &Scenario, index: usize) -> CellAddress<'static> {
    CellAddress {
        session: SESSION,
        turn: scenario.name,
        exec_key: match index {
            0 => "exec-code:cell-0",
            1 => "exec-code:cell-1",
            _ => panic!("corpus scenarios have at most two cells"),
        },
    }
}

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct RecordedScenario {
    /// Each cell's answer: its `finish` value, or `null` for a cell that
    /// hands the turn on.
    finishes: Vec<Option<serde_json::Value>>,
    /// The replay and group keys the scenario's turn journaled.
    replay_keys: Vec<String>,
    group_keys: Vec<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Manifest {
    format: String,
    grammar: u32,
    dispatched: usize,
    scenarios: std::collections::BTreeMap<String, RecordedScenario>,
}

const FORMAT: &str =
    "lash-protocol-rlm cell replay corpus v1: one SQLite effect journal, one turn per scenario";

async fn record(journal: &Journal, scenario: &Scenario, tools: &AppTools) -> RecordedScenario {
    let mut state = RlmExecutionState::for_engine("typescript");
    let mut finishes = Vec::with_capacity(scenario.cells.len());
    for (index, code) in scenario.cells.iter().enumerate() {
        let run = run_cell(
            cell(scenario, index),
            code,
            tools,
            journal.host().await,
            &mut state,
        )
        .await;
        run.assert_clean();
        finishes.push(run.response.terminal_finish.clone());
    }
    let (replay_keys, group_keys) = journal.keys_of(SESSION, scenario.name).await;
    RecordedScenario {
        finishes,
        replay_keys,
        group_keys,
    }
}

#[test]
fn cell_replay_corpus_replays_with_nothing_dispatched() {
    let manifest: Manifest = serde_json::from_str(MANIFEST).expect("the corpus manifest decodes");
    assert_eq!(manifest.format, FORMAT);
    assert_eq!(
        manifest.grammar,
        lash_lashlang_runtime::LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION,
        "the corpus was recorded under another replay-key grammar: a grammar bump re-records it"
    );
    assert_eq!(
        manifest.scenarios.keys().cloned().collect::<Vec<_>>(),
        {
            let mut names = SCENARIOS
                .iter()
                .map(|scenario| scenario.name.to_string())
                .collect::<Vec<_>>();
            names.sort();
            names
        },
        "every recorded scenario has exactly one registered scenario"
    );
    block_on(async {
        let journal = Journal::open();
        std::fs::write(&journal.path, JOURNAL).expect("copy the committed journal");
        let tools = AppTools::default();
        for scenario in SCENARIOS {
            let recorded = &manifest.scenarios[scenario.name];
            assert!(
                !recorded.replay_keys.is_empty(),
                "{}: the corpus journals the scenario's commands",
                scenario.name
            );
            let replayed = record(&journal, scenario, &tools).await;
            assert_eq!(
                &replayed, recorded,
                "{}: the committed journal replays to its recorded answers with no new row",
                scenario.name
            );
        }
        assert_eq!(
            tools.dispatched(),
            0,
            "replaying the committed corpus dispatches nothing"
        );
    });
}

#[test]
#[ignore = "re-records the committed cell replay corpus; set LASH_REGENERATE_CELL_REPLAY_CORPUS=1"]
fn regenerate_cell_replay_corpus() {
    assert_eq!(
        std::env::var(REGENERATE_ENV).as_deref(),
        Ok("1"),
        "set {REGENERATE_ENV}=1 to acknowledge replacing the committed corpus"
    );
    let directory = std::env::var_os("BUILD_WORKSPACE_DIRECTORY").map_or_else(
        || std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        |root| std::path::PathBuf::from(root).join("crates/lash-protocol-rlm"),
    );
    let directory = directory.join("src/executor/tests/fixtures/cell_replay_corpus");
    block_on(async {
        let journal = Journal::open();
        let tools = AppTools::default();
        let mut scenarios = std::collections::BTreeMap::new();
        for scenario in SCENARIOS {
            scenarios.insert(
                scenario.name.to_string(),
                record(&journal, scenario, &tools).await,
            );
        }
        let manifest = Manifest {
            format: FORMAT.to_string(),
            grammar: lash_lashlang_runtime::LASHLANG_CELL_JOURNAL_GRAMMAR_VERSION,
            dispatched: tools.dispatched(),
            scenarios,
        };
        std::fs::create_dir_all(&directory).expect("create the corpus directory");
        // The journal runs in WAL mode and its host may keep its connection,
        // so the newest rows can still sit in the log: fold them in and write
        // one compact, self-contained file.
        let target = directory.join("journal.sqlite");
        let _ = std::fs::remove_file(&target);
        let connection =
            rusqlite::Connection::open(&journal.path).expect("open the recorded journal");
        connection
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .expect("checkpoint the recorded journal");
        connection
            .execute(
                "VACUUM INTO ?1",
                [target.to_str().expect("the corpus path is UTF-8")],
            )
            .expect("write the corpus journal");
        std::fs::write(
            directory.join("manifest.json"),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&manifest).expect("encode the corpus manifest")
            ),
        )
        .expect("write the corpus manifest");
    });
}
