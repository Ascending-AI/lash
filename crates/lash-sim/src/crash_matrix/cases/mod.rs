//! The scenarios behind the matrix's cells, one module per seam, and the
//! recovery loop they share.
//!
//! A seam's `stage` builds its world, drives the seam's workload up to the
//! cell's crash point, and hands back a [`Staged`] world whose crash has
//! happened (the deployment already restarted where the point kills it).
//! [`run`] then ticks the recovery interval until the invariants hold, and
//! checks the detection bound and that every live session still drives.

mod child_cancel;
mod definition;
mod definition_carry;
mod definition_create;
mod ingress;

pub use ingress::{
    a_turn_outlasts_an_outage_past_its_attempt_budget, a_waiter_follows_its_input_past_a_lost_ask,
};
mod intent;
pub(crate) mod process;
mod scope;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmOutputPart, LlmRequest, LlmResponse};

use super::invariants::{self, Expected};
use super::world::{CoreBuild, CrashWorld};
use super::{CaseReport, CaseSpec, Seam};

/// A case that runs longer than this in wall time fails.
const CASE_WALL_LIMIT: Duration = Duration::from_secs(300);

/// How long a stage waits in wall time for its crash point to fire.
pub(super) const TRIP_WAIT: Duration = Duration::from_secs(20);

/// A world whose crash happened, and what its end state must be.
pub(crate) struct Staged {
    pub world: CrashWorld,
    /// What the stage drew, for the report.
    pub notes: Vec<String>,
    pub expected: Expected,
    /// Virtual time the recovery bound counts from: the crash, or the start
    /// of a persistent fault. `None` when the crash point never fired.
    pub origin_ms: Option<u64>,
}

