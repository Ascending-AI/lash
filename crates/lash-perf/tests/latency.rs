//! LATENCY-REMOTE: the remote latency instrument retains completed samples
//! without running the provider in its submitting process.
use std::process::Command;

#[test]
fn cross_worker_retains_remote_completion_samples() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let samples = dir.path().join("samples.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .args([
            "latency",
            "--cases",
            "cross-worker",
            "--lanes",
            "1",
            "--scale-down",
        ])
        .arg("--store-dir")
        .arg(dir.path().join("stores"))
        .arg("--out")
        .arg(&report)
        .arg("--samples-out")
        .arg(&samples)
        .output()
        .unwrap();
    // A standalone diagnostic cannot certify the omitted 10,000-sample fast gate.
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let gate: serde_json::Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(
        gate["verdict"]["violations"],
        serde_json::json!(["gated case `fast` did not run"])
    );
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(samples).unwrap()).unwrap();
    assert_eq!(rows.len(), 64);
    for row in rows {
        assert_eq!(row["status"], "answered");
        assert!(
            row["provider_ms"].is_null(),
            "the submitter must not execute the provider"
        );
        assert_eq!(row["poller_timed_out"], false);
        assert!(row["accept_to_applied_ms"].is_number());
        assert!(row["admission_to_settled_ms"].is_number());
    }
    assert!(report.exists());
}

/// LATENCY-BINDING: slowing follow backoff must not delay discovery of the
/// run whose terminal the remote follower waits on.
#[tokio::test]
async fn grace_case_probes_binding_before_remote_terminal_wait() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let samples = dir.path().join("samples.json");
    let output = tokio::time::timeout(
        // A functional watchdog, with a wide margin over the fixture's work;
        // this must not wait for the deliberately delayed 120-second poll.
        std::time::Duration::from_secs(20),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_lash-perf"))
            .args([
                "latency",
                "--cases",
                "grace",
                "--lanes",
                "1",
                "--scale-down",
            ])
            .arg("--store-dir")
            .arg(dir.path().join("stores"))
            .arg("--out")
            .arg(&report)
            .arg("--samples-out")
            .arg(&samples)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("the binding probe must keep its short floor")
    .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(samples).unwrap()).unwrap();
    assert_eq!(rows.len(), 16);
    assert!(rows.iter().all(|row| row["status"] == "answered"));
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(
        report["verdict"]["violations"],
        serde_json::json!(["gated case `fast` did not run"])
    );
}
