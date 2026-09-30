//! Durable attempt and measured CPU admission across parent loss.
#![expect(
    clippy::expect_used,
    reason = "conformance law fixtures require successful admission and settlement"
)]
use lash_core::store::worker_recovery::*;
use std::sync::Arc;

fn limits() -> WorkerRecoveryLimits {
    WorkerRecoveryLimits {
        max_attempts: 3,
        max_cpu_nanos: 1_000_000,
    }
}

pub async fn parent_loss_consumes_unknown_cpu_and_redrive_resumes(
    store: Arc<dyn WorkerRecoveryStore>,
) {
    let first = store
        .reserve("interrupted", limits())
        .await
        .expect("admit first attempt");
    assert_eq!(first.baseline.attempts, 1);
    store.mark_running(&first).await.expect("worker started");
    let redrive = store
        .reserve("interrupted", limits())
        .await
        .expect("substrate redrive resumes");
    assert_eq!(redrive.baseline.attempts, 2);
    assert_eq!(redrive.baseline.unknown_cpu_attempts, 1);
    assert_eq!(
        redrive.baseline.cpu_nanos, 0,
        "unknown CPU must never be invented"
    );
    store
        .settle(
            &redrive,
            WorkerRecoveryTotals {
                cpu_nanos: 37,
                ..redrive.baseline
            },
        )
        .await
        .expect("settle measured replacement CPU");
    let next = store
        .reserve("interrupted", limits())
        .await
        .expect("open settled execution");
    assert_eq!(next.baseline.cpu_nanos, 37);
    assert_eq!(next.baseline.attempts, 2);
    assert_eq!(next.baseline.unknown_cpu_attempts, 1);
}

pub async fn repeated_parent_loss_exhausts_worker_attempts(store: Arc<dyn WorkerRecoveryStore>) {
    for attempt in 1..=3 {
        let claim = store
            .reserve("repeated-loss", limits())
            .await
            .expect("bounded replacement admitted");
        assert_eq!(claim.baseline.attempts, attempt);
        assert_eq!(claim.baseline.unknown_cpu_attempts, attempt - 1);
        assert_eq!(claim.baseline.cpu_nanos, 0);
        store.mark_running(&claim).await.expect("worker started");
    }
    assert!(
        matches!(
            store.reserve("repeated-loss", limits()).await,
            Err(WorkerRecoveryError::AttemptsExhausted)
        ),
        "repeated parent loss ends with typed attempt exhaustion"
    );
}

pub async fn recovery_totals_are_monotone_and_stale_parents_are_fenced(
    store: Arc<dyn WorkerRecoveryStore>,
) {
    let claim = store.reserve("monotone", limits()).await.expect("admit");
    store
        .settle(
            &claim,
            WorkerRecoveryTotals {
                cpu_nanos: 70,
                ..claim.baseline
            },
        )
        .await
        .expect("settle first checkout");
    assert!(
        store
            .settle(
                &claim,
                WorkerRecoveryTotals {
                    cpu_nanos: 60,
                    ..claim.baseline
                }
            )
            .await
            .is_err(),
        "an earlier checkpoint cannot refund CPU"
    );
    store
        .mark_running(&claim)
        .await
        .expect("next checkout started");
    let redrive = store
        .reserve("monotone", limits())
        .await
        .expect("redrive preserves known CPU");
    assert_eq!(redrive.baseline.cpu_nanos, 70);
    assert_eq!(redrive.baseline.attempts, 2);
    assert_eq!(redrive.baseline.unknown_cpu_attempts, 1);
    assert!(matches!(
        store.mark_running(&claim).await,
        Err(WorkerRecoveryError::Fenced)
    ));
    assert!(matches!(
        store
            .settle(
                &claim,
                WorkerRecoveryTotals {
                    cpu_nanos: 80,
                    ..claim.baseline
                }
            )
            .await,
        Err(WorkerRecoveryError::Fenced)
    ));
    store
        .settle(
            &redrive,
            WorkerRecoveryTotals {
                cpu_nanos: limits().max_cpu_nanos,
                ..redrive.baseline
            },
        )
        .await
        .expect("settle cap");
    assert!(matches!(
        store.reserve("monotone", limits()).await,
        Err(WorkerRecoveryError::CpuExhausted)
    ));
}

pub async fn parked_worker_handover_preserves_attempt(store: Arc<dyn WorkerRecoveryStore>) {
    let first = store.reserve("parked", limits()).await.expect("admit");
    assert_eq!(first.baseline.attempts, 1);
    let first = store
        .reserve("parked", limits())
        .await
        .expect("resume before launch");
    assert_eq!(
        first.baseline.attempts, 1,
        "a suspension before worker launch consumes no replacement"
    );
    assert_eq!(first.baseline.unknown_cpu_attempts, 0);
    store.mark_running(&first).await.expect("worker started");
    store
        .settle(
            &first,
            WorkerRecoveryTotals {
                cpu_nanos: 40,
                ..first.baseline
            },
        )
        .await
        .expect("release before parent callback");
    let resumed = store
        .reserve("parked", limits())
        .await
        .expect("segment handover admitted");
    assert_eq!(resumed.baseline.attempts, first.baseline.attempts);
    assert_eq!(resumed.baseline.cpu_nanos, 40);
    assert_eq!(resumed.baseline.unknown_cpu_attempts, 0);
}

#[macro_export]
macro_rules! worker_recovery_tests {
    ($fixture:block) => {
        #[tokio::test]
        async fn parent_loss_consumes_unknown_cpu_and_redrive_resumes() {
            let (_held, store) = $fixture;
            $crate::parent_loss_consumes_unknown_cpu_and_redrive_resumes(store).await;
        }
        #[tokio::test]
        async fn repeated_parent_loss_exhausts_worker_attempts() {
            let (_held, store) = $fixture;
            $crate::repeated_parent_loss_exhausts_worker_attempts(store).await;
        }
        #[tokio::test]
        async fn recovery_totals_are_monotone_and_stale_parents_are_fenced() {
            let (_held, store) = $fixture;
            $crate::recovery_totals_are_monotone_and_stale_parents_are_fenced(store).await;
        }
        #[tokio::test]
        async fn parked_worker_handover_preserves_attempt() {
            let (_held, store) = $fixture;
            $crate::parked_worker_handover_preserves_attempt(store).await;
        }
    };
}
