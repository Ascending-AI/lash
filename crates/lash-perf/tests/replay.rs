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
