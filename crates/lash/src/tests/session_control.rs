//! A session close releases its running run's execution (ADR 0132 §12,
//! FIG-3871): deleting a session whose run is held inside its model call is
//! the session's close mail, and the close's `cancel` step cancels the open
//! turn, so the held call's future is dropped rather than left running
//! after the session's tombstone.

use super::*;

/// What the law's held model call reports: `held` counts the call once it
/// blocks, `dropped` its drop.
#[derive(Default)]
struct HeldCall {
    held: AtomicUsize,
    dropped: AtomicUsize,
}

/// Drops with the model call's future: the close's cancel aborts the turn,
/// and the call's drop is the proof the release ran.
struct HeldUntilCancelled(Arc<HeldCall>);

impl Drop for HeldUntilCancelled {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A model that answers every input but a `hold` one's: that call counts
/// itself held and never answers, keeping its run running.
fn hold_provider(calls: Arc<HeldCall>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("session-control-law")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            async move {
                let text = last_user_text(&request);
                if text.contains("hold") {
                    let _until_cancelled = HeldUntilCancelled(Arc::clone(&calls));
                    calls.held.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
                Ok(text_response(&format!("echo: {text}")))
            }
        })
        .build()
        .into_handle()
}

/// Deleting a session whose run is held inside its model call ends that
/// call: the close cancels the open turn before it writes the tombstone, so
/// the held call's future is dropped and the deletion completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_releases_its_running_runs_execution() -> Result<()> {
    let calls = Arc::new(HeldCall::default());
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(hold_provider(Arc::clone(&calls)), mock_llm_profile_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let session_id = crate::SessionId::parse("held-close").expect("nonblank host identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let held = session
        .send(crate::TurnInput::text("hold this run"))
        .await?;

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while calls.held.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sent run reached its held model call");
    assert_eq!(calls.dropped.load(Ordering::SeqCst), 0);

    let administration = core.session_administration().await;
    let deletion =
        LashCore::delete_session(administration.delete_context(session_id.as_str())?).await?;
    assert!(
        matches!(deletion, crate::SessionDeletion::Requested { .. }),
        "{deletion:?}"
    );
    let completion = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        core.await_session_deletion(&session_id),
    )
    .await
    .expect("the close finished")?;
    assert_eq!(completion, crate::SessionDeleteCompletion::Deleted);
    assert_eq!(
        calls.dropped.load(Ordering::SeqCst),
        1,
        "the close's cancel released the held model call"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), held.outcome())
        .await
        .expect("the held run answers once its session closed");
    if let Ok(crate::SendOutcome::Settled { output, .. }) = &outcome {
        assert!(
            !output.is_success(),
            "a run its session's close cancelled never succeeds: {output:?}"
        );
    }
    assert_eq!(calls.held.load(Ordering::SeqCst), 1, "the run never re-ran");
    core.shutdown().await?;
    Ok(())
}
