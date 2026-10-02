//! The crash-point matrix (FIG-3849): the 1.0 durability gate.
//!
//! One test per registered cell of {seam or obligation kind} × {crash point},
//! each running its cell's seeds (`LASH_CRASH_MATRIX_SEEDS`, default 3) on the
//! in-process Restate server double, or with `LASH_CRASH_MATRIX_ENGINE=live`
//! on a live `restate-server` (`just crash-matrix-restate-e2e`): the
//! deployment dies at the cell's crash point, a fresh one comes up, and the
//! recovery interval ticks until every input was driven exactly once, every
//! obligation settled or stalled typed, no child is orphaned, no session
//! wedged, every terminal root's scope closed, within the ADR 0109 §1.8
//! bound. The harness is `lash_sim::crash_matrix`; its module docs say how a
//! cell is built, and `lash_sim::crash_matrix::engine` how each engine runs
//! it.
//!
//! A cell today's `main` cannot pass is ignored with what makes it pass: the
//! S8 slice (`FIG-3600 S8-<slice>`), or a defect the matrix found
//! (`FIG-3849 F<n>`) and the slice after it. Whoever lands that change
//! deletes the `ignore` and flips the cell's activation in
//! `lash_sim::crash_matrix::MATRIX`;
//! [`every_registered_cell_has_one_generated_test`] refuses a mismatch.

use lash_sim::crash_matrix::{self, CrashPoint, MATRIX, Seam};

macro_rules! crash_matrix {
    ($( $(#[ignore = $reason:literal])? $name:ident => ($seam:ident, $point:ident); )*) => {
        $(
            $(#[ignore = $reason])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                crash_matrix::assert_cell(Seam::$seam, CrashPoint::$point).await;
            }
        )*

        mod postgres {
            $(
                #[test]
                #[ignore = "requires PostgreSQL; select inside a pg16 gate"]
                fn $name() {
                    super::run_postgres_variant(stringify!($name));
                }
            )*
        }

        /// Every generated test: its name, its cell and its ignore reason.
        const GENERATED: &[(&str, Seam, CrashPoint, Option<&str>)] = &[
            $((
                stringify!($name),
                Seam::$seam,
                CrashPoint::$point,
                {
                    #[allow(unused_mut, unused_assignments)]
                    let mut reason: Option<&str> = None;
                    $(reason = Some($reason);)?
                    reason
                },
            )),*
        ];
    };
}

crash_matrix! {
    ingress_after_state_commit => (Ingress, AfterStateCommit);
    ingress_during_engine_delivery => (Ingress, DuringEngineDelivery);
    ingress_after_delivery_before_settle => (Ingress, AfterDeliveryBeforeSettle);
    ingress_mid_journal_step => (Ingress, MidJournalStep);
    ingress_invocation_lost => (Ingress, InvocationLost);

    control_intent_after_state_commit => (ControlIntent, AfterStateCommit);
    control_intent_during_engine_delivery => (ControlIntent, DuringEngineDelivery);
    control_intent_after_delivery_before_settle => (ControlIntent, AfterDeliveryBeforeSettle);
    control_intent_delivery_retryable_forever => (ControlIntent, DeliveryRetryableForever);

    scope_close_after_state_commit => (ScopeClose, AfterStateCommit);
    scope_close_during_engine_delivery => (ScopeClose, DuringEngineDelivery);
    scope_close_after_delivery_before_settle => (ScopeClose, AfterDeliveryBeforeSettle);
    scope_close_mid_journal_step => (ScopeClose, MidJournalStep);
    scope_close_invocation_lost => (ScopeClose, InvocationLost);

    parent_end_after_state_commit => (ParentEnd, AfterStateCommit);
    parent_end_during_engine_delivery => (ParentEnd, DuringEngineDelivery);
    parent_end_after_delivery_before_settle => (ParentEnd, AfterDeliveryBeforeSettle);
    parent_end_delivery_refused => (ParentEnd, DeliveryRefused);

    session_delete_after_state_commit => (SessionDelete, AfterStateCommit);
    session_delete_after_delivery_before_settle => (SessionDelete, AfterDeliveryBeforeSettle);

    process_start_after_state_commit => (ProcessStart, AfterStateCommit);
    definition_start_during_engine_delivery => (DefinitionStart, DuringEngineDelivery);
    definition_start_mid_journal_step => (DefinitionStart, MidJournalStep);
    definition_start_after_state_commit => (DefinitionStart, AfterStateCommit);
    definition_create_mid_journal_step => (DefinitionCreate, MidJournalStep);
    definition_create_after_state_commit => (DefinitionCreate, AfterStateCommit);
    definition_create_after_delivery_before_settle => (DefinitionCreate, AfterDeliveryBeforeSettle);
    definition_carry_mid_journal_step => (DefinitionCarry, MidJournalStep);
    definition_carry_after_state_commit => (DefinitionCarry, AfterStateCommit);
    process_terminal_mid_journal_step => (ProcessTerminal, MidJournalStep);
    process_terminal_invocation_lost => (ProcessTerminal, InvocationLost);
    process_terminal_caller_killed => (ProcessTerminal, CallerKilled);

    child_cancel_mid_journal_step => (ChildCancel, MidJournalStep);
    child_cancel_during_engine_delivery => (ChildCancel, DuringEngineDelivery);
}

/// The generated tests and the registry agree: every registered cell has
/// exactly one test named for it, and a test is ignored exactly when its
/// cell waits for an S8 slice, with that slice's reason.
#[test]
fn every_registered_cell_has_one_generated_test() {
    for spec in MATRIX {
        let tests: Vec<_> = GENERATED
            .iter()
            .filter(|(_, seam, point, _)| *seam == spec.seam && *point == spec.point)
            .collect();
        assert_eq!(
            tests.len(),
            1,
            "cell {:?} × {:?} has {} generated test(s)",
            spec.seam,
            spec.point,
            tests.len()
        );
        let (name, _, _, reason) = tests[0];
        assert_eq!(*name, spec.test_name(), "the test is named for its cell");
        assert_eq!(
            reason.map(str::to_owned),
            spec.activation.ignore_reason(),
            "`{name}`'s ignore reason matches its cell's activation"
        );
    }
    for (name, seam, point, _) in GENERATED {
        assert!(
            crash_matrix::case(*seam, *point).is_some(),
            "`{name}` tests an unregistered cell"
        );
    }
    for seam in Seam::ALL {
        assert!(
            MATRIX.iter().any(|spec| spec.seam == seam),
            "{seam:?} has no cell"
        );
    }
}

/// The checker's red side: an input the recovery never drove is reported
/// lost, and one input committed by two roots is reported driven twice. A
/// checker that passed either would pass the matrix vacuously.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_checker_reports_a_lost_input_and_a_double_drive() {
    let lost = Box::pin(crash_matrix::cases::lost_input_control(0x3849_0001))
        .await
        .expect("stage the lost-input control");
    assert!(
        lost.iter()
            .any(|violation| violation.contains("committed 0 time(s)"))
            && lost
                .iter()
                .any(|violation| violation.contains("open ingress row")),
        "a lost input is reported: {lost:#?}"
    );
    let twice = Box::pin(crash_matrix::cases::double_drive_control(0x3849_0002))
        .await
        .expect("stage the double-drive control");
    assert!(
        twice
            .iter()
            .any(|violation| violation.contains("committed 2 time(s)")),
        "an input driven twice is reported: {twice:#?}"
    );
}

/// FIG-3879: a waiter on an input whose drive the engine lost before it
/// admitted anything follows the relay's ask under the next attempt to the
/// input's answer, instead of waiting on the lost drive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_waiter_follows_its_input_past_a_lost_ask() {
    let violations =
        Box::pin(crash_matrix::cases::a_waiter_follows_its_input_past_a_lost_ask(0x3879_0005))
            .await
            .expect("stage the lost ask");
    assert!(violations.is_empty(), "{violations:#?}");
}

