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

/// S21/L07: a host's cancel of an operation Run suspended on its Deferred
/// source reaches the Run's source wait: the command settles Cancelled and
/// the Run ends with its one store terminal (FIG-5006).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s21_cancel_reaches_a_suspended_deferred_operation_sqlite_memory() -> Result<()> {
    let (core, double) = double_fixture(0x495006).await?;
    let session_id = lash::SessionId::fixture("s21-cancel-deferred");
    core.session(session_id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "upgrade-harness-model",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(8),
        )))
        .await?;
    let session = core.session(session_id.clone()).open().await?;
    let handle = session
        .plugin_operations()
        .start_task_raw(
            "e2e.h3.deferred",
            serde_json::json!("s21-cancel-deferred"),
            "s21-cancel-deferred",
        )
        .await?;
    let run = handle.run().clone();
    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    let invocation =
        lash_upgrade_harness::node::h3::operation_invocation(&double, &session_id, &run).await?;
    ensure!(
        invocation.status != "completed",
        "deferred operation settled without its source: {invocation:?}"
    );
    ensure!(
        double
            .stores()
            .session_store_factory()
            .run_terminal(&session_id, &run)
            .await?
            .is_none(),
        "suspended operation already has a terminal"
    );
    let receipt = session.run(run.clone()).cancel().await?;
    ensure!(
        matches!(
            receipt,
            lash::CancelReceipt::OperationRequested {
                request: lash::PluginTaskCancelRequest::Requested,
                ..
            }
        ),
        "public cancellation did not reach the admitted operation's signal: {receipt:?}"
    );
    let outcome = match tokio::time::timeout(
        std::time::Duration::from_secs(20),
        session.durable().run(run.clone()).outcome(),
    )
    .await
    {
        Ok(outcome) => outcome?,
        Err(_) => {
            let invocation =
                lash_upgrade_harness::node::h3::operation_invocation(&double, &session_id, &run)
                    .await?;
            anyhow::bail!(
                "cancelled Deferred operation never settled; invocation: {}",
                invocation.status
            );
        }
    };
    ensure!(
        outcome.run() == Some(&run) && outcome.status() == lash::TurnStatus::Cancelled,
        "cancelled operation did not settle its own Run: {outcome:?}"
    );
    let terminal = double
        .stores()
        .session_store_factory()
        .run_terminal(&session_id, &run)
        .await?
        .ok_or_else(|| anyhow::anyhow!("cancelled operation has no store terminal"))?;
    // An operation Run admits no turn: its one terminal is the command run's
    // `CommandsApplied` end, and how the task ended is the command's
    // settlement, which `outcome` above already answered Cancelled (K8).
    ensure!(
        terminal.run == run
            && terminal.cause == lash_core::store::RunTerminalCause::CommandsApplied,
        "cancelled operation's terminal is not its command-run end: {terminal:?}"
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), double.server().settle()).await?;
    let pending: Vec<_> = double
        .server()
        .invocations()
        .into_iter()
        .filter(|candidate| {
            (candidate.id == invocation.id || candidate.target.contains("LashDurableWaitIndex"))
                && candidate.status != "completed"
        })
        .collect();
    ensure!(
        pending.is_empty(),
        "cancel retained operation/registry waits: {pending:?}"
    );
    ensure!(
        session.durable().unfinished_run().await?.is_none(),
        "cancel retained admission"
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
    let application_timers: Vec<_> = double
        .server()
        .timers()
        .into_iter()
        .filter(|timer| timer.invocation == suspended_id && timer.kind == "sleep")
        .collect();
    ensure!(
        !application_timers.is_empty(),
        "application sleep registered no real timer"
    );
    let deadline = application_timers
        .iter()
        .map(|timer| timer.fire_at_ms)
        .min()
        .ok_or_else(|| anyhow::anyhow!("application sleep has no deadline"))?;
    ensure!(
        double.server().now_ms() < deadline,
        "application timer already fired"
    );
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
        double.server().now_ms() < deadline,
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

/// S21's catalogue entry for its live upgrade-node case.
pub fn s21_spec(
    store: lash_upgrade_harness::e2e::case::StoreKind,
    artifacts: Vec<lash_upgrade_harness::e2e::case::ArtifactIdentity>,
) -> lash_upgrade_harness::e2e::case::CaseSpec {
    lash_upgrade_harness::e2e::case::CaseSpec {
        id: "S21".into(),
        rules: vec!["L07".into(), "L12".into()],
        host: lash_upgrade_harness::e2e::host::HostKind::UpgradeNode,
        store,
        channel: lash_upgrade_harness::e2e::case::Channel::Standard,
        provider: lash_upgrade_harness::e2e::provider::ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: "settled".into(),
        requires: Vec::new(),
    }
}

/// One source-registry control against the live deployment.
fn source(
    live: &mut crate::h3_live::Live,
    session: &str,
    operation: &str,
    op: lash_upgrade_harness::node::h3::live::SourceOp,
) -> Result<serde_json::Value> {
    let node = live.builds.n.clone();
    live.h3(
        &node,
        session,
        &lash_upgrade_harness::node::h3::H3Command::Source {
            operation: operation.into(),
            op,
        },
    )
}

fn reply<T: serde::de::DeserializeOwned>(answer: serde_json::Value) -> Result<T> {
    Ok(serde_json::from_value(answer["reply"].clone())?)
}

/// S21/L07/L12 on a real host: the live registry keeps the first seal
/// against duplicate, wrong-authority and owner-cancel writes; a real
/// Deferred Run drains its resolved final exactly once; a cancelled Run's
/// late completion neither publishes nor revives it.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private live Restate"]
fn s21_source_seal_stays_immutable_on_the_upgrade_node() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    s21(Permutation::provisioned(StoreKind::SqliteFile, Leg::Live)?)
}

