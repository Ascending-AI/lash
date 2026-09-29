//! `just e2e-rolling`: the ADR 0106 §6 choreography over the two Phase A
//! builds (ADR 0115 §6).
//!
//! Each roll runs on its own store, PostgreSQL and then a SQLite store
//! directory, against one live `restate-server`, with every turn on one
//! session so each build reads what the other wrote:
//!
//! 1. **migrate:** N provisions the store; N serves it and answers a turn.
//! 2. **half roll:** N+1 migrates (a no-op until the synthetic expand step
//!    lands) and serves beside N; a host of each build gets an answer.
//! 3. **rollback:** N+1 stops; N registers at a fresh URI and answers.
//! 4. **roll:** N+1 registers at a fresh URI, N stops; N+1 answers.
//!
//! Every step must succeed: a refusal fails the run. Drain, retire and
//! finalize are the choreography's last three steps; they wait for
//! `lashctl drain` (lane L6) and finalize (FIG-3800 B), and the Phase A legs
//! in `tests/phase_a/` wait for the lanes that build the rest.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::harness::{Case, NodeBinary, NodeBuilds, Services};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::{TurnReport, served_by};
use serde::Serialize;

/// Where the run keeps its evidence: `just e2e-rolling` points it at its
/// artifact directory. A bare run uses a temporary directory.
const ARTIFACT_DIR_ENV: &str = "LASH_E2E_ROLLING_ARTIFACT_DIR";

/// One turn of the choreography, as `rolling-report.json` records it.
#[derive(Serialize)]
struct StepRecord {
    case: String,
    step: &'static str,
    /// The build that must have driven the turn; `None` while both serve.
    expected_driver: Option<BuildLabel>,
    report: TurnReport,
}

/// One turn, checked to have been driven by `driver` when only one build
/// serves.
fn turn(
    steps: &mut Vec<StepRecord>,
    host: &NodeBinary,
    case: &Case,
    session: &str,
    step: &'static str,
    driver: Option<&NodeBinary>,
) -> Result<()> {
    let report = host.turn(case, session, &format!("{step} from {}", host.label()))?;
    println!(
        "{}: {step}: host {} got {:?}",
        case.name, report.host, report.reply
    );
    if let Some(driver) = driver {
        let expected = served_by(driver.label(), &driver.identity().generation);
        ensure!(
            report.reply.as_deref() == Some(expected.as_str()),
            "{}: {step}: expected the reply `{expected}`, got {:?}",
            case.name,
            report.reply
        );
    }
    steps.push(StepRecord {
        case: case.name.clone(),
        step,
        expected_driver: driver.map(NodeBinary::label),
        report,
    });
    Ok(())
}

fn roll(steps: &mut Vec<StepRecord>, builds: &NodeBuilds, case: &Case) -> Result<()> {
    let (n, next) = (&builds.n, &builds.next);
    let session = case.session_id("rolling");

    // migrate: N provisions the store and serves it.
    n.migrate(case)?;
    let n_first = n.serve(case)?;
    turn(steps, n, case, &session, "migrate", Some(n))?;

    // half roll: N+1 serves beside N. Which build drives each host's turn
    // depends on Restate's routing between the two deployments, so only the
    // answer is checked.
    next.migrate(case)?;
    let next_first = next.serve(case)?;
    turn(steps, n, case, &session, "half roll", None)?;
    turn(steps, next, case, &session, "half roll", None)?;

    // rollback: N+1 retires; N comes back at a URI of its own.
    next_first.stop()?;
    n_first.stop()?;
    let n_again = n.serve(case)?;
    turn(steps, n, case, &session, "rollback", Some(n))?;

    // roll: N+1 comes back at a URI of its own, then N retires.
    let next_again = next.serve(case)?;
    n_again.stop()?;
    turn(steps, next, case, &session, "roll", Some(next))?;
    next_again.stop()?;
    Ok(())
}

#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just e2e-rolling` runs it"]
fn roll_and_rollback_smoke() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    ensure!(builds.n.label() == BuildLabel::N && builds.next.label() == BuildLabel::Next);
    println!(
        "N at generation {}, N+1 at generation {}",
        builds.n.identity().generation,
        builds.next.identity().generation
    );
    let temporary = tempfile::tempdir()?;
    let scratch = std::env::var_os(ARTIFACT_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    let mut steps = Vec::new();
    let rolled = roll(
        &mut steps,
        &builds,
        &Case::postgres("postgres", &services, &scratch)?,
    )
    .and_then(|()| {
        roll(
            &mut steps,
            &builds,
            &Case::sqlite("sqlite", &services, &scratch)?,
        )
    });
    let report = scratch.join("rolling-report.json");
    std::fs::write(&report, serde_json::to_vec_pretty(&steps)?)
        .with_context(|| format!("write {}", report.display()))?;
    rolled?;
    ensure!(steps.len() == 10, "expected ten turns, ran {}", steps.len());
    Ok(())
}
