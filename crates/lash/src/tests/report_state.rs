//! A turn's report carries the session's state after its run. The turn runs
//! on the session actor, so a host's open session holds only the state it
//! opened with; the report is rebuilt from the committed head.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reports_state_is_the_session_after_its_run() -> Result<()> {
    let core = standard_core_over(sqlite_memory_store_backend().await);
    let session = core
        .session(crate::SessionId::parse("report-state").expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;

    let first = session.send(TurnInput::text("one")).output().await?;
    let second = session.send(TurnInput::text("two")).output().await?;

    assert_eq!(first.result.state.turn_index, 1, "the first run's state");
    assert_eq!(
        second.result.state.turn_index,
        committed(&session).await.turn_index(),
        "the second report's state is the committed head after its run"
    );
    assert_eq!(second.result.state.turn_index, 2);
    Ok(())
}
