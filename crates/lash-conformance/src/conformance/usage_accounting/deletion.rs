use super::*;

#[derive(Default)]
pub(super) struct Child {
    pub(super) release: tokio_util::sync::CancellationToken,
    pub(super) dispatched: AtomicUsize,
    finished: tokio_util::sync::CancellationToken,
    refused: std::sync::Mutex<Option<crate::PluginError>>,
}

impl Child {
    #[expect(clippy::expect_used, reason = "conformance fixture assertions")]
    pub(super) async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let request = || {
            let mut request =
                crate::DirectRequest::text("mock-model", "child held across deletion");
            request
                .extra_body
                .insert("usage_delete_child".into(), serde_json::json!(true));
            request
        };
        call.context
            .direct_completions()
            .complete(request(), "delete-child")
            .await
            .unwrap_or_else(|error| panic!("the call admitted before drain can finish: {error}"));
        let refused = call
            .context
            .direct_completions()
            .complete(request(), "delete-child")
            .await;
        let error = refused.expect_err("the drained owner refuses the child's next dispatch");
        *self.refused.lock_recover() = Some(error);
        self.finished.cancel();
        crate::ToolOutcome::ok(serde_json::json!("child finished")).into()
    }
}

/// E5: deletion resolves a child admitted before drain, retains settled spend,
/// and refuses the still-running child's next dispatch. Its late settlement
/// then replaces the unknown liability with the one reported fact.
#[expect(clippy::expect_used, reason = "conformance fixture assertions")]
pub async fn session_delete_drains_accounting_first(tier: &UsageAccountingTier) {
    let child = Arc::new(Child::default());
    let mut world = World::new(
        tier,
        "delete-child-mid-call",
        Script {
            kill: Kill::InTool(0),
            ..Script::completed(2)
        },
    );
    world.deletion_child = Some(Arc::clone(&child));
    world.run_killed_forever().await;
    assert_eq!(
        child.dispatched.load(Ordering::SeqCst),
        1,
        "the child is mid-call"
    );
    let accounting = tier.stores.usage_accounting();
    let deadline = tokio::time::Instant::now() + DELIVERY;
    loop {
        let usage = accounting
            .load_owner_usage(&world.owner())
            .await
            .expect("read before deletion");
        if usage
            .rows
            .iter()
            .map(|row| row.reported_attempts)
            .sum::<u64>()
            == 1
        {
            assert_eq!(
                usage.completeness.open_runs, 1,
                "the mid-call child has one open run"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the opener's settled call is delivered"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    delete_session(tier, &world).await;
    let drained = world.settled().await;
    assert!(drained.completeness.retired);
    assert_eq!(drained.completeness.open_runs, 0);
    assert_eq!(
        drained.completeness.unknown_runs, 1,
        "the mid-call child is unknown(owner_retired)"
    );
    assert_eq!(
        world.facts().await.len(),
        1,
        "the settled opener call survives physical deletion"
    );
    let runs = accounting
        .load_usage_run_page(
            &world.owner(),
            crate::UsageRunFilter::Unresolved,
            None,
            std::num::NonZeroU32::new(10).expect("nonzero"),
        )
        .await
        .expect("read the child's liability");
    assert_eq!(runs.runs.len(), 1);
    assert_eq!(
        runs.runs[0].state,
        crate::UsageRunState::Unknown(crate::UsageUnknownReason::OwnerRetired)
    );
    let bought = world.invocations();
    assert_eq!(bought, 2, "one opener call and one child call");
    child.release.cancel();
    tokio::time::timeout(DELIVERY, child.finished.cancelled())
        .await
        .expect("the surviving child finishes");
    assert_eq!(
        world.invocations(),
        bought,
        "a retired owner refuses every later provider dispatch"
    );
    let error = child
        .refused
        .lock_recover()
        .take()
        .expect("the dispatch was refused")
        .into_turn_failure(crate::RuntimeErrorCode::Plugin);
    assert_eq!(
        lash_sansio::FailureCode::from(&error.code).turn_code(),
        Some(lash_sansio::TurnFailureCode::UsageOwnerRetired),
        "the refusal retains its typed code: {error:?}"
    );
    tokio::time::timeout(DELIVERY, async {
        while world.facts().await.len() != 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the surviving child's detached settlement is delivered after deletion");
    assert_each_returned_attempt_once(&world, 0, "late child settlement after deletion").await;
    assert_eq!(
        world.facts().await.len(),
        2,
        "each paid call is charged once"
    );
    assert_eq!(
        world.invocations(),
        bought,
        "the refused dispatch buys nothing"
    );
}
