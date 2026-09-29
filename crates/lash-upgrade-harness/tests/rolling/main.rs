//! `just e2e-rolling`: the ADR 0106 §6 choreography over the two Phase A
//! builds (ADR 0115 §6).
//!
//! Each roll runs on its own store, a PostgreSQL database of its own and then
//! a SQLite store directory, against one live `restate-server`, with every
//! turn on one session so each build reads what the other wrote:
//!
//! 1. **migrate:** lashctl provisions PostgreSQL; SQLite migrates on open.
//!    N serves the store and answers a turn.
//! 2. **half roll:** lashctl expands PostgreSQL again; N+1 serves beside N,
//!    and the forward drain of N's generation starts.
//! 3. **rollback:** mid-drain, the roll reverses: the forward drain ends, N+1
//!    drains and stops, and N registers at a fresh URI and answers.
//! 4. **roll:** N+1 registers at a fresh URI, N drains and stops; N+1
//!    answers.
//! 5. **finalize** (PostgreSQL, FIG-3800 B): finalize is refused while N's
//!    deployments are registered and while an operator hold stands, and
//!    contract is refused before finalize. Once N's deployments are removed
//!    and the hold is cleared, finalize moves `F`, runs the backfill, and
//!    contract raises the reader floor, which N then refuses.
//!
//! Every step must succeed: a refusal outside the ones the choreography
//! expects fails the run. Nothing is lost or duplicated: every turn is
//! answered, and every turn's model call ran exactly once.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::harness::{
    Case, LASHCTL_N_ENV, LASHCTL_NEXT_ENV, NodeBinary, NodeBuilds, Operator, Services,
};
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

/// Finalize after N's drain (FIG-3800 B): refused while N's deployments are
/// registered and while an operator holds it, then run, then contract.
fn finalize(
    case: &Case,
    n: &NodeBinary,
    operator_next: &Operator,
    n_generation: &str,
) -> Result<()> {
    let finalize = case.finalize_args(n_generation);
    let refusal = |body: &serde_json::Value| body["error"]["refusal"]["refusal"].clone();

    // Retirement is read from the engine: N's stopped deployments are still
    // registered, so finalize refuses and moves nothing.
    let (code, retained) = operator_next.answer_args(&finalize)?;
    ensure!(
        code == 3 && refusal(&retained) == "deployments_retained",
        "finalize ran with N's deployments registered: {retained}"
    );
    let removed = case.retire_generation(n_generation)?;
    ensure!(removed >= 1, "N registered no deployment to retire");

    // An operator hold keeps the rollback window open, and contract waits
    // for finalize.
    operator_next.run_args(&[
        "finalize-hold",
        "set",
        "--reason",
        "e2e-rolling holds the window open",
    ])?;
    let (code, held) = operator_next.answer_args(&finalize)?;
    ensure!(
        code == 3 && refusal(&held) == "held",
        "finalize ran under the hold: {held}"
    );
    let (code, early) = operator_next.answer_args(&["migrate", "--phase", "contract"])?;
    ensure!(
        code == 3 && refusal(&early) == "contract_before_finalize",
        "contract ran before finalize: {early}"
    );
    operator_next.run_args(&["finalize-hold", "clear"])?;

    let finalized = operator_next.run_args(&finalize)?;
    ensure!(
        finalized["flip"] == serde_json::json!({"outcome": "finalized", "from": 1, "to": 2}),
        "finalize did not move F from 1 to 2: {finalized}"
    );
    ensure!(
        finalized["backfills"].as_array().is_some_and(
            |steps| !steps.is_empty() && steps.iter().all(|step| step["state"] == "applied")
        ),
        "finalize did not complete the backfills: {finalized}"
    );
    operator_next.run("end-drain", Some(n_generation))?;
    let contracted = operator_next.run_args(&["migrate", "--phase", "contract"])?;
    ensure!(
        contracted["executed"]
            .as_array()
            .is_some_and(|steps| !steps.is_empty()),
        "contract ran nothing after its backfills: {contracted}"
    );
    let refusal = n.probe_refusal(case)?;
    ensure!(
        matches!(
            refusal,
            lash_core::compat::CompatRefusal::ReaderFloorAbove { min_reader: 2, .. }
        ),
        "N opened the contracted store with {refusal:?}"
    );
    Ok(())
}

