//! The chaos soak (FIG-3873): randomized lash workloads under deployment
//! kills, leader-lease loss and rolling deploys on the in-process Restate
//! server double, checked against the crash matrix's end-state invariants.
//! The harness is `lash_sim::chaos_soak`; its module docs say what an epoch
//! draws and checks, and how a failed epoch replays from its seed.
//!
//! `chaos_soak_smoke` is the short mode: two epochs, about two minutes, over
//! every step kind no open finding exposes. `chaos_soak_release` is the
//! release gate's 90-minute soak over every step kind (`just chaos-soak`),
//! ignored in every ordinary run. Each finding has a regression test here
//! that replays it: ignored while it is open
//! (`lash_sim::chaos_soak::findings::OPEN`), live once it is fixed
//! (`findings::FIXED`). The change that fixes one moves its entry and deletes
//! its `ignore`, and [`every_finding_has_one_regression_test`] refuses a
//! mismatch.

use std::time::Duration;

use lash_sim::chaos_soak::{self, SoakConfig, findings};

/// The smoke seed when `LASH_CHAOS_SOAK_SEED` is unset.
const SMOKE_SEED: u64 = 0x3873_0001;

/// Epochs of the smoke mode when `LASH_CHAOS_SOAK_EPOCHS` is unset.
const SMOKE_EPOCHS: usize = 2;

/// The smoke mode's wall-time cap: it starts no epoch past it.
const SMOKE_CAP: Duration = Duration::from_secs(4 * 60);

fn assert_green(report: &chaos_soak::SoakReport) {
    let failed = report.failed();
    assert!(
        failed.is_empty(),
        "{} of {} chaos-soak epoch(s) failed:\n{}",
        failed.len(),
        report.epochs.len(),
        failed
            .iter()
            .map(|epoch| epoch.evidence())
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chaos_soak_smoke() {
    let mut config = SoakConfig::from_env(SMOKE_SEED, SMOKE_CAP, Some(SMOKE_EPOCHS));
    for kind in findings::smoke_without() {
        if !config.without.contains(&kind) {
            config.without.push(kind);
        }
    }
    let report = Box::pin(chaos_soak::run(config)).await;
    assert_green(&report);
}

#[ignore = "the release gate's 1-2 h chaos soak: run it with `just chaos-soak`"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chaos_soak_release() {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x3873, |now| now.as_nanos() as u64);
    let config = SoakConfig::from_env(seed, Duration::from_secs(90 * 60), None);
    let report = Box::pin(chaos_soak::run(config)).await;
    assert_green(&report);
}

macro_rules! regressions {
    (@reason $reason:literal) => { Some($reason) };
    (@reason) => { None };
    ($( $(#[ignore = $reason:literal])? $name:ident => $id:literal; )*) => {
        $(
            $(#[ignore = $reason])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn $name() {
                let finding = findings::OPEN
                    .iter()
                    .chain(findings::FIXED)
                    .find(|finding| finding.id == $id)
                    .unwrap_or_else(|| panic!("{} is not a finding", $id));
                let epoch = finding.replay().await;
                println!("{}", epoch.evidence());
                assert!(epoch.passed(), "{}: {}\n{}", finding.id, finding.summary, epoch.evidence());
            }
        )*

        /// Every generated regression test: its finding id and its ignore
        /// reason, `None` for a live test.
        const REGRESSIONS: &[(&str, Option<&str>)] = &[$(($id, regressions!(@reason $($reason)?))),*];
    };
}

regressions! {
    s1_queued_command_wedges_the_head_root => "FIG-3873 S1";
    #[ignore = "FIG-3873 S2: a crashed queued-work root replays into a journal mismatch"]
    s2_crashed_queued_work_root_diverges_on_replay => "FIG-3873 S2";
    s3_delete_with_an_orphaned_root_stays_due => "FIG-3873 S3";
    s4_interrupted_delete_leaks_the_cancel_gate_wait => "FIG-3873 S4";
    s5_cancelled_root_scope_close_stays_claimed_after_a_kill => "FIG-3873 S5";
}

/// The regression tests and the findings agree: one test per finding,
/// ignored under the finding's id while it is open and live once it is
/// fixed.
#[test]
fn every_finding_has_one_regression_test() {
    for (finding, open) in findings::OPEN
        .iter()
        .map(|finding| (finding, true))
        .chain(findings::FIXED.iter().map(|finding| (finding, false)))
    {
        let tests: Vec<_> = REGRESSIONS
            .iter()
            .filter(|(id, _)| *id == finding.id)
            .collect();
        assert_eq!(
            tests.len(),
            1,
            "{} has {} regression test(s)",
            finding.id,
            tests.len()
        );
        match (open, tests[0].1) {
            (true, Some(reason)) => assert!(
                reason.starts_with(finding.id),
                "{}'s regression test is ignored as `{reason}`",
                finding.id
            ),
            (true, None) => panic!("{} is open, but its regression test runs", finding.id),
            (false, Some(reason)) => panic!(
                "{} is fixed, but its regression test is ignored as `{reason}`",
                finding.id
            ),
            (false, None) => {}
        }
    }
    for (id, _) in REGRESSIONS {
        assert!(
            findings::OPEN
                .iter()
                .chain(findings::FIXED)
                .any(|finding| finding.id == *id),
            "a regression test names {id}, which is not a finding"
        );
    }
}
