//! `just e2e-rolling`: the ADR 0106 §6 choreography over the two Phase A
//! builds (ADR 0115 §6).
//!
//! Each roll runs on its own store, PostgreSQL and then a SQLite store
//! directory, against one live `restate-server`, with every turn on one
//! session so each build reads what the other wrote:
//!
//! 1. **migrate:** lashctl provisions PostgreSQL; SQLite migrates on open.
//!    N serves the store and answers a turn.
//! 2. **half roll:** lashctl expands PostgreSQL again; N+1 serves beside N.
//! 3. **rollback:** N+1 stops; N registers at a fresh URI and answers.
//! 4. **roll:** N+1 registers at a fresh URI, N stops; N+1 answers.
//!
//! Every step must succeed: a refusal fails the run. Finalize waits for
//! FIG-3800 B, and the Phase A legs wait for their owning lanes.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::harness::{Case, NodeBinary, NodeBuilds, Operator, Services};
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
    driver: Option<(&NodeBinary, &str)>,
) -> Result<()> {
    let report = host.turn(case, session, &format!("{step} from {}", host.label()))?;
    println!(
        "{}: {step}: host {} got {:?}",
        case.name, report.host, report.reply
    );
    if let Some((driver, generation)) = driver {
        let expected = served_by(driver.label(), generation);
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
        expected_driver: driver.map(|(node, _)| node.label()),
        report,
    });
    Ok(())
}

fn roll(
    steps: &mut Vec<StepRecord>,
    builds: &NodeBuilds,
    operator: &Operator,
    case: &Case,
    postgres: bool,
) -> Result<()> {
    let (n, next) = (&builds.n, &builds.next);
    let session = case.session_id("rolling");

    // PostgreSQL migrations are operator commands. SQLite migrates on open.
    if postgres {
        operator.run("migrate", None)?;
        operator.run("preflight", None)?;
    }
    let n_first = n.serve(case)?;
    let n_generation = n_first
        .ready()
        .context("N ready report")?
        .generation
        .clone();
    if postgres {
        operator.run("version", None)?;
    }
    turn(
        steps,
        n,
        case,
        &session,
        "migrate",
        Some((n, &n_generation)),
    )?;

    // half roll: N+1 serves beside N. Which build drives each host's turn
    // depends on Restate's routing between the two deployments, so only the
    // answer is checked.
    if postgres {
        operator.run("migrate", None)?;
        operator.run("preflight", None)?;
    }
    let next_first = next.serve(case)?;
    let next_generation = next_first
        .ready()
        .context("N+1 ready report")?
        .generation
        .clone();
    ensure!(n_generation != next_generation, "N and N+1 have the same G");
    turn(steps, n, case, &session, "half roll", None)?;
    turn(steps, next, case, &session, "half roll", None)?;

    // rollback: N+1 retires; N comes back at a URI of its own.
    if postgres {
        operator.run("drain", Some(&next_generation))?;
    }
    next_first.stop()?;
    if postgres {
        let status = operator.run("drain-status", Some(&next_generation))?;
        ensure!(status["drained"] == true, "N+1 did not drain: {status}");
        operator.run("end-drain", Some(&next_generation))?;
        operator.run("preflight", None)?;
    }
    n_first.stop()?;
    let n_again = n.serve(case)?;
    turn(
        steps,
        n,
        case,
        &session,
        "rollback",
        Some((n, &n_generation)),
    )?;

    // roll: N+1 comes back at a URI of its own, then N retires.
    if postgres {
        operator.run("preflight", None)?;
    }
    let next_again = next.serve(case)?;
    if postgres {
        operator.run("drain", Some(&n_generation))?;
    }
    n_again.stop()?;
    if postgres {
        let status = operator.run("drain-status", Some(&n_generation))?;
        ensure!(status["drained"] == true, "N did not drain: {status}");
        operator.run("end-drain", Some(&n_generation))?;
    }
    turn(
        steps,
        next,
        case,
        &session,
        "roll",
        Some((next, &next_generation)),
    )?;
    next_again.stop()?;
    Ok(())
}

#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just e2e-rolling` runs it"]
fn roll_and_rollback_smoke() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    let operator = Operator::from_env(&services)?;
    ensure!(builds.n.label() == BuildLabel::N && builds.next.label() == BuildLabel::Next);
    let temporary = tempfile::tempdir()?;
    let scratch = std::env::var_os(ARTIFACT_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    let mut steps = Vec::new();
    let rolled = roll(
        &mut steps,
        &builds,
        &operator,
        &Case::postgres("postgres", &services, &scratch)?,
        true,
    )
    .and_then(|()| {
        roll(
            &mut steps,
            &builds,
            &operator,
            &Case::sqlite("sqlite", &services, &scratch)?,
            false,
        )
    });
    let report = scratch.join("rolling-report.json");
    std::fs::write(&report, serde_json::to_vec_pretty(&steps)?)
        .with_context(|| format!("write {}", report.display()))?;
    rolled?;
    ensure!(steps.len() == 10, "expected ten turns, ran {}", steps.len());
    Ok(())
}
