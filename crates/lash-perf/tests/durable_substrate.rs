//! DURABLE-CELL (FIG-5923): the durable-substrate cell case runs its cell to
//! the end on the worker it names, and a cell session that parks is refused
//! at its park rather than after the turn deadline.
use std::ffi::OsStr;
use std::path::Path;
use std::process::Output;
use std::time::Duration;

/// Well inside the bench's 600-second turn deadline: a stalled turn fails
/// here instead of holding the test for ten minutes.
const LIMIT: Duration = Duration::from_secs(120);

/// One `cell-0` batch on a fresh SQLite file under `dir`, its cells run on
/// `worker` (named as the documented command names it), held to [`LIMIT`].
async fn cell_0(
    dir: &Path,
    worker: &OsStr,
) -> Result<std::io::Result<Output>, tokio::time::error::Elapsed> {
    let run = tokio::process::Command::new(env!("CARGO_BIN_EXE_durable-substrate"))
        .args(["--store", "sqlite", "--case", "cell-0", "--samples", "1"])
        .env("LASH_VM_WORKER", worker)
        .arg("--sqlite-dir")
        .arg(dir.join("db"))
        .arg("--out")
        .arg(dir.join("cells.jsonl"))
        .kill_on_drop(true)
        .output();
    tokio::time::timeout(LIMIT, run).await
}

/// The report's records of `kind`.
fn records(dir: &Path, kind: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let report = std::fs::read_to_string(dir.join("cells.jsonl"))?;
    let mut records = Vec::new();
    for line in report.lines() {
        let record: serde_json::Value = serde_json::from_str(line)?;
        if record["kind"] == kind {
            records.push(record);
        }
    }
    Ok(records)
}

/// Each of the cell's ten awaits commits its park with the operation's
/// admission and settles one round outcome; the first park also commits the
/// model response that asked for the cell (FIG-6000); the cell's end commits
/// with the commit that consumes its answer (FIG-5999), so it is no park.
/// The bench reports every park as a block and the batch's counters.
#[tokio::test]
async fn a_cell_settles_every_scripted_await_and_reports_each_park() {
    let dir = tempfile::tempdir().unwrap();
    let worker = std::env::var_os("LASH_VM_WORKER").expect("LASH_VM_WORKER names the VM worker");
    let output = cell_0(dir.path(), &worker)
        .await
        .expect("the cell case neither finished nor refused within the limit")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let labels: Vec<String> = records(dir.path(), "block")
        .unwrap()
        .iter()
        .map(|block| block["label"].as_str().unwrap().to_owned())
        .collect();
    let mut parks = vec!["cell.snapshot+admit".to_owned(); 10];
    parks[0] = "model.done+cell.snapshot+admit".to_owned();
    assert_eq!(labels, parks);
    let batches = records(dir.path(), "batch").unwrap();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0]["transactions"]["round.outcome"], 10);
    assert_eq!(
        batches[0]["transactions"]["model.done+cell.snapshot+admit"],
        1
    );
    assert_eq!(batches[0]["transactions"]["cell.snapshot+admit"], 9);
    let summaries = records(dir.path(), "summary").unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0]["blocks"], 10);
}

/// A cell session whose worker cannot start parks after its failed passes;
/// the bench refuses the case at that park, naming its reason, and reports
/// no sample.
#[tokio::test]
async fn a_parked_cell_session_is_refused_at_its_park() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-worker");
    let output = cell_0(dir.path(), missing.as_os_str())
        .await
        .expect("the cell case neither finished nor refused within the limit")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("parked"), "{stderr}");
    assert!(stderr.contains("vm_worker_unavailable"), "{stderr}");
    assert!(records(dir.path(), "block").unwrap().is_empty());
    assert!(records(dir.path(), "batch").unwrap().is_empty());
}
