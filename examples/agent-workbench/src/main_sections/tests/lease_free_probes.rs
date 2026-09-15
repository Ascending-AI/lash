use super::*;

/// Collects the workbench's own trace event names so a test can assert what a
/// request did, not only what it returned.
#[derive(Default)]
struct RecordedTraceNames {
    names: Mutex<Vec<String>>,
}

impl RecordedTraceNames {
    fn count(&self, name: &str) -> usize {
        self.names
            .lock_recover()
            .iter()
            .filter(|recorded| recorded.as_str() == name)
            .count()
    }
}

impl TraceSink for RecordedTraceNames {
    fn append(&self, record: &TraceRecord) -> Result<(), lash::tracing::TraceSinkError> {
        if let TraceEvent::Custom { name, .. } = &record.event {
            self.names.lock_recover().push(name.clone());
        }
        Ok(())
    }
}

/// The page polls `/api/state` and `/api/queued_work` about twice a second and
/// writes nothing, so neither may need the session's execution lease.
///
/// Reading them by opening the session claimed that lease, which is the one
/// thing a running turn must hold. The judged `workbench-continue-as` run
/// recorded 242 `session.open.contended` and 37 `session.open.retry_exhausted`
/// traces across nine turns, and every exhaustion is the 503 that renders as a
/// red banner. Here the lane is held for the whole test — as it is for the
/// length of a turn — and every probe still answers, with no contention to
/// retry against (FIG-3144).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn probes_answer_while_the_session_execution_lane_is_held() {
    let data_dir = tempfile::tempdir().expect("lease-free probe tempdir");
    let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let traces = Arc::new(RecordedTraceNames::default());
    state.trace_sink = Some(Arc::clone(&traces) as Arc<dyn TraceSink>);
    let session_id = state.current_session_id();

    // Materialize the session so the probes read a real durable head, then take
    // the execution lane away from them for the rest of the test.
    let session = state
        .open_session(&session_id)
        .await
        .expect("open the probed session once");
    session.close().await.expect("close the probed session");

    let store = state
        .session_store_factory
        .create_store(&state_store_request(&state, &session_id))
        .await
        .expect("create the probed session's store");
    let owner =
        lash::persistence::LeaseOwnerIdentity::opaque("fig3144-turn-holder", "incarnation-1");
    let claim = store
        .try_claim_session_execution_lease(&session_id, &owner, "fig3144-executor", 60_000)
        .await
        .expect("claim the session execution lane");
    let holder = match claim {
        lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition,
        lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { holder } => {
            panic!("the lane was already held by {holder:?}")
        }
    };

    let probes = (0..12)
        .map(|_| {
            let state = state.clone();
            tokio::spawn(async move {
                let state_probe = Box::pin(app_state(
                    State(state.clone()),
                    Query(SessionQuery::default()),
                ))
                .await
                .map(|Json(snapshot)| snapshot.state.messages.len());
                let queued_work_probe =
                    list_queued_work(State(state), Query(SessionQuery::default()))
                        .await
                        .map(|Json(batches)| batches.len());
                (state_probe, queued_work_probe)
            })
        })
        .collect::<Vec<_>>();
    for probe in probes {
        let (state_probe, queued_work_probe) = probe.await.expect("probe task finished");
        state_probe.expect("a read-only state probe must not be refused by a held lane");
        queued_work_probe
            .expect("a read-only queued-work probe must not be refused by a held lane");
    }

    assert_eq!(
        traces.count("session.open.retry_exhausted"),
        0,
        "a read-only probe exhausted the session-open retry budget, which is the 503 the page renders"
    );
    assert_eq!(
        traces.count("session.open.contended"),
        0,
        "a read-only probe contended for the session execution lane"
    );

    store
        .release_session_execution_lease(&holder.lease.fence())
        .await
        .expect("release the session execution lane");
}
