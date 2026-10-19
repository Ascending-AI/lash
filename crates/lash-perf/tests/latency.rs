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
    // Completion and the ledger below are the oracle. Startup, SQLite work
    // and teardown have no fixed wall-time budget on the shared executor.
    // grace_binding_discovery_stays_at_floor_after_unbound_reads pins the
    // probe cadence separately with store gates and a paused clock.
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_lash-perf"))
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
        .output()
        .await
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
        serde_json::json!([{ "reason": "missing-case" }])
    );
}

/// FIG-6841: each chat-loop case derives its own spans for every sample from
/// the fixtures' absolute clocks, in both topologies, and every causal span
/// is non-negative.
#[test]
fn chat_loop_cases_derive_their_spans_for_every_sample() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    let samples = dir.path().join("samples.json");
    let output = Command::new(env!("CARGO_BIN_EXE_lash-perf"))
        .args([
            "latency",
            "--cases",
            "cold,delivery,tool,cell,steer,cross-worker-tool",
            "--lanes",
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
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(samples).unwrap()).unwrap();
    let expected: &[(&str, &[&str])] = &[
        ("cold", &["session_open", "request_to_provider"]),
        ("delivery", &["first_delta_overhead", "request_to_provider"]),
        (
            "tool",
            &[
                "model_to_tool_start",
                "tool",
                "tool_to_recorded",
                "tool_to_next_request",
            ],
        ),
        (
            "cell",
            &["model_to_tool_start", "tool", "tool_to_next_request"],
        ),
        (
            "steer",
            &[
                "steer_request_to_accept",
                "boundary_to_steer_seen",
                "steer_send_to_model",
            ],
        ),
        (
            "cross-worker-tool",
            &["model_to_tool_start", "tool", "tool_to_next_request"],
        ),
    ];
    for (case, spans) in expected {
        let case_rows: Vec<_> = rows.iter().filter(|row| row["case"] == *case).collect();
        assert_eq!(case_rows.len(), 64, "{case}");
        for row in case_rows {
            assert_eq!(row["status"], "answered", "{row}");
            for span in *spans {
                assert!(
                    row["loop_spans"][span].as_f64().unwrap() >= 0.0,
                    "{case} {span}: {row}"
                );
            }
            if *case == "delivery" {
                let delivered = row["loop_spans"]["delta_delivery"].as_array().unwrap();
                assert_eq!(delivered.len(), 32, "{row}");
                assert!(delivered.iter().all(|ms| ms.as_f64().unwrap() >= 0.0));
            }
            if *case != "cold" && *case != "steer" && row["index"] != 0 {
                assert!(
                    row["loop_spans"]["previous_completion_to_provider"]
                        .as_f64()
                        .unwrap()
                        >= 0.0,
                    "{row}"
                );
            }
        }
    }
}
