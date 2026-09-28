mod tests {
    //! The `ProcessStart` relay law (ADR 0109 §1.5): a failing start
    //! delivery is retried with its `last_error` recorded on the armed row,
    //! and stalls typed once the kind's attempt ceiling is spent; a
    //! delivered one settles and is never claimed again.

    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::runtime::drive::relay::{RelayVerdict, relay_due};
    use crate::runtime::process_start::ProcessStartRelay;
    use crate::store::{ObligationKind, ObligationState, StallReason, process_start_obligation_id};
    use crate::testing::TestClock;
    use crate::{
        ClockWallTime as _, PluginError, ProcessRecord, ProcessRegistry, ProcessWorkSubstrate,
        StoreSet as _,
    };

    /// A process-work port whose start delivery always fails retryably.
    struct FailingStarts {
        deliveries: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProcessWorkSubstrate for FailingStarts {
        async fn deliver_process_start(&self, _record: &ProcessRecord) -> Result<(), PluginError> {
            self.deliveries.fetch_add(1, Ordering::SeqCst);
            Err(PluginError::Invoke("the engine's ingress is down".into()))
        }

        async fn await_process_terminal(
            &self,
            process_id: &crate::ProcessId,
        ) -> Result<crate::ProcessTerminalWait, PluginError> {
            panic!("unexpected terminal wait for {process_id}")
        }

        async fn deliver_cancel(
            &self,
            _process_id: &crate::ProcessId,
            _request: &crate::CancelRequest,
            _key: &str,
        ) -> Result<(), PluginError> {
            unreachable!("start witness does not deliver cancels")
        }

        async fn publish_process_terminal(
            &self,
            process_id: &crate::ProcessId,
            _output: &crate::ProcessAwaitOutput,
            _key: &str,
        ) -> Result<(), PluginError> {
            panic!("unexpected terminal publication for {process_id}")
        }
    }

    /// A process-work port whose start delivery always lands.
    struct DeliveringStarts {
        deliveries: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProcessWorkSubstrate for DeliveringStarts {
        async fn deliver_process_start(&self, _record: &ProcessRecord) -> Result<(), PluginError> {
            self.deliveries.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn await_process_terminal(
            &self,
            process_id: &crate::ProcessId,
        ) -> Result<crate::ProcessTerminalWait, PluginError> {
            panic!("unexpected terminal wait for {process_id}")
        }

        async fn deliver_cancel(
            &self,
            _process_id: &crate::ProcessId,
            _request: &crate::CancelRequest,
            _key: &str,
        ) -> Result<(), PluginError> {
            unreachable!("start witness does not deliver cancels")
        }

        async fn publish_process_terminal(
            &self,
            process_id: &crate::ProcessId,
            _output: &crate::ProcessAwaitOutput,
            _key: &str,
        ) -> Result<(), PluginError> {
            panic!("unexpected terminal publication for {process_id}")
        }
    }

    fn registration() -> crate::ProcessRegistration {
        crate::ProcessRegistration::new(
            crate::ProcessInput::Engine {
                kind: "test-engine".to_string(),
                payload: serde_json::Value::Null,
            },
            crate::ProcessProvenance::host(),
            crate::Lifetime::Detached,
        )
        .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new("process-env:test")))
    }

    /// A failing submit retires no row: the obligation stays `due` with its
    /// attempt counted and its `last_error` written, the reconcile pass
    /// retries it, and the attempt that reaches the ceiling stalls it typed —
    /// `AttemptsExhausted`, with the last delivery's error as its `last_error`
    /// — where an operator's `list_stalled` and re-arm find it.
    #[tokio::test]
    async fn a_failing_start_retries_with_its_last_error_and_stalls_typed_at_the_ceiling() {
        let stores = crate::support::memory_store_set().await;
        let registry: Arc<dyn ProcessRegistry> = stores.process_registry();
        let ledger = stores.obligation_ledger(ObligationKind::ProcessStart);
        let clock = Arc::new(TestClock::new(1_000));
        let port = Arc::new(FailingStarts {
            deliveries: AtomicUsize::new(0),
        });
        let relay = ProcessStartRelay::new(
            Arc::clone(&ledger),
            Arc::clone(&registry),
            port,
            Arc::clone(&clock) as Arc<dyn crate::Clock>,
        );
        let process_id = registry
            .register_process(registration())
            .await
            .expect("register the process")
            .id;
        let obligation = process_start_obligation_id(&process_id);

        // The producer's own-commit attempt fails retryably and settles the
        // failure onto the row rather than returning it.
        let verdict = relay.deliver_start(&process_id).await.expect("the settle");
        assert!(
            matches!(verdict, RelayVerdict::Retried { .. }),
            "a retryable failure is retried: {verdict:?}"
        );
        let standing = ledger
            .standing(&obligation)
            .await
            .expect("read the obligation")
            .expect("the obligation stands");
        assert_eq!(
            (standing.state, standing.attempts),
            (ObligationState::Due, 1),
            "the failed attempt is counted and the row stays due"
        );

        // The reconcile pass retries it once the backoff lapses.
        let pass = relay_due(&relay, clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("a due pass before the backoff");
        assert_eq!(pass.claimed, 0, "the backoff holds the row: {pass:?}");
        clock.advance(2_000);
        let pass = relay_due(&relay, clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("the reconcile pass retries the row");
        assert_eq!(
            (pass.claimed, pass.retried),
            (1, 1),
            "the reconcile's retry lands on the row: {pass:?}"
        );

        // The producer's own-claim entry point does not wait out a backoff:
        // run it until the attempt ceiling stalls the row.
        let mut last = verdict;
        for _ in 0..32 {
            last = relay.deliver_start(&process_id).await.expect("the settle");
            if matches!(last, RelayVerdict::Stalled(_) | RelayVerdict::NotDue) {
                break;
            }
        }
        assert_eq!(
            last,
            RelayVerdict::Stalled(StallReason::AttemptsExhausted),
            "the ceiling stalls the row typed"
        );

        let stalled = ledger
            .list_stalled(None, NonZeroUsize::MIN)
            .await
            .expect("list stalled obligations");
        assert_eq!(stalled.len(), 1, "the one start stalls: {stalled:?}");
        let stalled = &stalled[0];
        assert_eq!(stalled.id, obligation);
        assert_eq!(stalled.reason, StallReason::AttemptsExhausted);
        assert_eq!(
            stalled.attempts,
            u32::from(crate::runtime::drive::relay::RelayPolicy::default().attempt_ceiling),
            "the ceiling's attempts are counted"
        );
        assert_eq!(
            stalled.last_error.as_deref(),
            Some("plugin invoke error: the engine's ingress is down"),
            "the row carries the last delivery's error for the operator"
        );

        // A stalled row is never retried: neither entry point claims it.
        let pass = relay_due(&relay, clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("a due pass over a stalled row");
        assert_eq!(pass.claimed, 0);
        assert_eq!(
            relay.deliver_start(&process_id).await.expect("the settle"),
            RelayVerdict::NotDue,
            "a stalled obligation is not claimed again"
        );

        // Re-armed by the operator, the row is due again and the relay —
        // still failing — attempts it once more.
        assert!(
            ledger
                .rearm(&obligation, clock.timestamp_ms())
                .await
                .expect("rearm"),
            "the stalled row re-arms"
        );
        clock.advance(2_000);
        let pass = relay_due(&relay, clock.as_ref(), NonZeroUsize::MIN)
            .await
            .expect("the reconcile after the re-arm");
        assert_eq!((pass.claimed, pass.retried), (1, 1));
    }

    /// The happy path of the one delivery path: the armed row delivers at
    /// once, settles `delivered`, and is never claimed again.
    #[tokio::test]
    async fn a_delivered_start_settles_and_is_never_claimed_again() {
        let stores = crate::support::memory_store_set().await;
        let registry: Arc<dyn ProcessRegistry> = stores.process_registry();
        let ledger = stores.obligation_ledger(ObligationKind::ProcessStart);
        let port = Arc::new(DeliveringStarts {
            deliveries: AtomicUsize::new(0),
        });
        let relay = ProcessStartRelay::new(
            Arc::clone(&ledger),
            Arc::clone(&registry),
            Arc::clone(&port) as Arc<dyn ProcessWorkSubstrate>,
            stores.clock(),
        );
        let process_id = registry
            .register_process(registration())
            .await
            .expect("register the process")
            .id;

        assert_eq!(
            relay.deliver_start(&process_id).await.expect("the settle"),
            RelayVerdict::Delivered,
        );
        assert_eq!(port.deliveries.load(Ordering::SeqCst), 1);
        assert_eq!(
            ledger
                .state(&process_start_obligation_id(&process_id))
                .await
                .expect("read the obligation"),
            Some(ObligationState::Delivered),
        );
        assert_eq!(
            relay.deliver_start(&process_id).await.expect("the settle"),
            RelayVerdict::NotDue,
            "a delivered start is never delivered again"
        );
        assert_eq!(port.deliveries.load(Ordering::SeqCst), 1);
    }
}
