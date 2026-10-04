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
    let invocation = double
        .server()
        .invocations()
        .into_iter()
        .find(|invocation| {
            invocation.target.contains("LashTurn")
                && invocation.target.contains(run.as_str())
                && invocation.target.ends_with("/run")
        })
        .ok_or_else(|| anyhow::anyhow!("no operation journal"))?;
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
