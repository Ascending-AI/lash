//! Session image recorder and resume oracle.
use super::*;
/// Re-record the session image (`support/images.rs`): node A runs the turn
/// until its cell's `Once` operation's outcome commits, and dies; the store
/// it leaves is the fixture. It holds the turn checkpoint, the cell's
/// snapshot parked on the operation, the operation's run records and its
/// outcome's material.
pub(super) async fn record(protocol: Protocol) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = images::database_path(dir.path());
    let mut first = V0::new(protocol, Dialect::SqliteFile, None);
    first.record_at = Some(path.clone());
    let clock = SimClock::new();
    let database = first.database(Arc::clone(&clock)).await;
    let script = Script::new();
    script.cut_on("a", CommitLabel::ROUND_OUTCOME, 1, Fault::CommitThenAbort);
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        script,
        first.config(),
        first.activation(),
    );
    first.send().await.expect("the turn is sent");
    nodes.start("a");
    nodes.quiesce().await;
    while nodes.script().cuts().is_empty() {
        // The cut kills A, which leaves no timer armed: the step that cuts
        // can be the last.
        assert!(
            !first.done(&nodes).await,
            "{protocol:?} ended before the recording cut: {:?}",
            first.seen.lock_recover().requests
        );
        let stepped = nodes.step().await;
        assert!(
            stepped.is_some() || !nodes.script().cuts().is_empty(),
            "the turn stalled before its operation's outcome committed:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.clock().logical_ms() < 600_000,
            "the operation's outcome did not commit within ten minutes of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.kill("a");
    nodes.quiesce().await;
    assert_eq!(
        first.world.writes().len(),
        1,
        "the operation's body did not run once before the cut"
    );
    if matches!(protocol, Protocol::ImageCell(source, _) if source.contains("ext_read")) {
        assert_eq!(
            *first.world.reads.lock_recover(),
            1,
            "the image saved a declared read"
        );
    }
    (dir, path)
}

/// The session image resumes on a fresh node of this build: it reaps the
/// dead owner, claims the session in the build's session set, restores the
/// turn from its checkpoint and the cell from its snapshot, injects the
/// operation's committed outcome and commits the turn, without running the
/// operation's body or entering the cell's program again.
pub(super) async fn resume(image: &'static images::Image, protocol: Protocol) -> serde_json::Value {
    let mut resumed = V0::new(protocol, Dialect::SqliteFile, None);
    resumed.image = Some(image);
    let clock = SimClock::new();
    let database = resumed.database(Arc::clone(&clock)).await;
    let snapshot = database
        .actor(&actor())
        .await
        .expect("the image reads")
        .expect("the image holds the session");
    assert_eq!(
        &snapshot.formats,
        resumed.backend().formats().session(),
        "the image is in another build's set"
    );
    let nodes = SimNodes::new(
        Arc::clone(&database),
        Arc::clone(&clock),
        Script::new(),
        resumed.config(),
        resumed.activation(),
    );
    nodes.start("b");
    let horizon = clock.logical_ms() + 600_000;
    while !resumed.done(&nodes).await {
        assert!(
            clock.logical_ms() < horizon,
            "the resumed session is not done after 600 s of virtual time:\n{}",
            nodes.script().rendered_trace()
        );
        assert!(
            nodes.step().await.is_some(),
            "the resumed session stalled:\n{}",
            nodes.script().rendered_trace()
        );
    }
    nodes.quiesce().await;

    let mut violations = Vec::new();
    let labels: Vec<CommitLabel> = nodes
        .script()
        .trace()
        .iter()
        .filter(|write| write.kind == WriteKind::Actor && write.committed())
        .map(|write| write.point.label)
        .collect();
    if labels
        != [
            CommitLabel::MODEL_START,
            CommitLabel::TURN_COMMIT,
            CommitLabel::SESSION_RELEASE,
        ]
    {
        violations.push(format!(
            "the resumed session committed {labels:?}, not the second model call's start, which consumes the cell, the turn's commit and the release"
        ));
    }
    assert_eq!(
        *resumed.world.reads.lock_recover(),
        0,
        "a saved declared read was fetched again"
    );
    let writes = resumed.world.writes();
    if !writes.is_empty() {
        violations.push(format!(
            "the operation's body ran again on resume: {writes:?}"
        ));
    }
    let counts = resumed.tripwire.counts();
    let programs: usize = counts.vm_programs.values().sum();
    if programs != 0 {
        violations.push(format!(
            "the cell's program was entered {programs} times; it resumes from its snapshot"
        ));
    }
    let restores = counts
        .restores
        .get(&(session(), run()))
        .copied()
        .unwrap_or(0);
    if restores != 1 {
        violations.push(format!("the turn was restored {restores} times, not once"));
    }
    let terminal = resumed
        .backend()
        .session_store_factory()
        .run_terminal(&session(), &run())
        .await
        .expect("the resumed run's terminal reads");
    assert!(
        violations.is_empty(),
        "the session image's resume:\n  {}\nterminal: {terminal:?}\n{}",
        violations.join("\n  "),
        nodes.script().rendered_trace()
    );
    let history = serde_json::to_value(resumed.committed_history().await).expect("history encodes");
    assert!(
        history
            .to_string()
            .contains("final answer from the write completed"),
        "{history}"
    );
    history
}
