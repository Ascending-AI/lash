//! `just e2e-rolling`: the ADR 0106 §6 choreography over the two Phase A
//! builds (ADR 0115 §6).
//!
//! Each roll runs on its own store, a PostgreSQL database of its own and then
//! a SQLite store directory, against one live `restate-server`, with every
//! turn on one session so each build reads what the other wrote:
//!
//! 1. **migrate:** lashctl provisions PostgreSQL; SQLite migrates on open.
//!    N serves the store and answers a turn.
//! 2. **half roll** (PostgreSQL): lashctl expands again; N+1 serves beside N,
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
//! SQLite instead drains and stops N before N+1 opens and migrates. N+1's
//! open backs up all three databases before it migrates them (FIG-3801),
//! and nothing after it migrates again. It rolls back by draining and
//! stopping N+1 before N reopens, then re-rolls, finalizes, and requires the
//! old writer to refuse. Every SQLite turn checks the serving build and
//! generation. SQLite has no separate contract.
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

fn roll_postgres(steps: &mut Vec<StepRecord>, builds: &NodeBuilds, case: &Case) -> Result<()> {
    let (n, next) = (&builds.n, &builds.next);
    let session = case.session_id("rolling");
    let (operator_n, operator_next) = (
        Operator::for_case(case, LASHCTL_N_ENV)?,
        Operator::for_case(case, LASHCTL_NEXT_ENV)?,
    );

    // migrate
    operator_n.run("migrate", None)?;
    operator_n.run("preflight", None)?;
    let n_first = n.serve(case)?;
    let n_generation = n_first
        .ready()
        .context("N ready report")?
        .generation
        .clone();
    operator_n.run("version", None)?;
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
    operator_next.run("migrate", None)?;
    operator_next.run("preflight", None)?;
    let next_first = next.serve(case)?;
    operator_next.run("version", None)?;
    let next_generation = next_first
        .ready()
        .context("N+1 ready report")?
        .generation
        .clone();
    ensure!(n_generation != next_generation, "N and N+1 have the same G");
    turn(steps, n, case, &session, "half roll", None)?;
    turn(steps, next, case, &session, "half roll", None)?;
    operator_next.run("drain", Some(&n_generation))?;

    // rollback, mid-drain: the forward drain ends, N+1 drains and retires,
    // and N comes back at a URI of its own. Nothing was finalized, so N
    // serves everything N+1 wrote.
    operator_next.run("end-drain", Some(&n_generation))?;
    operator_next.run("drain", Some(&next_generation))?;
    next_first.stop()?;
    let status = operator_next.run_args(&case.drain_status_args(&next_generation))?;
    ensure!(status["drained"] == true, "N+1 did not drain: {status}");
    operator_next.run("end-drain", Some(&next_generation))?;
    operator_n.run("preflight", None)?;
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
    operator_next.run("preflight", None)?;
    let next_again = next.serve(case)?;
    operator_next.run("drain", Some(&n_generation))?;
    n_again.stop()?;
    let status = operator_next.run_args(&case.drain_status_args(&n_generation))?;
    ensure!(status["drained"] == true, "N did not drain: {status}");
    finalize(case, n, &operator_next, &n_generation)?;
    turn(
        steps,
        next,
        case,
        &session,
        "roll",
        Some((next, &next_generation)),
    )?;
    next_again.stop()?;

    verify_turns(steps, case)
}

fn verify_turns(steps: &[StepRecord], case: &Case) -> Result<()> {
    let effects = case.effects()?;
    let records: Vec<_> = steps
        .iter()
        .filter(|record| record.case == case.name)
        .collect();
    ensure!(
        effects.len() == records.len(),
        "{}: expected {} model calls, got {}",
        case.name,
        records.len(),
        effects.len()
    );
    for record in records {
        ensure!(
            record.report.status == "Answered",
            "{}: {} was not answered: {:?}",
            case.name,
            record.step,
            record.report
        );
        let marker = format!("{} from {} ", record.step, record.report.host);
        let calls = effects
            .iter()
            .filter(|effect| effect.message.starts_with(&marker))
            .count();
        ensure!(
            calls == 1,
            "{}: `{marker}` ran {calls} model calls",
            case.name
        );
    }
    Ok(())
}

