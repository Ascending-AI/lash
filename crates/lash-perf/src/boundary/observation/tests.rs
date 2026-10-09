//! FIG-5623's receipt rule: each observation workload proves the operations
//! and counters it names over SQLite stores, with no timing ceiling.
#![allow(clippy::unwrap_used)]
use super::super::{Args, Case, Receipt, run};

fn args(case: Case, dir: &std::path::Path, operations: usize, callers: usize) -> Args {
    Args {
        case,
        out: dir.join("receipt.json"),
        store_dir: dir.join("store"),
        operations,
        callers,
        postgres_url: None,
        workload: "smoke-v1".into(),
        dhat_out: None,
        dhat_frames: None,
        worker_stack_bytes: None,
    }
}

async fn receipt(case: Case, operations: usize, callers: usize) -> Receipt {
    let dir = tempfile::tempdir().unwrap();
    let receipt = run(&args(case, dir.path(), operations, callers))
        .await
        .unwrap();
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
    receipt
}

fn count(value: &serde_json::Value) -> u64 {
    value.as_u64().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_dispatcher_receipt_counts_store_round_trips_per_observation() {
    let receipt = receipt(Case::ProcessDispatcher, 8, 2).await;
    let replay = &receipt.counters["replay"];
    let provisional = count(&replay["language_drafts"]) + count(&replay["step_body_drafts"]);
    assert!(provisional > 0);
    assert!(count(&replay["publish_calls"]) <= count(&replay["drafts"]));
    assert_eq!(receipt.counters["store_invalidations"], 0);
    assert_eq!(receipt.evidence["feed_gaps"], 0);
    assert!(receipt.allocations.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_feeds_receipt_counts_publications_per_commit_and_recovers() {
    let receipt = receipt(Case::ProcessFeeds, 8, 2).await;
    let commits = count(&receipt.counters["durable_commits"]);
    assert!(commits >= 8);
    assert!(count(&receipt.counters["committed_publications"]) >= commits);
    for feed in receipt.counters["feeds"].as_array().unwrap() {
        assert_eq!(feed["terminal"], true);
    }
    assert_eq!(receipt.counters["recovered"]["terminal"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_burst_receipt_counts_invalidations_and_resnapshots() {
    let receipt = receipt(Case::ProcessBurst, 8, 2).await;
    assert!(receipt.counters["store_invalidations"].is_u64());
    assert_eq!(
        receipt.evidence["resnapshots"],
        receipt.counters["resnapshots"]
    );
    assert_eq!(receipt.evidence["bursting_processes"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_convergence_receipt_times_each_fact_on_both_replicas() {
    let receipt = receipt(Case::ProcessConvergence, 8, 1).await;
    let both = count(&receipt.counters["facts_on_both_replicas"]);
    assert!(both >= 8);
    let lag = receipt
        .latency
        .iter()
        .find(|latency| latency.boundary == "process.convergence.lag")
        .unwrap();
    assert_eq!(lag.samples as u64, both);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_reconcile_receipt_counts_the_facts_pushed_into_the_window() {
    let receipt = receipt(Case::ProcessReconcile, 16, 1).await;
    let behind = count(&receipt.counters["facts_behind"]);
    assert!(behind >= 16);
    assert_eq!(receipt.counters["publications_before_recover"], 0);
    assert_eq!(count(&receipt.counters["facts_pushed_into_window"]), behind);
    assert_eq!(receipt.evidence["gapped_anyway"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_roster_receipt_pages_every_process_and_change() {
    let receipt = receipt(Case::ProcessRoster, 6, 1).await;
    let scans = &receipt.counters["roster_scans"];
    assert_eq!(scans["unfiltered"]["rows"], 6);
    assert_eq!(scans["selecting_nothing"]["rows"], 0);
    assert!(count(&receipt.counters["change_feed"]["changes"]) >= 6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_replay_receipt_delivers_every_commit_to_every_observer() {
    let receipt = receipt(Case::SessionReplay, 3, 2).await;
    for observer in receipt.counters["observers"].as_array().unwrap() {
        assert_eq!(observer["events"]["committed"], 3);
        assert_eq!(observer["gaps"], 0);
    }
    assert_eq!(
        count(&receipt.counters["commits_published_before_send_settled"])
            + count(&receipt.counters["commits_published_after_send_settled"]),
        6
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_resume_receipt_gaps_every_observer_at_one_invalidation() {
    let receipt = receipt(Case::SessionResume, 3, 2).await;
    assert_eq!(receipt.counters["cursors_resumed"], 4);
    assert_eq!(receipt.counters["observers_gapped"], 2);
    assert_eq!(receipt.counters["observers_continued_after_gap"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_sink_receipts_carry_the_same_records_under_either_content_policy() {
    let omitted = receipt(Case::TraceSinkOmitted, 2, 1).await;
    let captured = receipt(Case::TraceSinkCaptured, 2, 1).await;
    assert_eq!(omitted.counters["content"], "omitted");
    assert_eq!(captured.counters["content"], "captured");
    assert!(count(&omitted.counters["records_offered"]) > 0);
    assert_eq!(
        omitted.counters["records_by_kind"],
        captured.counters["records_by_kind"]
    );
    assert!(count(&captured.counters["jsonl_bytes"]) > count(&omitted.counters["jsonl_bytes"]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_sink_custom_receipt_counts_each_custom_record() {
    let receipt = receipt(Case::TraceSinkCustom, 2, 4).await;
    assert_eq!(receipt.counters["custom_records"], 2);
    assert_eq!(receipt.counters["records_by_kind"]["custom"], 2);
    assert_eq!(receipt.counters["custom_payload_bytes"], 4096);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_sink_otel_receipt_counts_exported_spans() {
    let receipt = receipt(Case::TraceSinkOtel, 2, 1).await;
    assert!(count(&receipt.counters["otel_spans_exported"]) > 0);
    assert_eq!(
        receipt.counters["otel_exports"],
        receipt.counters["otel_spans_exported"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_sink_slow_receipt_blocks_the_emitter_and_loses_refused_records() {
    let receipt = receipt(Case::TraceSinkSlow, 2, 3).await;
    let offered = count(&receipt.counters["records_offered"]);
    assert_eq!(
        count(&receipt.counters["records_refused_and_lost"]),
        offered / 4
    );
    assert_eq!(count(&receipt.counters["emitter_blocked_ms"]), offered * 3);
    assert_eq!(receipt.evidence["settled_sends"], 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_fold_receipt_folds_every_delivered_observation() {
    let receipt = receipt(Case::OverlayFold, 4, 2).await;
    assert!(count(&receipt.counters["observations_folded"]) > 0);
    assert_eq!(
        receipt.counters["observations_folded"],
        receipt.counters["overlay_snapshots"]
    );
    assert!(count(&receipt.counters["document_sites"]) >= 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_attribution_receipt_observes_only_the_observed_run() {
    let receipt = receipt(Case::OverlayAttribution, 50, 2).await;
    assert!(count(&receipt.counters["observations_per_run"]) >= 50);
}

/// FIG-5637's rule: the owner id is the durable node name, and a second boot
/// under one name fences the first.
#[test]
fn fleet_members_register_under_distinct_node_names() {
    let (first, second) = (
        super::node_identity(0, "run"),
        super::node_identity(1, "run"),
    );
    assert_ne!(first.owner_id, second.owner_id);
    assert_eq!(first.incarnation_id, second.incarnation_id);
}
