//! Exact successful commit evidence, separate from the session's write counter.

use std::sync::{Arc, Mutex};

use pretty_assertions::assert_eq;

#[derive(Debug)]
struct Commit {
    operation: crate::store::OperationId,
    revision: u64,
}

pub(crate) struct CommitReceipts {
    inner: Arc<dyn crate::RuntimeStore>,
    commits: Mutex<Vec<Commit>>,
}

impl CommitReceipts {
    pub(crate) fn new(inner: Arc<dyn crate::RuntimeStore>) -> Self {
        Self {
            inner,
            commits: Mutex::new(Vec::new()),
        }
    }

    /// Every write in the interval has exactly one successful operation receipt;
    /// retries return that receipt and cannot increase any kind's count.
    pub(crate) fn assert_since(
        &self,
        before: u64,
        after: u64,
        pressure_frames: usize,
        turns: usize,
        command_frames: usize,
        transitions: usize,
    ) {
        let commits = self
            .commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let interval: Vec<_> = commits
            .iter()
            .filter(|commit| commit.revision > before && commit.revision <= after)
            .collect();
        let mut counts = [0; 4];
        for commit in &interval {
            let kind = match commit.operation.key.as_str() {
                "frame-commit" => 0,
                "final" => 1,
                "session-command" | "append-session-nodes" => 2,
                key if key.starts_with("plugin-transition:") => 3,
                other => panic!("unexpected frame-law commit kind {other}: {commit:?}"),
            };
            counts[kind] += 1;
        }
        assert_eq!(
            counts,
            [pressure_frames, turns, command_frames, transitions],
            "exact fresh receipts for pressure frames, turns, command frames and plugin transitions: {interval:?}"
        );
        let mut revisions: Vec<_> = interval.iter().map(|commit| commit.revision).collect();
        revisions.sort_unstable();
        assert_eq!(
            revisions,
            (before + 1..=after).collect::<Vec<_>>(),
            "every head write has exactly one observed fresh receipt"
        );
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for CommitReceipts {
    type Inner = dyn crate::RuntimeStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
        let operation = commit.turn_commit.operation.clone();
        let receipt = self.inner.commit_runtime_state(commit).await?;
        let mut commits = self
            .commits
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if receipt.receipt_replayed {
            let original = commits.iter().find(|commit| commit.operation == operation);
            assert!(
                original.is_some_and(|commit| commit.revision == receipt.head_revision),
                "replayed operation returns its original fresh receipt: {operation:?}, {receipt:?}, {original:?}"
            );
        } else {
            assert!(
                !commits.iter().any(|commit| commit.operation == operation),
                "an operation commits fresh exactly once: {operation:?}"
            );
            commits.push(Commit {
                operation,
                revision: receipt.head_revision,
            });
        }
        Ok(receipt)
    }
}