/// FIG-4784: a turn whose deployment stays down for more dispatches than a
/// turn handler's retry policy allows is still retrying when the next
/// deployment comes up, and commits its answer there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_outlasts_an_outage_past_its_attempt_budget() {
    let violations = Box::pin(
        crash_matrix::cases::a_turn_outlasts_an_outage_past_its_attempt_budget(0x4784_0001),
    )
    .await
    .expect("stage the outage");
    assert!(violations.is_empty(), "{violations:#?}");
}

/// The retry-forever cell uses a settle budget below the live poll interval.
#[tokio::test]
#[ignore = "requires an isolated Restate server"]
async fn live_short_settle_budget_is_bounded() {
    use crash_matrix::engine::{Engine, EngineKind};
    use std::time::Duration;

    let kind = EngineKind::from_env().expect("the live engine is configured");
    assert!(matches!(kind, EngineKind::Live(_)));
    let engine = Engine::start(&kind, 4278, lash_restate_test::DeploymentHooks::default())
        .await
        .expect("the live engine starts");
    let budget = Duration::from_millis(20);
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    engine.settle(budget).await;
    let elapsed = started.elapsed();
    tokio::time::resume();
    engine.finish().await;
    assert!(
        elapsed <= budget + Duration::from_millis(1),
        "settle exceeded its {budget:?} budget plus one timer tick: {elapsed:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "a service variant must execute its matrix law in a child process"
)]
fn run_postgres_variant(law: &str) {
    let url = lash_postgres_store::testing::required_database_url();
    let output = std::process::Command::new(std::env::current_exe().expect("matrix executable"))
        .args(["--exact", law, "--include-ignored", "--nocapture"])
        .env("LASH_POSTGRES_DATABASE_URL", url)
        .env("LASH_CRASH_MATRIX_STORE", "postgres")
        .env("LASH_CRASH_MATRIX_ENGINE", "double")
        .output()
        .expect("run the PostgreSQL matrix variant");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("running 1 test"), "{stdout}\n{stderr}");
    assert!(output.status.success(), "{stdout}\n{stderr}");
}
