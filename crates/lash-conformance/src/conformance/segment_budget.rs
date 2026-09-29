//! Segmentation must preserve the process's result across durable waits.

/// What a process that sleeps, executes an effect, waits for a signal, then
/// executes another effect reports after its deployment is reconstructed.
pub struct SegmentBudgetObservation {
    pub output: serde_json::Value,
    pub segments: usize,
    pub continuation_bytes_at_wait: usize,
}

/// The tier runs a real process with the selected effect budget. Its run
/// checks pending-wait placement, recorded-effect reuse and terminal delivery.
#[async_trait::async_trait]
pub trait SegmentBudgetHarness: Send + Sync {
    async fn run(&self, budget: Option<u64>, live_bytes: usize) -> SegmentBudgetObservation;
}

/// A segmentation cut changes the journal partition, while preserving the
/// program result and the live value carried through its continuation.
pub async fn segment_budget_and_continuation_preserve_results_across_waits(
    harness: &impl SegmentBudgetHarness,
) {
    let baseline = harness.run(None, 32).await;
    assert_eq!(
        baseline.segments, 1,
        "the short baseline needs no budget cut"
    );
    for budget in [1, 2] {
        let segmented = harness.run(Some(budget), 32).await;
        assert_eq!(segmented.output, baseline.output, "budget {budget}");
        assert!(
            segmented.segments > 1,
            "the budget forces a real engine handover"
        );
        assert!(
            segmented.continuation_bytes_at_wait > 0,
            "the successor carries a serialized VM continuation"
        );
    }
    let small = harness.run(Some(1), 16).await;
    let large = harness.run(Some(1), 128 * 1024).await;
    assert!(
        large.continuation_bytes_at_wait > small.continuation_bytes_at_wait + 64 * 1024,
        "continuation cost follows live program data, without a universal byte cap"
    );
}
