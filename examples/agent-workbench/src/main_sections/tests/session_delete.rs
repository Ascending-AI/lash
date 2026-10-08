use super::session_delete_faults::{CloseFault, SessionDeleteFaults};
use super::*;
use lash::SessionId;

async fn deleting_workbench() -> (Workbench, SessionId, Arc<SessionDeleteFaults>) {
    let sessions = WorkbenchSessions::fresh();
    let session_id = sessions.current();
    let faults = SessionDeleteFaults::new(session_id.clone());
    let workbench = Workbench::builder(silent_provider())
        .sessions(sessions)
        .session_delete_faults(Arc::clone(&faults))
        .build()
        .await;
    workbench
        .state
        .create_or_open_session(&session_id, "test")
        .await
        .expect("materialize the session before deleting it");
    (workbench, session_id, faults)
}

async fn reset(state: &AppState, session: &SessionId) -> Result<Json<StateReadSnapshot>, AppError> {
    Box::pin(reset_chat(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session.clone()),
        }),
    ))
    .await
}

// A refused close with a provably live catalog lifts the fence: the roster
// retains the id and the route admits work on it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_close_lifts_retirement_when_the_session_is_live() {
    let (workbench, session_id, faults) = deleting_workbench().await;
    let state = &workbench.state;
    let roster = serde_json::to_value(state.sessions.list()).expect("the roster encodes");
    faults.refuse_close(CloseFault::Refuse);

    let error = reset(state, &session_id)
        .await
        .expect_err("the close is refused");

    assert!(
        faults.refused_close(),
        "the store refused the close-mail append"
    );
    assert_eq!(error.status, StatusCode::CONFLICT);
    assert_eq!(error.verdict, AppErrorVerdict::Terminal);
    assert_eq!(state.active_turns.retirement(&session_id), None);
    assert_eq!(state.sessions.current(), session_id);
    assert_eq!(
        serde_json::to_value(state.sessions.list()).expect("the roster encodes"),
        roster
    );
    assert!(matches!(
        state
            .session_store_factory
            .lookup_session(&session_id)
            .await
            .expect("read the durable live fact"),
        lash::persistence::SessionLookup::Live(_)
    ));
    state
        .admit_live_session(&session_id, "test.after-refused-close")
        .await
        .expect("the old session remains usable");
    // The one-shot refusal requested no close; a new request can delete it.
    let Json(replacement) = reset(state, &session_id)
        .await
        .expect("retry the refused close");
    assert_ne!(replacement.settings.session_id, session_id);
    workbench.shutdown().await;
}

// A close can commit and lose its acknowledgement. The failed call follows
// the durable tombstone, confirms the fence and gives the page a replacement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_close_confirms_retirement_when_a_tombstone_committed() {
    let (workbench, session_id, faults) = deleting_workbench().await;
    let state = &workbench.state;
    faults.refuse_close(CloseFault::CommitThenRefuse);

    let Json(snapshot) = reset(state, &session_id)
        .await
        .expect("a committed tombstone settles the failed close call");

    assert!(
        faults.refused_close(),
        "the close committed but its acknowledgement was refused"
    );
    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retired)
    );
    assert_ne!(snapshot.settings.session_id, session_id);
    assert_eq!(state.sessions.current(), snapshot.settings.session_id);
    assert!(state.sessions.entry(&session_id).is_none());
    reset_chat_tests::assert_tombstoned(state, &session_id).await;
    let Json(retried) = reset(state, &session_id)
        .await
        .expect("retry the lost answer");
    assert_eq!(
        retried.settings.session_id, snapshot.settings.session_id,
        "a retry must not rotate the roster twice"
    );
    workbench.shutdown().await;
}

// A timed-out observation proves neither liveness nor deletion. Keep the
// fence and roster, answer 503 Ambiguous, and let a retry settle the tombstone.
#[tokio::test]
async fn an_unconfirmed_tombstone_is_ambiguous_and_a_retry_completes_the_delete() {
    let (workbench, session_id, faults) = deleting_workbench().await;
    let state = &workbench.state;
    let roster = serde_json::to_value(state.sessions.list()).expect("the roster encodes");
    faults.withhold_tombstone();
    let deleting = tokio::spawn({
        let state = state.clone();
        let session_id = session_id.clone();
        async move { reset(&state, &session_id).await }
    });
    tokio::time::timeout(Duration::from_secs(30), faults.wait_for_tombstone())
        .await
        .expect("the real actor commits its tombstone");
    tokio::time::timeout(Duration::from_secs(30), faults.wait_for_hidden_read())
        .await
        .expect("the route reaches an unavailable tombstone read");
    // Advance the actual route deadline only after the observed store events.
    // No wall-clock sleep or test-only engine hook drives the close.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    let error = deleting
        .await
        .expect("the reset task returns")
        .expect_err("the tombstone was not confirmed by the deadline");
    tokio::time::resume();

    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.verdict, AppErrorVerdict::Ambiguous);
    assert!(!error.message.contains("remains live"));
    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retiring)
    );
    assert_eq!(state.sessions.current(), session_id);
    assert_eq!(
        serde_json::to_value(state.sessions.list()).expect("the roster encodes"),
        roster
    );
    let refused = state
        .admit_session_id(&session_id, "test.unconfirmed")
        .await
        .expect_err("an ambiguous close must keep refusing new work");
    assert_eq!(
        refused
            .retirement
            .expect("the refusal names its fence")
            .retirement,
        SessionRetirement::Retiring
    );

    faults.reveal_tombstone();
    let Json(snapshot) = reset(state, &session_id)
        .await
        .expect("retry confirms deletion");
    assert_ne!(snapshot.settings.session_id, session_id);
    assert_eq!(state.sessions.current(), snapshot.settings.session_id);
    assert_eq!(
        state.active_turns.retirement(&session_id),
        Some(SessionRetirement::Retired)
    );
    reset_chat_tests::assert_tombstoned(state, &session_id).await;
    workbench.shutdown().await;
}
