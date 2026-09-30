use super::{Duration, Rendezvous};
use std::future::Future;

pub(super) async fn run_with_activation_budget<F: Future>(
    turn: F,
    rendezvous: &Rendezvous,
    budget: Duration,
) -> Option<F::Output> {
    tokio::pin!(turn);
    let mut progress = rendezvous.notify.subscribe();
    loop {
        let started = rendezvous.started_count();
        tokio::select! {
            output = &mut turn => return Some(output),
            _ = progress.changed() => {},
            () = tokio::time::sleep(budget) => {
                if rendezvous.expire_if_activation_stalled(started, budget) {
                    return None;
                }
            }
        }
        // Journaled turns return Pending to let their handler suspend or fail.
        // Propagate that boundary before a progress wake can poll them again.
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_secs(60);

    fn rendezvous() -> Rendezvous {
        Rendezvous::new(vec!["first".to_string(), "second".to_string()], true)
    }

    #[tokio::test(start_paused = true)]
    async fn continuing_activation_outlasts_one_budget() {
        let rendezvous = rendezvous();
        let turn = async {
            tokio::time::sleep(Duration::from_secs(40)).await;
            rendezvous.record_started("first");
            tokio::time::sleep(Duration::from_secs(40)).await;
            rendezvous.record_started("second");
            rendezvous.wait_for(&rendezvous.expected).await;
            rendezvous.record_answered("first");
            rendezvous.record_answered("second");
            "settled"
        };
        assert_eq!(
            run_with_activation_budget(turn, &rendezvous, BUDGET).await,
            Some("settled"),
        );
        assert!(rendezvous.expired().is_none());
        assert_eq!(rendezvous.peak_in_flight(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn activated_members_can_settle_after_one_budget() {
        let rendezvous = rendezvous();
        let turn = async {
            rendezvous.record_started("first");
            rendezvous.record_started("second");
            rendezvous.wait_for(&rendezvous.expected).await;
            tokio::time::sleep(Duration::from_secs(80)).await;
            rendezvous.record_answered("first");
            rendezvous.record_answered("second");
            "settled"
        };
        assert_eq!(
            run_with_activation_budget(turn, &rendezvous, BUDGET).await,
            Some("settled"),
        );
        assert!(rendezvous.expired().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_activation_names_missing_members_and_releases_waiters() {
        let rendezvous = rendezvous();
        let started = tokio::time::Instant::now();
        let turn = async {
            rendezvous.record_started("first");
            rendezvous.wait_for(&rendezvous.expected).await;
        };
        assert!(
            run_with_activation_budget(turn, &rendezvous, BUDGET)
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), BUDGET);
        assert_eq!(rendezvous.never_started(), vec!["second"]);
        rendezvous.wait_for(&rendezvous.expected).await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_that_never_activates_expires() {
        let rendezvous = rendezvous();
        let started = tokio::time::Instant::now();
        assert!(
            run_with_activation_budget(std::future::pending::<()>(), &rendezvous, BUDGET)
                .await
                .is_none()
        );
        assert_eq!(started.elapsed(), BUDGET);
        assert_eq!(rendezvous.never_started(), vec!["first", "second"]);
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_activation_budget_drives_the_released_turn() {
        let rendezvous = rendezvous();
        rendezvous.record_started("first");
        assert!(rendezvous.expire_if_activation_stalled(1, BUDGET));
        let turn = async {
            rendezvous.record_started("second");
            rendezvous.wait_for(&rendezvous.expected).await;
            tokio::time::sleep(Duration::from_secs(80)).await;
            rendezvous.record_answered("first");
            rendezvous.record_answered("second");
            "drained"
        };
        assert_eq!(
            run_with_activation_budget(turn, &rendezvous, BUDGET).await,
            Some("drained"),
        );
        assert_eq!(rendezvous.never_started(), vec!["second"]);
    }

    #[tokio::test]
    async fn progress_wakes_yield_before_repolling_a_pending_turn() {
        // A journaled turn can return Pending after failing its handler. Its
        // owner must observe that Pending before another poll of the turn.
        for _ in 0..64 {
            let rendezvous = rendezvous();
            let polls = std::cell::Cell::new(0);
            let turn = std::future::poll_fn(|_| {
                let count = polls.get() + 1;
                polls.set(count);
                if count == 1 {
                    rendezvous.record_started("first");
                    std::task::Poll::Pending
                } else {
                    std::task::Poll::Ready("resumed")
                }
            });
            let budget = run_with_activation_budget(turn, &rendezvous, BUDGET);
            tokio::pin!(budget);
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(budget.as_mut().poll(&mut context).is_pending());
            assert_eq!(polls.get(), 1);
        }
    }
}
