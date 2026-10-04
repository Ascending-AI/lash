//! L15: the census retains native Run receipts and the whole invocation tree.

use super::*;
use lash_core::tool_run::{RunEvent, RunRecord};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l15_census_tracks_native_run_records_and_all_descendants() {
    for width in [1, 2, 16] {
        let receipt = measure(Branch::Done, width, 32, "native-run-census-law")
            .await
            .expect("complete native receipt");
        assert_eq!(receipt.branch_observation["attempts"], width);
        assert_eq!(receipt.branch_observation["incorporated"], width);
        assert_eq!(receipt.branch_observation["boundary_complete"], true);
        assert!(receipt.invocations.iter().all(|row| {
            !row.target_service_name.starts_with("EffectGroup")
                && !row.target_service_name.starts_with("LashToolChild")
        }));

        let records: Vec<RunRecord> = receipt
            .journal
            .iter()
            .filter_map(|entry| {
                let completion = entry.data.run_completion()?.ok()?;
                let value: Value = serde_json::from_slice(&completion).ok()?;
                serde_json::from_value(value.get("record")?.clone()).ok()
            })
            .collect();
        let mut admitted = BTreeSet::new();
        let mut attempts = BTreeSet::new();
        let mut decisions = BTreeSet::new();
        let mut presented = BTreeSet::new();
        let mut incorporated = BTreeSet::new();
        for record in &records {
            for event in &record.events {
                match event {
                    RunEvent::Admitted { round } => {
                        for member in &round.members {
                            assert!(admitted.insert(member.call_id.clone()));
                        }
                    }
                    RunEvent::AttemptRecorded { call_id, .. } => {
                        assert!(attempts.insert(call_id.clone()));
                    }
                    RunEvent::Decided { call_id, .. } => {
                        assert!(decisions.insert(call_id.clone()));
                    }
                    RunEvent::Presented { call_id, .. } => {
                        assert!(presented.insert(call_id.clone()));
                    }
                    RunEvent::Incorporated { call_id } => {
                        assert!(incorporated.insert(call_id.clone()));
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(admitted.len(), width, "the census includes admission");
        assert_eq!(attempts, admitted, "each X keeps its logical call");
        assert_eq!(decisions, admitted, "each D keeps its logical call");
        assert_eq!(presented, admitted, "each V keeps its logical call");
        assert_eq!(
            incorporated, admitted,
            "consumption belongs to the boundary"
        );
        assert_eq!(receipt.engine.total, receipt.journal.len());
        assert_eq!(
            receipt.engine.total,
            receipt
                .invocations
                .iter()
                .map(|row| row.journal_size)
                .sum::<usize>(),
            "the raw census counts every descendant's entire journal"
        );
        assert_eq!(
            receipt.bytes["journal_protobuf_payload"],
            receipt
                .journal
                .iter()
                .map(|row| row.payload_bytes)
                .sum::<usize>()
        );
    }
}
