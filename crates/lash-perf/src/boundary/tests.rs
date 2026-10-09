//! FIG-5562's receipt rule: each population proves the named operations,
//! independently of any timing ceiling. The CLI proves the OS-writer case.
#![allow(clippy::unwrap_used)]
use super::*;

#[tokio::test]
async fn wire_slots_receipt_counts_every_retry_and_derivative_boundary() {
    let receipt = attachments::run(1).await.unwrap();
    assert_eq!(receipt.evidence["attempts"], 3);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn token_healthy_receipt_counts_concurrent_source_calls_without_refresh() {
    let receipt = tokens::run(Case::TokenHealthy, 1, 4).await.unwrap();
    assert_eq!(receipt.evidence["replacements"], 0);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn token_expiring_receipt_counts_one_refresh_for_concurrent_callers() {
    let receipt = tokens::run(Case::TokenExpiring, 1, 4).await.unwrap();
    assert_eq!(receipt.evidence["replacements"], 1);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn token_rejected_receipt_counts_one_refresh_for_concurrent_rejections() {
    let receipt = tokens::run(Case::TokenRejected, 1, 4).await.unwrap();
    assert_eq!(receipt.evidence["replacements"], 1);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
fn args(case: Case, dir: &std::path::Path) -> Args {
    Args {
        case,
        out: dir.join("receipt.json"),
        store_dir: dir.join("store"),
        operations: 1,
        callers: 2,
        postgres_url: None,
        workload: "smoke-v1".into(),
    }
}
#[tokio::test]
async fn root_redrive_receipt_requires_a_park_and_explicit_operator_mail() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = run(&args(Case::RootRedrive, dir.path())).await.unwrap();
    assert_eq!(receipt.evidence["explicit_redrives"], 1);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn parked_call_takeover_receipt_preserves_the_pinned_identity() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = run(&args(Case::ParkedTakeover, dir.path())).await.unwrap();
    assert_eq!(receipt.evidence["stable_call_keys"], 1);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn typed_history_receipt_reads_and_folds_every_committed_turn_once() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = run(&args(Case::TypedHistory, dir.path())).await.unwrap();
    assert_eq!(receipt.evidence["turns"], 1);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn sustained_lifecycle_receipt_counts_distinct_processes_and_terminals() {
    let dir = tempfile::tempdir().unwrap();
    let mut args = args(Case::ProcessLifecycle, dir.path());
    args.operations = 3;
    let receipt = run(&args).await.unwrap();
    assert_eq!(receipt.evidence["terminal"], 3);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
#[tokio::test]
async fn seeded_plan_receipt_runs_generated_cells_on_the_durable_node() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = run(&args(Case::SeededPlan, dir.path())).await.unwrap();
    assert_eq!(receipt.evidence["primary_turns"], 2);
    eprintln!("{}", serde_json::to_string(&receipt).unwrap());
}
/// FIG-5637: every writer is a node of its own. Under one shared node name
/// the writer that booted last fenced the others, and their sends settled
/// only while it was still there to run them.
#[tokio::test]
async fn sqlite_writer_settles_its_send_after_a_later_booted_writer_has_left() {
    let dir = tempfile::tempdir().unwrap();
    lash_sqlite_store::SqliteStoreSet::open(
        dir.path().join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .unwrap();
    let first = workers::Writer::open(dir.path(), 0).await.unwrap();
    let second = workers::Writer::open(dir.path(), 1).await.unwrap();
    let left = second.finish(1).await.unwrap();
    assert_eq!(left.count("send.settle"), 1);
    let stayed = first.finish(1).await.unwrap();
    assert_eq!(stayed.count("send.settle"), 1);
}
