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