/// The one scripted model every case's deployments share: stateless, it
/// answers the latest user message with `answer:<root>;` for every
/// `input:<root>;` it carries, so a call a crash re-executes answers the
/// same, and inputs an admission batched into one message are each answered once.
/// A message naming a `held-` root is never answered: the call counts itself
/// in `held` and waits forever, holding its root live.
fn scripted_provider(held: Arc<AtomicUsize>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("crash-matrix")
        .complete(move |request: LlmRequest| {
            let held = Arc::clone(&held);
            async move {
                let latest_user = request
                    .messages
                    .iter()
                    .rev()
                    .find(|message| matches!(message.role, lash_core::llm::types::LlmRole::User))
                    .and_then(|message| serde_json::to_string(message).ok())
                    .unwrap_or_default();
                let roots = invariants::input_roots(&latest_user);
                if roots.iter().any(|root| root.starts_with("held-")) {
                    held.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
                let text: String = roots
                    .iter()
                    .map(|root| invariants::answer_text(root))
                    .collect();
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The model [`standard_core`] and [`held_core`] serve: what a seam's
/// sessions are created to run.
pub(crate) const MODEL: &str = "crash-matrix-model";

/// The standard-protocol core every seam's deployment runs.
pub(crate) fn standard_core() -> CoreBuild {
    held_core(Arc::new(AtomicUsize::new(0)))
}

/// [`standard_core`], counting the model calls it holds open in `held`.
pub(crate) fn held_core(held: Arc<AtomicUsize>) -> CoreBuild {
    Arc::new(move |backend, owner| {
        let model = lash_core::ModelMetadata::builder(MODEL)
            .context_window_tokens(200_000)
            .build()
            .map_err(|error| format!("model spec: {error}"))?;
        lash::LashCore::standard_builder(backend)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .recovery_lease(recovery_lease())
            .serve_test_model(scripted_provider(Arc::clone(&held)), model)
            .build(owner)
            .map_err(|error| format!("build the lash core: {error}"))
    })
}

/// The recovery leader lease of a matrix deployment. The lease renews on a
/// wall-clock cadence while the world advances the store clock by a whole
/// tick at a time, so a default 15 s TTL lapses in virtual time after two
/// ticks and a case that needs more (a lapsed claim, an attempt ceiling) would
/// lose its leader. A deployment the case kills resigns as it drops, so the
/// long TTL does not delay its successor.
fn recovery_lease() -> lash::RecoveryLeaseConfig {
    lash::RecoveryLeaseConfig {
        generation_rank: 0,
        timings: lash::RecoveryLeaseTimings {
            ttl: std::time::Duration::from_secs(24 * 60 * 60),
            ..lash::RecoveryLeaseTimings::default()
        },
    }
}

/// The session a case runs, unique per seed.
pub(crate) fn session_name(seam: Seam, seed: u64) -> lash_core::SessionId {
    lash_core::SessionId::from(format!("crash-{}-{seed:016x}", seam.kind_label()))
}

/// Wait for the armed crash point and, when it fired, kill the deployment
/// and bring up a fresh one. Answers the crash's virtual time.
pub(crate) async fn crash_and_restart(world: &CrashWorld) -> Result<Option<u64>, String> {
    let Some(tripped) = world.trip().wait(TRIP_WAIT).await else {
        return Ok(None);
    };
    world.crash_and_restart().await?;
    Ok(Some(tripped.at_ms))
}

/// Accept `root`'s input on `session` through the live deployment, as the
/// host's own work, and wait for the acceptance (not the turn). A retryable
/// refusal is retried, as a host retries it; a host that died inside the
/// send answers `Ok`, since the crash is the case's.
pub(crate) async fn send(
    world: &CrashWorld,
    session: &lash_core::SessionId,
    root: &str,
) -> Result<(), String> {
    send_text(world, session, root, &invariants::input_text(root)).await
}

/// [`send`] of `text` under the host id `root`.
async fn send_text(
    world: &CrashWorld,
    session: &lash_core::SessionId,
    root: &str,
    text: &str,
) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..20 {
        let core = world.core()?;
        let session = session.clone();
        let root = root.to_owned();
        let text = text.to_owned();
        let sent = world
            .host_op(async move {
                let session = crate::open_created_session(MODEL, &core, session).await?;
                session
                    .send(lash::TurnInput::text(text))
                    .id(root.as_str())
                    .await
                    .map(|_| ())
            })
            .await;
        match sent {
            None | Some(Ok(())) => return Ok(()),
            Some(Err(error)) if error.is_retryable() => {
                last = error.to_string();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Some(Err(error)) => return Err(format!("the send was refused: {error}")),
        }
    }
    Err(format!("the send stayed refused retryably: {last}"))
}

/// Run one seed of `spec`.
pub async fn run(spec: &CaseSpec, seed: u64) -> CaseReport {
    let mut report = CaseReport {
        seed,
        test_name: spec.test_name(),
        crashed: false,
        detected_after: None,
        violations: Vec::new(),
        notes: Vec::new(),
        ticks: 0,
        tick_times: Vec::new(),
    };
    match tokio::time::timeout(
        CASE_WALL_LIMIT,
        Box::pin(run_staged(spec, seed, &mut report)),
    )
    .await
    {
        Ok(()) => {}
        Err(_) => report.violations.push(format!(
            "the case ran past {CASE_WALL_LIMIT:?} of wall time"
        )),
    }
    report
}

async fn stage(spec: &CaseSpec, seed: u64) -> Result<Staged, String> {
    match spec.seam {
        Seam::Ingress => Box::pin(ingress::stage(spec.point, seed)).await,
        Seam::ControlIntent => Box::pin(intent::stage_close(spec.point, seed)).await,
        Seam::SessionDelete => Box::pin(intent::stage_delete(spec.point, seed)).await,
        Seam::ScopeClose => Box::pin(scope::stage_scope_close(spec.point, seed)).await,
        Seam::ParentEnd => Box::pin(scope::stage_parent_end(spec.point, seed)).await,
        Seam::ProcessStart => Box::pin(process::stage_start(spec.point, seed)).await,
        Seam::DefinitionStart => Box::pin(definition::stage(spec.point, seed)).await,
        Seam::DefinitionCreate => Box::pin(definition_create::stage(spec.point, seed)).await,
        Seam::DefinitionCarry => Box::pin(definition_carry::stage(spec.point, seed)).await,
        Seam::ProcessTerminal => Box::pin(process::stage(spec.point, seed)).await,
        Seam::ChildCancel => Box::pin(child_cancel::stage(spec.point, seed)).await,
    }
}

async fn run_staged(spec: &CaseSpec, seed: u64, report: &mut CaseReport) {
    let staged = match Box::pin(stage(spec, seed)).await {
        Ok(staged) => staged,
        Err(error) => {
            report.violations.push(format!("stage: {error}"));
            return;
        }
    };
    Box::pin(recover_staged(spec, seed, report, staged)).await;
}

async fn recover_staged(spec: &CaseSpec, seed: u64, report: &mut CaseReport, staged: Staged) {
    let Staged {
        world,
        notes,
        expected,
        origin_ms,
    } = staged;
    report.notes = notes;
    report.crashed = origin_ms.is_some();
    // A trip another cut recorded cannot stand in for the point this cell
    // armed: an armed crash site nothing reached never fired.
    for site in world.faults().unfired_crashes() {
        report
            .violations
            .push(format!("the armed crash site {site:?} never fired"));
    }
    let Some(origin_ms) = origin_ms else {
        report
            .violations
            .push(format!("the crash point {:?} never fired", spec.point));
        report
            .violations
            .extend(invariants::journal_names(&world).await);
        report
            .violations
            .extend(invariants::check(&world, &expected).await);
        report.violations.extend(invariants::diagnose(&world).await);
        world.finish().await;
        return;
    };
    // Both cuts stop before the close step records a result. Its immediate
    // delivery may already have claimed the row, so use that row's bound.
    let detection_bound = if spec.seam == Seam::ScopeClose
        && matches!(
            spec.point,
            super::CrashPoint::AfterStateCommit | super::CrashPoint::InvocationLost
        ) {
        match scope::obligation_at_restart(&world, seed).await {
            Ok(state) => {
                report
                    .notes
                    .push(format!("scope_close_at_restart={state:?}"));
                if state == lash_core::store::ObligationState::Claimed {
                    super::DetectionBound::LapsedClaim
                } else {
                    spec.bound
                }
            }
            Err(error) => {
                report.violations.push(error);
                world.finish().await;
                return;
            }
        }
    } else {
        spec.bound
    };
    if spec.bound == super::DetectionBound::AttemptCeiling {
        // Hundreds of ticks, each awaiting its own relay pass: the wait for
        // the engine to settle only lets host work the pass handed off land,
        // and the held root this world keeps open never settles.
        world.set_quiesce_budget(Duration::from_millis(20));
    }
    let min_tick = super::TICK - super::TICK / 10;
    let max_ticks = (detection_bound.limit().as_millis() / min_tick.as_millis()) as usize + 2;
    let mut last = Vec::new();
    for tick in 0..=max_ticks {
        world.quiesce().await;
        last = invariants::check(&world, &expected).await;
        if last.is_empty() {
            // Detected at the first tick after which every invariant held,
            // not after the harness's own wait for the engine to settle; with
            // no tick yet, at the check itself. The bound is judged in passes
            // from the tick the obligation became eligible at; the sim time
            // is reported beside it.
            let detected_ms = report.tick_times.last().map_or_else(
                || world.now_ms().saturating_sub(origin_ms),
                |at| u64::try_from(at.as_millis()).unwrap_or(u64::MAX),
            );
            report.detected_after = Some(Duration::from_millis(detected_ms));
            if let Err(violation) = detection_bound.judge(&report.tick_times, tick) {
                report.violations.push(violation);
            }
            break;
        }
        if tick < max_ticks {
            match world.tick().await {
                Ok(at_ms) => report
                    .tick_times
                    .push(Duration::from_millis(at_ms.saturating_sub(origin_ms))),
                Err(error) => {
                    report.violations.push(error);
                    break;
                }
            }
        }
    }
    report.ticks = world.ticks_run();
    if report.detected_after.is_none() {
        report.violations.push(format!(
            "the invariants never held within {max_ticks} tick(s) ({:?} of sim time after the crash)",
            Duration::from_millis(world.now_ms().saturating_sub(origin_ms))
        ));
        report.violations.extend(last);
        report.violations.extend(
            invariants::diagnose(&world)
                .await
                .into_iter()
                .map(|line| format!("diagnosis: {line}")),
        );
    } else {
        for (name, audit) in &expected.audits {
            report.violations.extend(
                audit(&world)
                    .await
                    .into_iter()
                    .map(|entry| format!("[{name}] {entry}")),
            );
        }
        report
            .violations
            .extend(invariants::probe_live_sessions(&world, &expected, 4).await);
        report.violations.extend(
            crate::invariants::check_crash_world(
                &world,
                &format!("crash-matrix/{}", spec.test_name()),
            )
            .await,
        );
    }
    world.finish().await;
}

/// The checker's red side, for a lost input: the ingress cell cut after its
/// state commit, with no recovery tick ever run. Answers the violations the
/// checker reports; a checker that reports none would pass a lost input.
pub async fn lost_input_control(seed: u64) -> Result<Vec<String>, String> {
    let Staged {
        world, expected, ..
    } = ingress::stage(super::CrashPoint::AfterStateCommit, seed).await?;
    world.quiesce().await;
    let violations = invariants::check(&world, &expected).await;
    world.finish().await;
    Ok(violations)
}

/// The checker's red side, for an input driven twice: one input's text is
/// accepted under two host ids, so two roots commit it. Answers the
/// violations the checker reports.
pub async fn double_drive_control(seed: u64) -> Result<Vec<String>, String> {
    let world = CrashWorld::new(seed, standard_core(), false).await?;
    world.restart().await?;
    let session = session_name(Seam::Ingress, seed);
    let text = invariants::input_text("twice");
    for id in ["twice-a", "twice-b"] {
        send_text(&world, &session, id, &text).await?;
        world.quiesce().await;
    }
    let expected = Expected {
        inputs: vec![invariants::AcceptedInput {
            session: session.clone(),
            root: lash_core::TurnId::from("twice"),
        }],
        live_sessions: vec![session],
        ..Expected::default()
    };
    let violations = invariants::check(&world, &expected).await;
    world.finish().await;
    Ok(violations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_claimed_after_commit_scope_close_uses_its_lapse_bound() {
        let seed = 0x2d5f_311b_c328_b88c;
        let spec = super::super::case(Seam::ScopeClose, super::super::CrashPoint::AfterStateCommit)
            .expect("registered scope-close cell");
        let staged =
            scope::stage_scope_close_claimed(super::super::CrashPoint::AfterStateCommit, seed)
                .await
                .expect("stage a claimed scope close");
        let mut report = CaseReport {
            seed,
            test_name: spec.test_name(),
            crashed: false,
            detected_after: None,
            violations: Vec::new(),
            notes: Vec::new(),
            ticks: 0,
            tick_times: Vec::new(),
        };
        Box::pin(tokio::time::timeout(
            CASE_WALL_LIMIT,
            recover_staged(spec, seed, &mut report, staged),
        ))
        .await
        .expect("the claimed scope close finished within the wall limit");
        assert!(report.crashed, "{report:#?}");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note == "scope_close_at_restart=Claimed"),
            "{report:#?}"
        );
        assert!(
            report
                .detected_after
                .is_some_and(|after| after > spec.bound.limit()),
            "the abandoned claim must outlive the due-row bound: {report:#?}"
        );
        assert!(report.passed(), "{report:#?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_claimed_invocation_lost_scope_close_uses_its_lapse_bound() {
        let seed = 0xa1aa_32da_93f8_dc1b;
        let point = super::super::CrashPoint::InvocationLost;
        let spec =
            super::super::case(Seam::ScopeClose, point).expect("registered scope-close cell");
        let staged = scope::stage_scope_close_claimed(point, seed)
            .await
            .expect("stage a claimed scope close after invocation loss");
        let mut report = CaseReport {
            seed,
            test_name: spec.test_name(),
            crashed: false,
            detected_after: None,
            violations: Vec::new(),
            notes: Vec::new(),
            ticks: 0,
            tick_times: Vec::new(),
        };
        Box::pin(tokio::time::timeout(
            CASE_WALL_LIMIT,
            recover_staged(spec, seed, &mut report, staged),
        ))
        .await
        .expect("the claimed scope close finished within the wall limit");
        assert!(report.crashed, "{report:#?}");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note == "scope_close_at_restart=Claimed"),
            "{report:#?}"
        );
        assert!(
            report
                .detected_after
                .is_some_and(|after| after > spec.bound.limit()),
            "the abandoned claim must outlive the due-row bound: {report:#?}"
        );
        assert!(report.passed(), "{report:#?}");
    }

    /// Recover `staged` with the harness stalling `stall` before every tick,
    /// as FIG-4161's crash run did on a host whose Restate server logged
    /// stalls of up to 48 s.
    async fn recover_under_harness_stalls(
        spec: &CaseSpec,
        seed: u64,
        staged: Staged,
        stall: Duration,
    ) -> CaseReport {
        let mut report = CaseReport {
            seed,
            test_name: spec.test_name(),
            crashed: false,
            detected_after: None,
            violations: Vec::new(),
            notes: Vec::new(),
            ticks: 0,
            tick_times: Vec::new(),
        };
        staged.world.stall_harness_before_each_tick(stall);
        Box::pin(tokio::time::timeout(
            CASE_WALL_LIMIT,
            recover_staged(spec, seed, &mut report, staged),
        ))
        .await
        .expect("the stalled recovery finished within the wall limit");
        report
    }

    /// FIG-4161's ingress seed with the harness stalling 25 s before every
    /// tick: the claim lapses between the second and the third tick, the
    /// third retakes it, and the input is driven once. The recovery lands
    /// past the §1.8 bound in sim time only because the stalls moved the
    /// clock; in passes it meets the bound (FIG-4309).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lapsed_ingress_claim_meets_its_bound_under_harness_stalls() {
        let seed = 0x5ead_8d74_e2c9_07a4;
        let point = super::super::CrashPoint::AfterStateCommit;
        let spec = super::super::case(Seam::Ingress, point).expect("registered ingress cell");
        let staged = ingress::stage(point, seed)
            .await
            .expect("stage the ingress cell");
        let report =
            recover_under_harness_stalls(spec, seed, staged, Duration::from_secs(25)).await;
        assert!(report.crashed, "{report:#?}");
        assert!(
            report
                .detected_after
                .is_some_and(|after| after > spec.bound.limit()),
            "the stalls must carry the recovery past the store-time bound: {report:#?}"
        );
        assert!(report.passed(), "{report:#?}");
    }

    /// FIG-4161's 131 s session-delete seed with the harness stalling 45 s
    /// before every tick: the close's lapsed claim is retaken and the delete
    /// it arms is delivered within the bound's two passes (FIG-4309).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_lapsed_session_delete_meets_its_bound_under_harness_stalls() {
        let seed = 0x58e4_b8c5_ff7e_d3c9;
        let point = super::super::CrashPoint::AfterStateCommit;
        let spec =
            super::super::case(Seam::SessionDelete, point).expect("registered session-delete cell");
        let staged = intent::stage_delete(point, seed)
            .await
            .expect("stage the session-delete cell");
        let report =
            recover_under_harness_stalls(spec, seed, staged, Duration::from_secs(45)).await;
        assert!(report.crashed, "{report:#?}");
        assert!(
            report
                .detected_after
                .is_some_and(|after| after > spec.bound.limit()),
            "the stalls must carry the recovery past the store-time bound: {report:#?}"
        );
        assert!(report.passed(), "{report:#?}");
    }
}
