//! Latency receipts retain engine clocks and physical SQLite work.
use std::process::Command;

/// FIG-5933: both topologies publish causal phase clocks and physical work,
/// including a fast run whose admission races the acceptance acknowledgement.
#[test]
fn latency_receipts_use_engine_clocks_and_include_remote_work() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let samples = dir.path().join("samples.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .args([
            "latency",
            "--cases",
            "fast,cross-worker,grace",
            "--lanes",
            "3",
            "--fast-samples",
            "2",
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
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let gate: serde_json::Value = serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap();
    assert_eq!(gate["cases"][0]["actual_lanes"], 2);
    for case in gate["cases"].as_array().unwrap() {
        assert!(
            case["sqlite_work"]["write_transactions_per_send"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        assert!(
            case["sqlite_work"]["writer_busy_ms_per_send"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        assert!(
            case["sqlite_work"]["reader_busy_ms_per_send"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        assert!(case["baseline"]["sha"].is_string());
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(samples).unwrap()).unwrap();
    assert_eq!(rows.len(), 82);
    for row in rows {
        assert_eq!(row["status"], "answered");
        let clocks = &row["clocks"];
        let ordered: Vec<u64> = ["request", "accepted", "admission", "terminal", "completion"]
            .iter()
            .map(|key| clocks[key]["monotonic_ns"].as_u64().unwrap())
            .collect();
        assert!(ordered.windows(2).all(|pair| pair[0] <= pair[1]), "{row}");
        for key in ["request", "accepted", "admission", "terminal", "completion"] {
            assert!(clocks[key]["unix_ns"].as_u64().unwrap() > 0);
        }
        for key in [
            "accept_to_admission_ms",
            "admission_to_first_visible_ms",
            "admission_to_settled_ms",
            "settled_to_complete_ms",
            "provider_ms",
            "overhead_ms",
            "admission_to_first_delta_ms",
        ] {
            assert!(row[key].as_f64().unwrap() >= 0.0, "{key}: {row}");
        }
        assert!(clocks["first_delta"]["monotonic_ns"].as_u64().unwrap() >= ordered[2]);
        assert!(
            clocks["first_activity"]["monotonic_ns"].as_u64().unwrap()
                <= clocks["first_delta"]["monotonic_ns"].as_u64().unwrap()
        );
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(printed.contains("writer busy ms/send"));
    assert!(printed.contains("write transactions/send"));
    assert!(printed.contains("absolute clocks"));
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
