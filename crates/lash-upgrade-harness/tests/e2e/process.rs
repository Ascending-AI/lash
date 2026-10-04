//! S17–S21: operation and process lifecycles (L07/L08/L12/L14).
use anyhow::{Result, ensure};
use lash_upgrade_harness::node::h3::double_fixture;

/// L08/L14: admission is durable independently of the dropped caller; a
/// reattached follower sees the explicit operation result after its tool settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s17_operation_drop_and_follow_sqlite_memory() -> Result<()> {
    let (core, double) = double_fixture(0x493417).await?;
    core.session("s17-operation")
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "upgrade-harness-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core.session("s17-operation").open().await?;
    let operation = session
        .plugin_operations()
        .start_task_raw(
            "e2e.h3.operation",
            serde_json::json!("s17-exact-result"),
            "s17-input",
        )
        .await?;
    let run = operation.run().clone();
    drop(operation);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.durable().run(run.clone()).result(),
    )
    .await??;
    ensure!(
        result.output == serde_json::json!("s17-exact-result"),
        "operation result changed"
    );
    ensure!(
        session.durable().unfinished_run().await?.is_none(),
        "operation retained admission"
    );
    let invocation = lash_upgrade_harness::node::h3::operation_invocation(
        &double,
        &lash::SessionId::fixture("s17-operation"),
        &run,
    )
    .await?;
    let journal = double
        .server()
        .journal(&invocation.id)
        .ok_or_else(|| anyhow::anyhow!("journal missing"))?;
    ensure!(!journal.is_empty(), "operation has no durable records");
    Ok(())
}

/// S21/L07: the actual source object preserves the first retained result,
/// rejects another owner and returns a seal to a subscriber arriving late.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s21_resolved_before_subscription_sqlite_memory() -> Result<()> {
    use lash_core::tool_run::{
        SealOutcome, SealRefusal, SealWriter, SegmentOrdinal, SourceRefusal, SourceSeal,
    };
    use lash_upgrade_harness::node::h3::SourceFixture;
    use lash_upgrade_harness::node::h3::{SourceArmReply, SourceSealReply, SourceSubscribeReply};
    let (_core, double) = double_fixture(0x493421).await?;
    let fixture = SourceFixture::new(&double, "s21-resolved", "s21-owner").await?;
    ensure!(fixture.arm().await? == SourceArmReply::Armed { seal: None });
    let first = fixture.retained("s21-original-result").await?;
    let second = fixture.retained("s21-late-result").await?;
    ensure!(
        fixture.seal(SealWriter::External, first.clone()).await?
            == SourceSealReply::Outcome {
                outcome: SealOutcome::Sealed {
                    seal: first.clone()
                }
            }
    );
    ensure!(
        fixture
            .seal(
                SealWriter::Owner {
                    opener: lash_core::EffectOpener::session_operation(
                        "s21-resolved",
                        "wrong-owner"
                    ),
                },
                SourceSeal::Cancelled
            )
            .await?
            == SourceSealReply::Refused {
                refusal: SourceRefusal::Seal {
                    seal: SealRefusal::WrongAuthority
                }
            }
    );
    ensure!(
        fixture.seal(SealWriter::External, second).await?
            == SourceSealReply::Outcome {
                outcome: SealOutcome::AlreadySealed {
                    seal: first.clone()
                }
            }
    );
    ensure!(
        fixture
            .seal(
                SealWriter::Owner {
                    opener: fixture.descriptor.owner.clone()
                },
                SourceSeal::Cancelled
            )
            .await?
            == SourceSealReply::Outcome {
                outcome: SealOutcome::AlreadySealed {
                    seal: first.clone()
                }
            }
    );
    ensure!(
        fixture
            .subscribe_sealed(
                lash_core::EffectOpener::session_operation("s21-resolved", "wrong-owner"),
                SegmentOrdinal(1),
            )
            .await?
            == SourceSubscribeReply::Refused {
                refusal: SourceRefusal::WrongOwner
            }
    );
    ensure!(
        fixture
            .subscribe_sealed(fixture.descriptor.owner.clone(), SegmentOrdinal(1))
            .await?
            == SourceSubscribeReply::Sealed {
                seal: first.clone()
            }
    );
    ensure!(fixture.arm().await? == SourceArmReply::Armed { seal: Some(first) });
    double.server().settle().await;
    ensure!(
        double
            .server()
            .invocations()
            .iter()
            .filter(|invocation| invocation.target.contains("LashDurableWaitIndex"))
            .all(|invocation| invocation.status == "completed"),
        "source retained a waiting registry invocation"
    );
    Ok(())
}