fn sqlite_drain(node: &NodeBinary, case: &Case, session: &str, generation: &str) -> Result<()> {
    use lash_upgrade_harness::harness::{block_on, wait_for};
    let view = case.view()?;
    wait_for("the SQLite session's invocations to finish", || {
        Ok(block_on(view.live_invocations("LashSession", session))?
            .is_empty()
            .then_some(()))
    })?;
    wait_for("the SQLite generation to drain", || {
        let status = node.sqlite_upgrade(case, "drain", generation)?;
        ensure!(
            status["generation"] == generation,
            "wrong drained generation: {status}"
        );
        Ok((status["drained"] == true).then_some(()))
    })
}

/// The SQLite store set's migration backups (FIG-3801): each open that
/// migrates first copies all three databases beside the store, into
/// `migration-backups/sqlite-backup-*/`, and records the migration in the
/// backup's `manifest.json`.
fn sqlite_backups(case: &Case) -> Result<Vec<serde_json::Value>> {
    let root = case
        .sqlite_dir()
        .context("SQLite directory")?
        .join("migration-backups");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut backups = Vec::new();
    for entry in std::fs::read_dir(&root).with_context(|| format!("read {}", root.display()))? {
        let directory = entry?.path();
        if !directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("sqlite-backup-"))
        {
            continue;
        }
        let manifest = directory.join("manifest.json");
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&manifest).with_context(|| format!("read {}", manifest.display()))?,
        )?;
        for database in manifest["databases"].as_array().into_iter().flatten() {
            let copy = directory.join(database["file"].as_str().context("a backed-up file")?);
            let bytes = std::fs::metadata(&copy)
                .with_context(|| format!("stat the backup copy {}", copy.display()))?
                .len();
            ensure!(
                Some(bytes) == database["bytes"].as_u64() && bytes > 0,
                "{}: the backup copy is {bytes} bytes, its manifest records {}",
                copy.display(),
                database["bytes"]
            );
        }
        backups.push(manifest);
    }
    Ok(backups)
}

/// N+1's first open migrated the store N provisioned, after one complete
/// backup of every database at N's versions; nothing since migrated again.
fn require_one_migration_backup(case: &Case, step: &str) -> Result<()> {
    let backups = sqlite_backups(case)?;
    ensure!(
        backups.len() == 1,
        "{}: {step}: expected N+1's one migration backup, found {backups:?}",
        case.name
    );
    let manifest = &backups[0];
    let databases = manifest["databases"]
        .as_array()
        .context("backed-up databases")?;
    ensure!(
        manifest["state"] == "migrated"
            && databases.len() == 3
            && databases
                .iter()
                .all(|database| database["from"].as_u64() < database["to"].as_u64()),
        "{}: {step}: the backup does not record a completed migration of all three databases: {manifest}",
        case.name
    );
    println!("{}: {step}: migration backup {manifest}", case.name);
    Ok(())
}