/// S21 with the upgrade node serving over the case's own PostgreSQL database.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries, private live Restate and PostgreSQL"]
fn s21_source_seal_stays_immutable_on_the_upgrade_node_postgresql() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    s21(Permutation::provisioned(StoreKind::PostgreSql, Leg::Live)?)
}

/// S21 on the replay leg: the runner's always-suspending Restate makes
/// every resumption replay its journal.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private replay-leg Restate"]
fn s21_source_seal_stays_immutable_on_the_upgrade_node_replay() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    s21(Permutation::provisioned(
        StoreKind::SqliteFile,
        Leg::Replay,
    )?)
}

/// S21 on the replay leg over the case's own PostgreSQL database.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries, private replay-leg Restate and PostgreSQL"]
fn s21_source_seal_stays_immutable_on_the_upgrade_node_postgresql_replay() -> Result<()> {
    use lash_upgrade_harness::e2e::case::{Leg, Permutation, StoreKind};
    s21(Permutation::provisioned(
        StoreKind::PostgreSql,
        Leg::Replay,
    )?)
}

pub fn s21(permutation: lash_upgrade_harness::e2e::case::Permutation) -> Result<()> {
    use lash_core::tool_run::{
        SealOutcome, SealRefusal, SealWriter, SegmentOrdinal, SourceRefusal, SourceSeal,
    };
    use lash_upgrade_harness::harness::{block_on, wait_for};
    use lash_upgrade_harness::node::h3::live::SourceOp;
    use lash_upgrade_harness::node::h3::{
        H3Command, SourceArmReply, SourceSealReply, SourceSubscribeReply,
    };
    use serde_json::json;
    let mut live = crate::h3_live::Live::setup("s21", permutation, s21_spec)?;
    let n = live.builds.n.clone();
    let n_host = live.serve(&n, "candidate")?;
    let opener = |session: &str, operation: &str| {
        lash_core::EffectOpener::session_operation(
            lash_core::SessionId::fixture(session.to_owned()),
            operation,
        )
    };
    let retained =
        |live: &mut crate::h3_live::Live, session: &str, value: &str| -> Result<SourceSeal> {
            let answer = source(
                live,
                session,
                "s21-owner",
                SourceOp::Retain {
                    value: value.into(),
                },
            )?;
            Ok(serde_json::from_value(answer["seal"].clone())?)
        };
    let seal = |live: &mut crate::h3_live::Live,
                session: &str,
                operation: &str,
                writer: SealWriter,
                seal: SourceSeal|
     -> Result<SourceSealReply> {
        reply(source(
            live,
            session,
            operation,
            SourceOp::Seal { writer, seal },
        )?)
    };
    let subscribe = |live: &mut crate::h3_live::Live,
                     session: &str,
                     operation: &str,
                     owner: lash_core::EffectOpener,
                     segment: u32|
     -> Result<SourceSubscribeReply> {
        reply(source(
            live,
            session,
            operation,
            SourceOp::Subscribe {
                owner,
                segment: SegmentOrdinal(segment),
            },
        )?)
    };
    let arm = |live: &mut crate::h3_live::Live, session: &str| -> Result<SourceArmReply> {
        reply(source(live, session, "s21-owner", SourceOp::Arm)?)
    };

    // Resolve before subscribe, against the live registry object.
    let resolved = live.case.session_id("s21-resolved");
    let owner = opener(&resolved, "s21-owner");
    let wrong = opener(&resolved, "wrong-owner");
    ensure!(arm(&mut live, &resolved)? == SourceArmReply::Armed { seal: None });
    let first = retained(&mut live, &resolved, "s21-original-result")?;
    let second = retained(&mut live, &resolved, "s21-late-result")?;
    ensure!(
        seal(
            &mut live,
            &resolved,
            "s21-owner",
            SealWriter::External,
            first.clone()
        )? == SourceSealReply::Outcome {
            outcome: SealOutcome::Sealed {
                seal: first.clone()
            }
        }
    );
    ensure!(
        seal(
            &mut live,
            &resolved,
            "s21-owner",
            SealWriter::Owner {
                opener: wrong.clone()
            },
            SourceSeal::Cancelled
        )? == SourceSealReply::Refused {
            refusal: SourceRefusal::Seal {
                seal: SealRefusal::WrongAuthority
            }
        }
    );
    ensure!(
        seal(
            &mut live,
            &resolved,
            "s21-owner",
            SealWriter::External,
            second
        )? == SourceSealReply::Outcome {
            outcome: SealOutcome::AlreadySealed {
                seal: first.clone()
            }
        }
    );
    ensure!(
        seal(
            &mut live,
            &resolved,
            "s21-owner",
            SealWriter::Owner {
                opener: owner.clone()
            },
            SourceSeal::Cancelled
        )? == SourceSealReply::Outcome {
            outcome: SealOutcome::AlreadySealed {
                seal: first.clone()
            }
        }
    );
    ensure!(
        subscribe(&mut live, &resolved, "s21-owner", wrong, 1)?
            == SourceSubscribeReply::Refused {
                refusal: SourceRefusal::WrongOwner
            }
    );
    ensure!(
        subscribe(&mut live, &resolved, "s21-owner", owner.clone(), 1)?
            == SourceSubscribeReply::Sealed {
                seal: first.clone()
            }
    );
    ensure!(arm(&mut live, &resolved)? == SourceArmReply::Armed { seal: Some(first) });

    // An owner cancellation seals first; a late authorized completion
    // cannot change the next segment's observation.
    let cancelled = live.case.session_id("s21-cancelled");
    let owner = opener(&cancelled, "s21-owner");
    ensure!(arm(&mut live, &cancelled)? == SourceArmReply::Armed { seal: None });
    ensure!(
        seal(
            &mut live,
            &cancelled,
            "s21-owner",
            SealWriter::Owner {
                opener: owner.clone()
            },
            SourceSeal::Cancelled
        )? == SourceSealReply::Outcome {
            outcome: SealOutcome::Sealed {
                seal: SourceSeal::Cancelled
            }
        }
    );
    let late = retained(&mut live, &cancelled, "s21-too-late")?;
    ensure!(
        seal(
            &mut live,
            &cancelled,
            "s21-owner",
            SealWriter::External,
            late
        )? == SourceSealReply::Outcome {
            outcome: SealOutcome::AlreadySealed {
                seal: SourceSeal::Cancelled
            }
        }
    );
    ensure!(
        subscribe(&mut live, &cancelled, "s21-owner", owner, 2)?
            == SourceSubscribeReply::Sealed {
                seal: SourceSeal::Cancelled
            }
    );

    // A real Deferred Run: its first resolution is the protected final it
    // drains; a duplicate and a wrong-owner cancel change nothing.
    let session = live.case.session_id("s21-run");
    let admitted = live.h3(
        &n,
        &session,
        &H3Command::Deferred {
            key: "s21-resolved-run".into(),
        },
    )?;
    let run: lash_core::TurnId = serde_json::from_value(admitted["run"].clone())?;
    let (key, suspended) = live.await_suspended(&n, &session, &run)?;
    let operation = lash_core::tool_run::OperationRun::for_run_id(
        lash::SessionId::fixture(session.clone()),
        &run,
    )
    .ok_or_else(|| anyhow::anyhow!("{run} is not an operation Run"))?
    .operation_id;
    let described = source(&mut live, &session, &operation, SourceOp::Describe)?;
    let descriptor: lash_core::tool_run::SourceDescriptor =
        serde_json::from_value(described["descriptor"].clone())?;
    ensure!(
        seal(
            &mut live,
            &session,
            &operation,
            SealWriter::Owner {
                opener: opener(&session, "wrong-owner")
            },
            SourceSeal::Cancelled
        )? == SourceSealReply::Refused {
            refusal: SourceRefusal::Seal {
                seal: SealRefusal::WrongAuthority
            }
        },
        "a wrong owner's cancel was not refused typed"
    );
    let first = live.complete_first(&n, &session, &operation, "s21-first")?;
    let duplicate = live.complete(&n, &session, &operation, "s21-second")?;
    ensure!(
        crate::h3_live::kept(&duplicate, &first),
        "a duplicate completion displaced the first seal: {duplicate:?}"
    );
    let followed = live.h3(&n, &session, &H3Command::Follow { run: run.clone() })?;
    ensure!(
        followed["output"] == json!("s21-first"),
        "the Run did not drain its first resolution: {followed}"
    );
    let snapshot = live.h3(&n, &session, &H3Command::Snapshot { run: run.clone() })?;
    let terminal: lash_core::store::RunTerminal =
        serde_json::from_value(snapshot["terminal"].clone())?;
    ensure!(
        terminal.run == run
            && terminal.kind() == lash_core::store::RunTerminalKind::Answered
            && snapshot["unfinished"] == false,
        "resolved Run has no single Answered terminal: {snapshot}"
    );
    // Once the Run settles its scope may already be retired: a typed
    // refusal is allowed, a different seal or a fresh subscription is not.
    let sealed = subscribe(&mut live, &session, &operation, descriptor.owner.clone(), 1)?;
    ensure!(
        match &sealed {
            SourceSubscribeReply::Sealed { seal } => *seal == first,
            SourceSubscribeReply::Refused { .. } => true,
            SourceSubscribeReply::Subscribed => false,
        },
        "the settled Run's source shows another seal: {sealed:?}"
    );
    live.journal(&suspended.id, "s21-resolved-run", &run)?;
    let invocations = live.run_invocations(&key)?;
    ensure!(
        invocations.len() == 1 && invocations[0].status == "completed",
        "resolved Run kept or opened another journal: {invocations:?}"
    );

    // A cancelled Run: the late completion neither publishes nor revives it.
    let session = live.case.session_id("s21-cancel-run");
    let admitted = live.h3(
        &n,
        &session,
        &H3Command::Deferred {
            key: "s21-cancelled-run".into(),
        },
    )?;
    let run: lash_core::TurnId = serde_json::from_value(admitted["run"].clone())?;
    let (key, suspended) = live.await_suspended(&n, &session, &run)?;
    let operation = lash_core::tool_run::OperationRun::for_run_id(
        lash::SessionId::fixture(session.clone()),
        &run,
    )
    .ok_or_else(|| anyhow::anyhow!("{run} is not an operation Run"))?
    .operation_id;
    let described = source(&mut live, &session, &operation, SourceOp::Describe)?;
    let descriptor: lash_core::tool_run::SourceDescriptor =
        serde_json::from_value(described["descriptor"].clone())?;
    let receipt = live.h3(&n, &session, &H3Command::Cancel { run: run.clone() })?;
    ensure!(
        receipt["receipt"]
            .as_str()
            .is_some_and(|receipt| receipt.starts_with("OperationRequested")),
        "public cancellation did not address the admitted operation: {receipt}"
    );
    let probe = H3Command::Snapshot { run: run.clone() };
    let snapshot = wait_for("the cancelled Run's store terminal", || {
        let snapshot = n.h3(&live.case, &session, &probe)?;
        Ok((!snapshot["terminal"].is_null()).then_some(snapshot))
    })
    .map_err(|error| {
        let open = live.run_invocations(&key);
        let snapshot = n.h3(&live.case, &session, &probe);
        anyhow::anyhow!("{error}; journals {open:?}; store {snapshot:?}")
    })?;
    let terminal: lash_core::store::RunTerminal =
        serde_json::from_value(snapshot["terminal"].clone())?;
    ensure!(
        terminal.run == run
            && terminal.cause == lash_core::store::RunTerminalCause::CommandsApplied,
        "cancel did not end its own operation Run: {snapshot}"
    );
    // Operation Runs end CommandsApplied; cancellation is the task's
    // command outcome in the same durable settling commit (ADR 0101).
    let completion = block_on(async {
        live.stores()
            .await?
            .session_store_factory()
            .queued_work_batch_completion(&lash::SessionId::fixture(session.clone()), &operation)
            .await?
            .ok_or_else(|| anyhow::anyhow!("cancelled operation has no command settlement"))
    })?;
    ensure!(
        completion.command_outcomes.iter().any(|(batch, outcome)| {
            batch.as_str() == operation
                && matches!(
                    outcome,
                    lash_core::runtime::SessionCommandOutcome::PluginOperation {
                        outcome: lash_core::runtime::PluginOperationCommandOutcome::Cancelled
                    }
                )
        }),
        "cancel did not settle its own task Cancelled: {completion:?}"
    );
    live.evidence.stores.push(json!({
        "kind": "cancelled_operation_settlement",
        "run": run,
        "receipt": completion,
    }));
    live.quiesce()?;
    let before = live.run_invocations(&key)?;
    let late = live.complete(&n, &session, &operation, "s21-too-late")?;
    ensure!(
        crate::h3_live::kept(&late, &SourceSeal::Cancelled),
        "a completion after cancellation displaced the Cancelled seal: {late:?}"
    );
    let observed = subscribe(&mut live, &session, &operation, descriptor.owner.clone(), 2)?;
    ensure!(
        matches!(
            &observed,
            SourceSubscribeReply::Sealed {
                seal: SourceSeal::Cancelled
            } | SourceSubscribeReply::Refused { .. }
        ),
        "a late completion resealed the cancelled source: {observed:?}"
    );
    ensure!(
        subscribe(&mut live, &session, &operation, descriptor.owner.clone(), 2)? == observed,
        "the cancelled source's observation changed"
    );
    live.quiesce()?;
    let after = live.h3(&n, &session, &probe)?;
    ensure!(
        after["terminal"] == snapshot["terminal"] && after["unfinished"] == false,
        "a late completion changed the cancelled Run: {after}; late answer {late:?}"
    );
    let revived = live.run_invocations(&key)?;
    ensure!(
        revived == before
            && revived
                .iter()
                .all(|invocation| invocation.status == "completed"),
        "a late completion revived the cancelled Run: {revived:?}"
    );
    live.journal(&suspended.id, "s21-cancelled-run", &run)?;
    let view = live.case.view()?;
    let waits: Vec<serde_json::Value> = block_on(view.query(&format!(
        "SELECT id, status FROM sys_invocation WHERE target_service_name = '{}' AND status <> 'completed'",
        view.service_name("LashDurableWaitIndex")
    )))?;
    ensure!(
        waits.is_empty(),
        "sources retained waiting registry invocations: {waits:?}"
    );
    live.stop(n_host, "candidate")?;
    live.finish()
}