/// S21/L07: an authorized completion arriving after owner cancellation reads
/// Cancelled and cannot change the next segment's observation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s21_cancelled_source_refuses_revival_sqlite_memory() -> Result<()> {
    use lash_core::tool_run::{SealOutcome, SealWriter, SegmentOrdinal, SourceSeal};
    use lash_upgrade_harness::node::h3::SourceFixture;
    use lash_upgrade_harness::node::h3::{SourceArmReply, SourceSealReply, SourceSubscribeReply};
    let (_core, double) = double_fixture(0x493422).await?;
    let fixture = SourceFixture::new(&double, "s21-cancelled", "s21-owner").await?;
    ensure!(fixture.arm().await? == SourceArmReply::Armed { seal: None });
    ensure!(
        fixture
            .seal(
                SealWriter::Owner {
                    opener: fixture.descriptor.owner.clone()
                },
                SourceSeal::Cancelled
            )
            .await?
            == SourceSealReply::Outcome {
                outcome: SealOutcome::Sealed {
                    seal: SourceSeal::Cancelled
                }
            }
    );
    let late = fixture.retained("s21-too-late").await?;
    ensure!(
        fixture.seal(SealWriter::External, late).await?
            == SourceSealReply::Outcome {
                outcome: SealOutcome::AlreadySealed {
                    seal: SourceSeal::Cancelled
                }
            }
    );
    ensure!(
        fixture
            .subscribe_sealed(fixture.descriptor.owner.clone(), SegmentOrdinal(2))
            .await?
            == SourceSubscribeReply::Sealed {
                seal: SourceSeal::Cancelled
            }
    );
    Ok(())
}

/// S18/L07: cancellation wakes a real suspended application timer without
/// firing it. Both an active follower and a late follower read the same store
/// terminal; the exact final wait-routing claim is guarded by FIG-4897.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s18_cancel_suspended_application_timer_sqlite_memory() -> Result<()> {
    use lash_upgrade_harness::node::h3::rlm::fixture;
    let (core, double) = fixture(
        0x493418,
        "await sleep(86400000); finish(\"timer-elapsed\");",
    )
    .await?;
    let session_id = lash::SessionId::fixture("s18-application-timer");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "upgrade-harness-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core.session(session_id.clone()).open().await?;
    let handle = session
        .send(lash::TurnInput::text("await the application timer"))
        .id("s18-timer-input")
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    let run = handle
        .run()
        .await?
        .ok_or_else(|| anyhow::anyhow!("timer input not admitted"))?;
    drop(handle);
    let invocation =
        lash_upgrade_harness::node::h3::operation_invocation(&double, &session_id, &run).await?;
    if invocation.status != "suspended" {
        let outcome = session.run(run.clone()).outcome().await?;
        anyhow::bail!(
            "application timer did not wait: {invocation:?}; terminal={:?}",
            outcome.status()
        );
    }
    ensure!(
        invocation.status == "suspended" && invocation.suspensions > 0,
        "operation timer never actually suspended: {invocation:?}"
    );
    let suspended_id = invocation.id.clone();
    let application_timers = double.server().timers();
    ensure!(
        !application_timers.is_empty(),
        "application sleep registered no real timer"
    );
    let before = double.server().now_ms();
    ensure!(
        double
            .stores()
            .session_store_factory()
            .run_terminal(&session_id, &run)
            .await?
            .is_none(),
        "suspended operation already has a terminal"
    );
    let follow = session.run(run.clone()).outcome();
    let receipt = session.run(run.clone()).cancel().await?;
    ensure!(
        matches!(receipt, lash::CancelReceipt::Requested { .. }),
        "public cancellation did not address the admitted operation: {receipt:?}"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), follow).await??;
    ensure!(
        outcome.run() == Some(&run) && outcome.status() == lash::TurnStatus::Cancelled,
        "cancelled timer did not settle its own Run: {outcome:?}"
    );
    let terminal = double
        .stores()
        .session_store_factory()
        .run_terminal(&session_id, &run)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cancelled operation has no store terminal"))?;
    ensure!(
        terminal.kind() == lash_core::store::RunTerminalKind::Cancelled,
        "stored timer terminal is not cancellation"
    );
    ensure!(
        double.server().now_ms().saturating_sub(before) < 86_400_000,
        "cancellation waited for the application timer"
    );
    let late = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        session.durable().run(run.clone()).outcome(),
    )
    .await??;
    ensure!(
        late.run() == Some(&run) && late.status() == lash::TurnStatus::Cancelled,
        "late follower lost the cancelled terminal"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    let pending: Vec<_> = double
        .server()
        .invocations()
        .into_iter()
        .filter(|invocation| {
            (invocation.id == suspended_id || invocation.target.ends_with("/await_terminal"))
                && invocation.status != "completed"
        })
        .collect();
    ensure!(
        pending.is_empty(),
        "cancel retained operation/attach waits: {pending:?}"
    );
    ensure!(
        session.durable().unfinished_run().await?.is_none(),
        "cancel retained admission"
    );
    Ok(())
}