fn roll_sqlite(steps: &mut Vec<StepRecord>, builds: &NodeBuilds, case: &Case) -> Result<()> {
    let (n, next) = (&builds.n, &builds.next);
    let session = case.session_id("rolling");
    let n_first = n.serve(case)?;
    let n_generation = n_first.generation()?.to_owned();
    turn(
        steps,
        n,
        case,
        &session,
        "migrate",
        Some((n, &n_generation)),
    )?;
    sqlite_drain(n, case, &session, &n_generation)?;
    n_first.stop()?;
    ensure!(
        sqlite_backups(case)?.is_empty(),
        "{}: N's provisioning took a migration backup",
        case.name
    );

    let next_first = next.serve(case)?;
    let next_generation = next_first.generation()?.to_owned();
    ensure!(n_generation != next_generation, "N and N+1 have the same G");
    turn(
        steps,
        next,
        case,
        &session,
        "roll",
        Some((next, &next_generation)),
    )?;
    require_one_migration_backup(case, "roll")?;
    sqlite_drain(next, case, &session, &next_generation)?;
    next_first.stop()?;

    n.sqlite_upgrade(case, "end-drain", &n_generation)?;
    let n_again = n.serve(case)?;
    ensure!(
        n_again.generation()? == n_generation,
        "N changed generation on rollback"
    );
    turn(
        steps,
        n,
        case,
        &session,
        "rollback",
        Some((n, &n_generation)),
    )?;
    sqlite_drain(n, case, &session, &n_generation)?;
    n_again.stop()?;

    next.sqlite_upgrade(case, "end-drain", &next_generation)?;
    let next_again = next.serve(case)?;
    ensure!(
        next_again.generation()? == next_generation,
        "N+1 changed generation on re-roll"
    );
    turn(
        steps,
        next,
        case,
        &session,
        "re-roll",
        Some((next, &next_generation)),
    )?;
    // Finish the last pre-finalize invocation before retiring the old lanes.
    let view = case.view()?;
    lash_upgrade_harness::harness::wait_for("the SQLite pre-finalize turn to finish", || {
        Ok(
            lash_upgrade_harness::harness::block_on(
                view.live_invocations("LashSession", &session),
            )?
            .is_empty()
            .then_some(()),
        )
    })?;
    ensure!(
        case.retire_generation(&n_generation)? >= 1,
        "N had no deployment to retire"
    );
    let flip = next.sqlite_upgrade(case, "finalize", &n_generation)?;
    ensure!(
        flip == serde_json::json!({"outcome": "finalized", "from": 1, "to": 2}),
        "SQLite finalize did not move F: {flip}"
    );
    let refusal = n.probe_refusal(case)?;
    ensure!(
        matches!(
            refusal,
            lash_core::compat::CompatRefusal::FleetOutsideWritable { recorded: 2, .. }
        ),
        "N opened finalized SQLite with {refusal:?}"
    );
    println!("{}: old writer refused: {refusal:?}", case.name);
    turn(
        steps,
        next,
        case,
        &session,
        "finalize",
        Some((next, &next_generation)),
    )?;
    sqlite_drain(next, case, &session, &next_generation)?;
    next_again.stop()?;
    require_one_migration_backup(case, "finalize")?;
    verify_turns(steps, case)
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
    let rolled = roll_postgres(
        &mut steps,
        &builds,
        &Case::postgres_database("postgres", &services, &scratch)?,
    )
    .and_then(|()| {
        roll_sqlite(
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

#[test]
#[ignore = "needs both node builds and a restate-server"]
fn sqlite_migration_overlap_refused() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    let scratch = tempfile::tempdir()?;
    let case = Case::sqlite("sqlite-overlap", &services, scratch.path())?;
    let held = builds.n.serve(&case)?;
    let output =
        std::process::Command::new(std::env::var(lash_upgrade_harness::harness::NODE_NEXT_ENV)?)
            .arg("probe")
            .args([
                "--store",
                &format!(
                    "sqlite:{}",
                    case.sqlite_dir().context("SQLite directory")?.display()
                ),
                "--data-dir",
                scratch.path().to_str().context("scratch path")?,
            ])
            .output()?;
    ensure!(
        output.status.success(),
        "overlap probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        report["open_elsewhere"]["database"] == "durable core",
        "missing typed open-elsewhere refusal: {report}"
    );
    ensure!(
        report["open_elsewhere"]["location"]
            == case
                .sqlite_dir()
                .context("SQLite directory")?
                .to_string_lossy()
                .as_ref(),
        "wrong refused store: {report}"
    );
    held.stop()?;
    builds.next.probe(&case, None)?;
    Ok(())
}

#[test]
#[ignore = "needs both node builds and a restate-server"]
fn sqlite_stop_then_start_roll_and_rollback() -> Result<()> {
    let services = Services::from_env()?;
    let builds = NodeBuilds::from_env()?;
    let scratch = tempfile::tempdir()?;
    let case = Case::sqlite("sqlite-roll", &services, scratch.path())?;
    let mut steps = Vec::new();
    roll_sqlite(&mut steps, &builds, &case)?;
    ensure!(
        steps.iter().map(|record| record.step).collect::<Vec<_>>()
            == ["migrate", "roll", "rollback", "re-roll", "finalize"],
        "wrong SQLite sequence"
    );
    ensure!(
        steps.iter().all(|record| record.expected_driver.is_some()),
        "every SQLite turn needs its build and generation checked"
    );
    Ok(())
}
