//! S22/L20: parked Runs redrive and cancel through their owner's own park,
//! on real hosts, with no group catalog, RunChild or ProcessAttach route.
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseSpec, Channel, Leg, Permutation, StoreKind},
    host::HostKind,
    provider::ProviderKind,
};

pub fn spec(store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> CaseSpec {
    CaseSpec {
        id: "S22".into(),
        rules: vec!["L20".into()],
        host: HostKind::UpgradeNode,
        store,
        channel: Channel::Standard,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: "settled".into(),
        // FIG-4896..4900 landed; their commits are the manifest's arc guards.
        requires: Vec::new(),
    }
}

const FORBIDDEN: [&str; 4] = ["EffectGroup", "RunChild", "ProcessAttach", "retired-child"];

/// Wait for `run`'s physical journal to refuse on N+1 naming N's generation
/// and then pause after its last attempt.
fn await_paused(
    live: &crate::h3_live::Live,
    invocations: impl Fn() -> anyhow::Result<Vec<lash_upgrade_harness::restate_view::Invocation>>,
    g_n: &str,
) -> anyhow::Result<lash_upgrade_harness::restate_view::Invocation> {
    use lash_upgrade_harness::harness::wait_for;
    let refused = wait_for("N+1 to refuse N's journal", || {
        Ok(invocations()?.into_iter().find(|invocation| {
            invocation.last_failure.as_deref().is_some_and(|failure| {
                failure.contains("RetiredGeneration") && failure.contains(g_n)
            })
        }))
    })?;
    live.record(
        &format!("refused-{}.json", refused.id),
        &serde_json::json!(format!("{refused:?}")),
    )?;
    wait_for("the refused journal to pause", || {
        Ok(invocations()?
            .into_iter()
            .find(|invocation| invocation.id == refused.id && invocation.status == "paused"))
    })
}

/// The session's live park, once one is recorded.
fn await_park(
    live: &mut crate::h3_live::Live,
    session: &str,
) -> anyhow::Result<lash_core::store::TurnPark> {
    use lash_upgrade_harness::node::h3::H3Command;
    let n = live.builds.n.clone();
    let parks = lash_upgrade_harness::harness::wait_for("the Run's owner park", || {
        let parks = n.h3(&live.case, session, &H3Command::Parks)?;
        Ok((!parks["park"].is_null()).then_some(parks))
    })?;
    live.retain_control(&n, session, &H3Command::Parks, &parks)?;
    Ok(serde_json::from_value(parks["park"].clone())?)
}

fn terminal(
    live: &mut crate::h3_live::Live,
    session: &str,
    run: &lash_core::TurnId,
) -> anyhow::Result<(lash_core::store::RunTerminal, serde_json::Value)> {
    use lash_upgrade_harness::node::h3::H3Command;
    let n = live.builds.n.clone();
    let probe = H3Command::Snapshot { run: run.clone() };
    let snapshot = lash_upgrade_harness::harness::wait_for("the Run's store terminal", || {
        let snapshot = n.h3(&live.case, session, &probe)?;
        Ok((!snapshot["terminal"].is_null()).then_some(snapshot))
    })?;
    live.retain_control(&n, session, &probe, &snapshot)?;
    let terminal: lash_core::store::RunTerminal =
        serde_json::from_value(snapshot["terminal"].clone())?;
    anyhow::ensure!(
        terminal.run == *run,
        "terminal names another Run: {snapshot}"
    );
    Ok((terminal, snapshot))
}

/// S22/L20 on real hosts. A turn Run and an operation Run each park when
/// N+1 refuses N's journal behind N's crashed address. The turn parks before
/// its commit and is redriven onto its original journal on the restored N;
/// the operation parks after its Deferred attempt is durable, is resolved
/// while parked, and its redrive races its cancel to exactly one terminal.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private live Restate"]
fn s22_parked_runs_redrive_and_cancel_without_a_catalogue_on_upgrade_nodes() -> anyhow::Result<()> {
    s22(Permutation::provisioned(StoreKind::SqliteFile, Leg::Live)?)
}

/// S22 with N and N+1 serving over the case's own PostgreSQL database.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries, private live Restate and PostgreSQL"]
fn s22_parked_runs_redrive_and_cancel_without_a_catalogue_on_upgrade_nodes_postgresql()
-> anyhow::Result<()> {
    s22(Permutation::provisioned(StoreKind::PostgreSql, Leg::Live)?)
}

/// S22 on the replay leg: the runner's always-suspending Restate makes
/// every resumption replay its journal.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private replay-leg Restate"]
fn s22_parked_runs_redrive_and_cancel_without_a_catalogue_on_upgrade_nodes_replay()
-> anyhow::Result<()> {
    s22(Permutation::provisioned(
        StoreKind::SqliteFile,
        Leg::Replay,
    )?)
}

/// S22 on the replay leg over the case's own PostgreSQL database.
#[test]
#[ignore = "needs exact candidate/synthetic-next binaries, private replay-leg Restate and PostgreSQL"]
fn s22_parked_runs_redrive_and_cancel_without_a_catalogue_on_upgrade_nodes_postgresql_replay()
-> anyhow::Result<()> {
    s22(Permutation::provisioned(
        StoreKind::PostgreSql,
        Leg::Replay,
    )?)
}

pub fn s22(permutation: Permutation) -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use lash_upgrade_harness::harness::block_on;
    use lash_upgrade_harness::identity::BuildLabel;
    use lash_upgrade_harness::node::h3::{H3Command, SourceSubscribeReply, live::SourceOp};
    use lash_upgrade_harness::node::served_by;
    use serde_json::json;
    let mut live = crate::h3_live::Live::setup("s22", permutation, spec)?;
    let (n, next) = (live.builds.n.clone(), live.builds.next.clone());
    let view = live.case.view()?;

    // Turn owner: park before the turn's commit.
    let host = live.serve(&n, "candidate")?;
    let g_n = host.generation()?.to_owned();
    let bind = host.bind()?;
    let session = live.case.session_id("s22-turn");
    let gate = "s22-turn";
    let pending = n.spawn_turn(
        &live.case,
        &session,
        &format!("hold:{gate} a turn N journals"),
    )?;
    let held_by = live.case.await_gate(gate)?;
    ensure!(
        held_by == g_n,
        "the turn was held by {held_by}, not N's {g_n}"
    );
    live.kill(host, "candidate", "turn model call held before commit")?;
    let imposter = live.serve_at(&next, &bind, "successor")?;
    let turns = || {
        Ok(
            block_on(view.invocations_like("LashTurn", &session, "run"))?
                .into_iter()
                .map(|(_, invocation)| invocation)
                .collect(),
        )
    };
    let paused = await_paused(&live, turns, &g_n)?;
    let park = await_park(&mut live, &session)?;
    ensure!(
        park.engine
            .as_ref()
            .is_none_or(|engine| engine.as_str() == paused.id),
        "the park names another execution: {park:?} vs {paused:?}"
    );
    let parked_turn = park.turn_id.clone();
    // The live follower settles on the park itself, naming this park.
    let parked = pending
        .wait()
        .err()
        .context("the turn's follower answered while its Run was parked")?
        .to_string();
    ensure!(
        parked.contains("Parked")
            && parked.contains(parked_turn.as_str())
            && parked.contains(&format!("{:?}", park.park_id)),
        "the follower did not observe this park: {parked}"
    );
    live.record("s22-turn-follower.json", &json!(parked))?;
    let unsettled = live.h3(&n, &session, &H3Command::Parks)?;
    ensure!(
        unsettled["terminal"].is_null(),
        "a parked Run already has a terminal: {unsettled}"
    );
    ensure!(
        live.case
            .effects_of(gate)?
            .iter()
            .all(|effect| effect.build != BuildLabel::Next),
        "N+1 made model calls for N's journal"
    );
    live.kill(imposter, "successor", "turn journal paused on N+1")?;
    let restored = live.serve_at(&n, &bind, "candidate")?;
    live.case.release(gate)?;
    let redrive = live.h3(
        &n,
        &session,
        &H3Command::Redrive {
            run: parked_turn.clone(),
            park: park.park_id,
        },
    )?;
    ensure!(
        redrive.get("redrive").is_some(),
        "the owner park refused its redrive: {redrive}"
    );
    let (answered, answer) = terminal(&mut live, &session, &parked_turn)?;
    ensure!(
        answered.kind() == lash_core::store::RunTerminalKind::Answered
            && answer["terminal"]
                .to_string()
                .contains(&served_by(BuildLabel::N, &g_n)),
        "the redriven turn did not settle Answered once by N: {answer}"
    );
    // The store terminal commits inside the original journal; the journal
    // completes only after the handler returns.
    lash_upgrade_harness::harness::wait_for("the redrive to finish the original journal", || {
        Ok(turns()?
            .into_iter()
            .find(|invocation| invocation.id == paused.id && invocation.status == "completed"))
    })?;
    let cleared = live.h3(&n, &session, &H3Command::Parks)?;
    ensure!(
        cleared["park"].is_null(),
        "the park outlived its Run: {cleared}"
    );
    let effects = live.case.effects_of(gate)?;
    ensure!(
        !effects.is_empty() && effects.iter().all(|effect| effect.build == BuildLabel::N),
        "the turn's model calls were not all N's: {effects:?}"
    );
    let stale = live.h3(
        &n,
        &session,
        &H3Command::Redrive {
            run: parked_turn.clone(),
            park: park.park_id,
        },
    )?;
    ensure!(
        matches!(
            stale["refused"].as_str(),
            Some("NotParked" | "ParkSuperseded")
        ),
        "a stale redrive was not refused typed: {stale}"
    );
    live.journal(&paused.id, gate, &parked_turn)?;

    // Operation owner: park after the Deferred attempt is durable, resolve
    // while parked, then race redrive and cancel.
    let session_op = live.case.session_id("s22-op");
    let admitted = live.h3(
        &n,
        &session_op,
        &H3Command::Deferred {
            key: "s22-op".into(),
        },
    )?;
    let run: lash_core::TurnId = serde_json::from_value(admitted["run"].clone())?;
    let (key, suspended) = live.await_suspended(&n, &session_op, &run)?;
    let operation = lash_core::tool_run::OperationRun::for_run_id(
        lash::SessionId::fixture(session_op.clone()),
        &run,
    )
    .context("operation Run name")?
    .operation_id;
    let source = |op| H3Command::Source {
        operation: operation.clone(),
        op,
    };
    let described = live.h3(&n, &session_op, &source(SourceOp::Describe))?;
    let descriptor: lash_core::tool_run::SourceDescriptor =
        serde_json::from_value(described["descriptor"].clone())?;
    live.kill(restored, "candidate", "operation suspended on its source")?;
    let imposter = live.serve_at(&next, &bind, "successor")?;
    let first = live.complete_first(&n, &session_op, &operation, "s22-resolved")?;
    let operations = || live.run_invocations(&key);
    let paused_op = await_paused(&live, operations, &g_n)?;
    ensure!(
        paused_op.id == suspended.id,
        "the operation paused another journal"
    );
    let park = await_park(&mut live, &session_op)?;
    ensure!(
        park.turn_id == run,
        "the operation's park names another Run: {park:?}"
    );
    let other = live.h3(&n, &session, &H3Command::Parks)?;
    ensure!(
        other["park"].is_null(),
        "another owner's park changed: {other}"
    );
    let subscribe = source(SourceOp::Subscribe {
        owner: descriptor.owner.clone(),
        segment: lash_core::tool_run::SegmentOrdinal(1),
    });
    let sealed = live.h3(&n, &session_op, &subscribe)?;
    ensure!(
        serde_json::from_value::<SourceSubscribeReply>(sealed["reply"].clone())?
            == SourceSubscribeReply::Sealed {
                seal: first.clone()
            },
        "the parked operation's source is not sealed by its completion: {sealed}"
    );
    let late = live.complete(&n, &session_op, &operation, "s22-late")?;
    ensure!(
        crate::h3_live::kept(&late, &first),
        "a completion while parked displaced the first seal: {late:?}"
    );
    let resealed = live.h3(&n, &session_op, &subscribe)?;
    ensure!(
        resealed["reply"] == sealed["reply"],
        "a completion while parked changed the seal: {resealed} vs {sealed}"
    );
    live.kill(imposter, "successor", "operation journal paused on N+1")?;
    let restored = live.serve_at(&n, &bind, "candidate")?;
    let redrive = H3Command::Redrive {
        run: run.clone(),
        park: park.park_id,
    };
    let cancel = H3Command::CancelPark {
        run: run.clone(),
        park: park.park_id,
    };
    let racing = (
        n.spawn_h3(&live.case, &session_op, &redrive)?,
        n.spawn_h3(&live.case, &session_op, &cancel)?,
    );
    let (redriven, cancelled) = (racing.0.wait()?, racing.1.wait()?);
    live.retain_control(&n, &session_op, &redrive, &redriven)?;
    live.retain_control(&n, &session_op, &cancel, &cancelled)?;
    // A verb wins when its intent applied. The loser is refused, or, for a
    // redrive the cancel superseded before it applied, accepted unapplied.
    let won = |answer: &serde_json::Value, verb: &str| answer[verb]["applied"] == json!(true);
    let redrive_won = won(&redriven, "redrive");
    ensure!(
        redrive_won != won(&cancelled, "cancel"),
        "redrive and cancel did not have exactly one winner: {redriven} / {cancelled}"
    );
    let (settled, snapshot) = terminal(&mut live, &session_op, &run)?;
    if redrive_won {
        ensure!(
            settled.kind() == lash_core::store::RunTerminalKind::Answered,
            "the winning redrive did not answer: {snapshot}"
        );
        let followed = live.h3(&n, &session_op, &H3Command::Follow { run: run.clone() })?;
        ensure!(
            followed["output"] == json!("s22-resolved"),
            "the redriven operation lost its first resolution: {followed}"
        );
        live.quiesce()?;
        ensure!(
            live.run_invocations(&key)?
                .iter()
                .any(|invocation| invocation.id == paused_op.id
                    && invocation.status == "completed"),
            "redrive did not finish the original operation journal"
        );
    } else {
        ensure!(
            settled.kind() == lash_core::store::RunTerminalKind::Cancelled
                && snapshot["unfinished"] == false,
            "the winning cancel did not end the Run once: {snapshot}"
        );
    }
    live.quiesce()?;
    let (again, _) = terminal(&mut live, &session_op, &run)?;
    ensure!(
        again == settled,
        "the operation settled twice: {again:?} vs {settled:?}"
    );
    live.record(
        "s22-race.json",
        &json!({"redrive": redriven, "cancel": cancelled, "terminal": snapshot}),
    )?;
    live.journal(&paused_op.id, "s22-op", &run)?;

    // No final binary routes recovery through a catalogue or child service.
    let targets: Vec<serde_json::Value> = block_on(view.query(&format!(
        "SELECT target FROM sys_invocation WHERE target_service_name LIKE '{}%'",
        view.service_name("")
    )))?;
    ensure!(
        targets
            .iter()
            .all(|row| FORBIDDEN.iter().all(|name| !row.to_string().contains(name))),
        "recovery reached a retired catalogue or child service: {targets:?}"
    );
    let deployments = block_on(view.deployments())?;
    ensure!(
        deployments.iter().all(|deployment| deployment
            .services
            .iter()
            .all(|service| FORBIDDEN.iter().all(|name| !service.contains(name)))),
        "a final binary still serves a retired catalogue or child service: {deployments:?}"
    );
    live.record("s22-final-routes.json", &json!({"targets": targets}))?;
    live.stop(restored, "candidate")?;
    live.finish()
}
