//! TERMINAL-WINDOW-RETENTION (FIG-6606): the window a run's terminal names
//! is its session's revision, so the session's `Retention` releases it with
//! that revision, a pin keeps it exactly as it ended, and its compact
//! outcome reads forever.

use super::*;

use lash_core::Target;

/// One ended run: its id, the read of the window it ended on, and that
/// window's checkpoint root.
struct Ended {
    run: lash_vocabulary::identity::RunId,
    window: serde_json::Value,
    checkpoint: String,
}

/// The checkpoint root the window `run` of `session` ended on names, and
/// every root source that still names it.
fn window_roots(
    stores: &lash_sqlite_store::SqliteStoreSet,
    session: &str,
    checkpoint: Option<&str>,
    run: &str,
) -> (String, Vec<String>) {
    let conn = rusqlite::Connection::open(stores.database_uri()).expect("open the SQLite catalog");
    let checkpoint = checkpoint.map_or_else(
        || {
            conn.query_row(
                "SELECT lower(hex(revision.checkpoint_ref)) FROM session_runs AS ended
                 JOIN session_revisions AS revision
                   ON revision.session_id = ended.session_id
                  AND revision.head_revision = ended.terminal_window_revision
                 WHERE ended.session_id = ?1 AND ended.run = ?2",
                [session, run],
                |row| row.get::<_, String>(0),
            )
            .expect("an ended run's window names its revision's checkpoint")
        },
        str::to_owned,
    );
    let mut sources = conn
        .prepare("SELECT source FROM graph_root_catalog WHERE root_ref = unhex(?1) ORDER BY source")
        .expect("root catalog")
        .query_map([checkpoint.as_str()], |row| row.get::<_, String>(0))
        .expect("read the root catalog")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("root sources");
    sources.dedup();
    (checkpoint, sources)
}

/// Under `retention`, four changed-state turns end with the first one
/// pinned. The middle windows are released with their revisions: no source
/// roots their checkpoints, a read of them is refused as released (never
/// the head in their place), and their outcomes still read. The pinned
/// window and the head's read exactly as they ended.
async fn terminal_windows_follow_the_session_retention(
    retention: lash_core::Retention,
    session_name: &str,
) -> Result<()> {
    let stores = sqlite_memory_store_set().await;
    let backend =
        lash_conformance::backend_over(Arc::<lash_sqlite_store::SqliteStoreSet>::clone(&stores));
    let fixture = fixture_over(1, |_| backend).await?;
    let session = created_session(
        &fixture.core,
        lash_kernel_value::identity::SessionId::parse(session_name)
            .expect("nonblank host identity"),
    )
    .await
    .open()
    .await?;
    session.set_retention(retention).await?;
    let mut ended = Vec::new();
    for turn in 0..4 {
        let handle = session
            .send(TurnInput::text(format!("turn {turn}")))
            .await?;
        let input = handle.input_id().clone();
        handle.output().await?;
        let run = session
            .attach(input)
            .run()
            .await?
            .expect("a settled turn names its run");
        if turn == 0 {
            session
                .pin(Target::Turn(
                    lash_vocabulary::identity::TurnId::parse(run.as_str())
                        .expect("a run id is a turn id"),
                ))
                .await?;
        }
        let window = session
            .durable()
            .read_run(&run)
            .await?
            .expect("a run that just ended reads its window");
        let (checkpoint, _) = window_roots(&stores, session_name, None, run.as_str());
        ended.push(Ended {
            run,
            window: serde_json::to_value(window.to_snapshot()).expect("snapshot"),
            checkpoint,
        });
    }
    fixture
        .core
        .store_factory
        .gc_unreachable()
        .await
        .expect("the host collection completes");

    let checkpoints = ended
        .iter()
        .map(|run| run.checkpoint.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        checkpoints.len(),
        4,
        "each changed-state turn ends on its own checkpoint"
    );
    for (index, run) in ended.iter().enumerate() {
        let read = session.durable().read_run(&run.run).await;
        let (_, sources) = window_roots(
            &stores,
            session_name,
            Some(&run.checkpoint),
            run.run.as_str(),
        );
        if index == 0 || index == 3 {
            let window = read?.expect("a pinned or head window stays readable");
            assert_eq!(
                serde_json::to_value(window.to_snapshot()).expect("snapshot"),
                run.window,
                "{retention:?}: window {index} reads exactly as it ended"
            );
            assert_eq!(
                sources,
                ["revision"],
                "{retention:?}: only its revision roots it"
            );
        } else {
            assert!(
                matches!(
                    &read,
                    Err(EmbedError::Store(lash_core::StoreError::RunWindowReleased { run: released, .. }))
                        if released.as_str() == run.run.as_str()
                ),
                "{retention:?}: released window {index} must be refused as released: {read:?}"
            );
            assert!(
                sources.is_empty(),
                "{retention:?}: released window {index}'s checkpoint is still rooted by {sources:?}"
            );
        }
        let outcome = session.run(run.run.clone()).outcome().await?;
        assert_eq!(
            outcome
                .output()
                .and_then(|output| output.assistant_message()),
            Some(format!("echo: turn {index}").as_str()),
            "{retention:?}: run {index}'s compact outcome reads after its window is released"
        );
    }
    fixture.core.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_windows_are_released_with_their_revision_under_head_only() -> Result<()> {
    terminal_windows_follow_the_session_retention(lash_core::Retention::HeadOnly, "windows-head")
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminal_windows_are_released_with_their_revision_under_last_turns() -> Result<()> {
    terminal_windows_follow_the_session_retention(
        lash_core::Retention::LastTurns(std::num::NonZeroU32::MIN),
        "windows-last",
    )
    .await
}
