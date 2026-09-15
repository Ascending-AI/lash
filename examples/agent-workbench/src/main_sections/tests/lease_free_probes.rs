use super::*;

/// Collects the workbench's own trace events so a test can assert what a
/// request did, not only what it returned. The payload rides along because a
/// contention record names the caller that contended (FIG-3151), and a test
/// that only counted them could not tell which route produced them.
#[derive(Default)]
struct RecordedTraceNames {
    names: Mutex<Vec<(String, Value)>>,
}

impl RecordedTraceNames {
    /// The name a workbench trace event is recorded under.
    ///
    /// `emit_workbench_trace` namespaces every event it emits, so the recorded
    /// name is `agent_workbench.session.open.contended`, not the bare
    /// `session.open.contended` the emitting code passes. Matching the bare
    /// name here counted zero of everything and made every assertion below
    /// vacuously true (FIG-3151); the count is built from the same name the
    /// sink sees.
    fn recorded_name(name: &str) -> String {
        format!("agent_workbench.{name}")
    }

    fn count(&self, name: &str) -> usize {
        let name = Self::recorded_name(name);
        self.names
            .lock_recover()
            .iter()
            .filter(|(recorded, _)| recorded == &name)
            .count()
    }

    /// Every `surface` recorded on the named event, in order.
    fn surfaces(&self, name: &str) -> Vec<String> {
        let name = Self::recorded_name(name);
        self.names
            .lock_recover()
            .iter()
            .filter(|(recorded, _)| recorded == &name)
            .map(|(_, payload)| {
                payload
                    .get("surface")
                    .and_then(Value::as_str)
                    .unwrap_or("<unattributed>")
                    .to_string()
            })
            .collect()
    }
}

impl TraceSink for RecordedTraceNames {
    fn append(&self, record: &TraceRecord) -> Result<(), lash::tracing::TraceSinkError> {
        if let TraceEvent::Custom { name, payload } = &record.event {
            self.names
                .lock_recover()
                .push((name.clone(), payload.clone()));
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
        .open_session(&session_id, "test")
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

/// `/api/observations` is the page's live turn stream, and the page reconnects
/// it every 900 ms for as long as it is refused.
///
/// Attaching it opened the session, and the open claimed the execution lease to
/// admit the state it loaded — the one thing a running turn holds for its whole
/// length. So every reconnect during a turn burnt the six-attempt budget and
/// answered 503, and the reconnect loop turned that into a storm: 156
/// `session.open.contended` and 27 `session.open.retry_exhausted` records in one
/// ~25 s turn on the judged `rlm-cell-boundary` run, each exhaustion a red
/// banner. Here the lane is held for the whole test, as it is for the length of
/// a turn, and the attach still answers without contending (FIG-3151).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_observation_stream_attaches_while_the_session_execution_lane_is_held() {
    let data_dir = tempfile::tempdir().expect("lease-free observation tempdir");
    let mut state = recoverable_chat_test_state(data_dir.path(), 16).await;
    let traces = Arc::new(RecordedTraceNames::default());
    state.trace_sink = Some(Arc::clone(&traces) as Arc<dyn TraceSink>);
    let session_id = state.current_session_id();

    // Materialize the session so the attach reads a real durable head, then
    // take the execution lane away from it for the rest of the test.
    let session = state
        .open_session(&session_id, "test")
        .await
        .expect("open the observed session once");
    session.close().await.expect("close the observed session");

    let store = state
        .session_store_factory
        .create_store(&state_store_request(&state, &session_id))
        .await
        .expect("create the observed session's store");
    let owner =
        lash::persistence::LeaseOwnerIdentity::opaque("fig3151-turn-holder", "incarnation-1");
    let claim = store
        .try_claim_session_execution_lease(&session_id, &owner, "fig3151-executor", 60_000)
        .await
        .expect("claim the session execution lane");
    let holder = match claim {
        lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(acquisition) => acquisition,
        lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { holder } => {
            panic!("the lane was already held by {holder:?}")
        }
    };

    // The page's reconnect loop, at its own cadence, against a held lane.
    for attach in 0..6 {
        drop(
            session_observations(
                State(state.clone()),
                Query(EventsQuery {
                    cursor: None,
                    session_id: Some(session_id.clone()),
                }),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("observation attach {attach} was refused by a held lane: {error:?}")
            }),
        );
    }

    assert_eq!(
        traces.count("session.open.retry_exhausted"),
        0,
        "an observation attach exhausted the session-open retry budget, which is the 503 the page renders"
    );
    assert_eq!(
        traces.count("session.open.contended"),
        0,
        "an observation attach contended for the session execution lane"
    );

    // The precondition: the lane really is held, and an open that does claim it
    // still exhausts against it. Without this the assertions above would pass on
    // a test that simply never held the lane.
    assert!(
        state.open_session(&session_id, "api.turn").await.is_err(),
        "a lease-taking open must still be refused by a held lane"
    );
    assert!(
        traces.count("session.open.contended") > 0,
        "the held lane must still contend a lease-taking open"
    );
    assert_eq!(
        traces.surfaces("session.open.retry_exhausted"),
        vec!["api.turn".to_string()],
        "a contention record must name the caller that contended"
    );

    store
        .release_session_execution_lease(&holder.lease.fence())
        .await
        .expect("release the session execution lane");
}