fn roll(
    steps: &mut Vec<StepRecord>,
    builds: &NodeBuilds,
    case: &Case,
    postgres: bool,
) -> Result<()> {
    let (n, next) = (&builds.n, &builds.next);
    let session = case.session_id("rolling");
    // PostgreSQL steps are operator commands over the case's own database;
    // SQLite migrates on open, and `lashctl` does not reach it.
    let operators = if postgres {
        Some((
            Operator::for_case(case, LASHCTL_N_ENV)?,
            Operator::for_case(case, LASHCTL_NEXT_ENV)?,
        ))
    } else {
        None
    };

    // migrate
    if let Some((operator_n, _)) = &operators {
        operator_n.run("migrate", None)?;
        operator_n.run("preflight", None)?;
    }
    let n_first = n.serve(case)?;
    let n_generation = n_first
        .ready()
        .context("N ready report")?
        .generation
        .clone();
    if let Some((operator_n, _)) = &operators {
        operator_n.run("version", None)?;
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
    // answer is checked. The forward drain of N's generation starts.
    if let Some((_, operator_next)) = &operators {
        operator_next.run("migrate", None)?;
        operator_next.run("preflight", None)?;
    }
    let next_first = next.serve(case)?;
    if let Some((_, operator_next)) = &operators {
        operator_next.run("version", None)?;
    }
    let next_generation = next_first
        .ready()
        .context("N+1 ready report")?
        .generation
        .clone();
    ensure!(n_generation != next_generation, "N and N+1 have the same G");
    turn(steps, n, case, &session, "half roll", None)?;
    turn(steps, next, case, &session, "half roll", None)?;
    if let Some((_, operator_next)) = &operators {
        operator_next.run("drain", Some(&n_generation))?;
    }

    // rollback, mid-drain: the forward drain ends, N+1 drains and retires,
    // and N comes back at a URI of its own. Nothing was finalized, so N
    // serves everything N+1 wrote.
    if let Some((_, operator_next)) = &operators {
        operator_next.run("end-drain", Some(&n_generation))?;
        operator_next.run("drain", Some(&next_generation))?;
    }
    next_first.stop()?;
    if let Some((operator_n, operator_next)) = &operators {
        let status = operator_next.run("drain-status", Some(&next_generation))?;
        ensure!(status["drained"] == true, "N+1 did not drain: {status}");
        operator_next.run("end-drain", Some(&next_generation))?;
        operator_n.run("preflight", None)?;
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

    // roll: N+1 comes back at a URI of its own, then N drains and retires.
    if let Some((_, operator_next)) = &operators {
        operator_next.run("preflight", None)?;
    }
    let next_again = next.serve(case)?;
    if let Some((_, operator_next)) = &operators {
        operator_next.run("drain", Some(&n_generation))?;
    }
    n_again.stop()?;
    if let Some((_, operator_next)) = &operators {
        let status = operator_next.run("drain-status", Some(&n_generation))?;
        ensure!(status["drained"] == true, "N did not drain: {status}");
        finalize(case, n, operator_next, &n_generation)?;
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

    // Nothing lost or duplicated: every turn of this case was answered, and
    // each turn's model call ran exactly once, whichever build drove it.
    for record in steps.iter().filter(|record| record.case == case.name) {
        ensure!(
            record.report.status == "Answered",
            "{}: {} was not answered: {:?}",
            case.name,
            record.step,
            record.report
        );
    }
    let effects = case.effects()?;
    for step in ["migrate", "half roll", "rollback", "roll"] {
        for host in [n, next] {
            let marker = format!("{step} from {}", host.label());
            // The provider records the rendered message: the turn's text,
            // then the role that follows it.
            let calls = effects
                .iter()
                .filter(|effect| effect.message.starts_with(&format!("{marker} ")))
                .count();
            let expected = usize::from(
                step == "half roll" || (step == "roll") == (host.label() == BuildLabel::Next),
            );
            ensure!(
                calls == expected,
                "{}: `{marker}` ran {calls} model calls, not {expected}",
                case.name
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "needs both node builds, PostgreSQL and a restate-server: `just e2e-rolling` runs it"]
fn roll_and_rollback_smoke() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    ensure!(builds.n.label() == BuildLabel::N && builds.next.label() == BuildLabel::Next);
    let temporary = tempfile::tempdir()?;
    let scratch = std::env::var_os(ARTIFACT_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| temporary.path().to_path_buf());
    let mut steps = Vec::new();
    // The PostgreSQL roll finalizes, and `F` is one row per database, so it
    // runs on a database of its own.
    let rolled = roll(
        &mut steps,
        &builds,
        &Case::postgres_database("postgres", &services, &scratch)?,
        true,
    )
    .and_then(|()| {
        roll(
            &mut steps,
            &builds,
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
