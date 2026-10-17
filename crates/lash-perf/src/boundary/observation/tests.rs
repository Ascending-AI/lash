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
        future_out: None,
        future_top: 20,
        worker_stack_bytes: None,
        ledger_cap: 100_000,
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
    assert_eq!(receipt.counters["records_by_kind"]["extension_span"], 2);
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

/// START-DECLARATIONS-ONCE (FIG-6548): starting an already published
/// 70-declaration workflow adds retention edges, never declaration nodes.
/// Set acquisition and adoption add a bounded number of SQL statements,
/// independent of the declaration count.
#[tokio::test]
async fn a_published_workflow_start_references_seventy_declarations_in_bounded_work() {
    use super::{Flavor, Fleet, QUIET_DOCUMENT, environment, publish, start};
    use lash_core_ids::perf_witness::Collector;
    use std::collections::{BTreeMap, BTreeSet};

    let dir = tempfile::tempdir().unwrap();
    let args = args(Case::ProcessRoster, dir.path(), 1, 1);
    let fleet = Fleet::open(
        &args,
        &super::super::Meter::default(),
        1,
        Flavor::Workflow,
        |builder, _| Ok(builder.serve_sessions(false)),
    )
    .await
    .unwrap();
    let core = &fleet.nodes[0].core;
    let offered = core
        .host_artifacts()
        .workflow_environment(&environment(false).unwrap())
        .await
        .unwrap()
        .unwrap();
    let mut names = BTreeSet::new();
    let helpers: Vec<_> = offered
        .functions()
        .iter()
        .filter(|(_, f)| {
            f.definition()
                .body()
                .is_none_or(|body| body.functions.is_empty())
        })
        .filter(|(_, f)| names.insert(f.definition().name.clone()))
        .take(70)
        .collect();
    assert_eq!(helpers.len(), 70);
    let uses = helpers
        .iter()
        .map(|(id, f)| format!("use {} = @{id}\n", f.definition().name))
        .collect::<String>();
    let calls = helpers
        .iter()
        .enumerate()
        .map(|(i, (_, f))| {
            let args = vec!["arg"; f.definition().signature.required()].join(", ");
            format!("let v{i} = invoke {}({args})\n", f.definition().name)
        })
        .collect::<String>();
    let document = QUIET_DOCUMENT
        .replacen("entry observed", &format!("{uses}entry observed"), 1)
        .replace(
            "main {",
            &format!("fn declarations(arg) {{\n{calls}return null\n}}\nmain {{"),
        );
    let witness = Collector::install_with_sql_shapes().unwrap();
    let probe = rusqlite::Connection::open(args.store_dir.join("lash.db")).unwrap();
    let nodes = || {
        probe
            .query_row("SELECT COUNT(*) FROM blobs WHERE encoding = 1", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap()
    };
    let mut readings = Vec::new();
    for (name, text) in [("control", QUIET_DOCUMENT), ("seventy", document.as_str())] {
        let before = witness.snapshot();
        let before_nodes = nodes();
        let at = std::time::Instant::now();
        let published = publish(core, text).await.unwrap();
        eprintln!(
            "START-COST {name} publication statements={} rows={} nodes={} wall_ms={:.3}",
            witness.snapshot().sql_statements - before.sql_statements,
            witness.snapshot().sql_rows_written - before.sql_rows_written,
            nodes() - before_nodes,
            at.elapsed().as_secs_f64() * 1000.0
        );
        for n in 0..2 {
            let before = witness.snapshot();
            let before_nodes = nodes();
            let at = std::time::Instant::now();
            start(core, &published, &format!("cost-{name}-{n}"))
                .await
                .unwrap();
            let wall_ms = at.elapsed().as_secs_f64() * 1000.0;
            let after = witness.snapshot();
            let statements = after.sql_statements - before.sql_statements;
            let rows = after.sql_rows_written - before.sql_rows_written;
            let nodes = nodes() - before_nodes;
            let shapes = after
                .sql_shapes
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .filter_map(|row| {
                    let old = before
                        .sql_shapes
                        .as_ref()
                        .unwrap()
                        .rows
                        .iter()
                        .find(|old| old.shape == row.shape && old.store_origin == row.store_origin)
                        .map_or(0, |old| old.counts.statements);
                    let count = row.counts.statements - old;
                    if count == 0 {
                        None
                    } else {
                        Some((format!("{}: {}", row.store_origin, row.shape), count))
                    }
                })
                .collect::<BTreeMap<_, _>>();
            assert_eq!(after.sql_shapes.as_ref().unwrap().overflow.statements, 0);
            eprintln!(
                "START-COST {name} start={n} statements={statements} rows={rows} nodes={nodes} wall_ms={wall_ms:.3} shapes={shapes:?}"
            );
            readings.push((statements, rows, nodes));
        }
    }
    fleet.close().await.unwrap();
    for (control, seventy) in readings[..2].iter().zip(&readings[2..]) {
        assert!(
            seventy.0 <= control.0 + 16,
            "70 stored declarations require set acquisition, not per-declaration SQL: control={control:?}, seventy={seventy:?}"
        );
        // This producer-only node does not deliver the Start guard yet:
        // exactly one new Start edge per declaration, held for adoption.
        assert_eq!(
            seventy.1,
            control.1 + 70,
            "a start adds only its declaration retention edges: {readings:?}"
        );
        assert_eq!(
            seventy.2, 0,
            "declarations are published once: {readings:?}"
        );
    }
}
