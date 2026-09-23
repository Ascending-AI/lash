//! ADR 0099 §11 clause 4 on the product path: every timer aggregate is its own
//! group, with its own admission (FIG-3397).
//!
//! A timer-only aggregate has no tool call to name it, so its group identity
//! must come from the timers themselves and from the site that awaits them.
//! Two awaits that shared one identity would share one group: the second would
//! reopen the first — answering at once from its recorded settlement when the
//! durations match, or failing the reopen fence when they do not.
//!
//! "Waits" is asserted as a lower bound on the turn's wall time, which a timer
//! can only ever lengthen: no ordering is inferred from it.

use super::*;

/// One timer's duration. Long enough that a skipped wait cannot hide inside
/// the turn's own overhead.
const TIMER_MS: u64 = 400;

async fn elapsed_turn(
    tier: &JournaledTier,
    session_id: &str,
    cells: Vec<String>,
) -> Result<(OracleRun, std::time::Duration)> {
    let started = std::time::Instant::now();
    let run = run_cells(tier, session_id, cells).await?;
    Ok((run, started.elapsed()))
}

/// Two awaited timer aggregates at two sites of one cell, with equal and with
/// different durations, each written both as a one-leaf race and as an
/// unawaited `sleep` awaited later. Each one waits its own duration.
async fn each_timer_aggregate_in_a_cell_waits_on_its_own(tier: &JournaledTier) -> Result<()> {
    let short = TIMER_MS / 2;
    for (label, cell, waited_ms) in [
        (
            "equal races",
            format!(
                "await Promise.race([sleep({TIMER_MS})]);\nawait Promise.race([sleep({TIMER_MS})]);\nfinish(\"done\");"
            ),
            2 * TIMER_MS,
        ),
        (
            "different races",
            format!(
                "await Promise.race([sleep({short})]);\nawait Promise.race([sleep({TIMER_MS})]);\nfinish(\"done\");"
            ),
            short + TIMER_MS,
        ),
        (
            "equal handles",
            format!(
                "const first = sleep({TIMER_MS});\nawait first;\nconst second = sleep({TIMER_MS});\nawait second;\nfinish(\"done\");"
            ),
            2 * TIMER_MS,
        ),
        (
            "different handles",
            format!(
                "const first = sleep({short});\nawait first;\nconst second = sleep({TIMER_MS});\nawait second;\nfinish(\"done\");"
            ),
            short + TIMER_MS,
        ),
    ] {
        let session_id = format!(
            "aggregate-oracle-timer-identity-{}",
            label.replace(' ', "-")
        );
        let (run, elapsed) = elapsed_turn(tier, &session_id, vec![typescript_block(&cell)]).await?;
        assert_eq!(
            run.final_value(),
            &serde_json::json!("done"),
            "{}/{label}: both timers settle as their own aggregates",
            tier.name
        );
        assert!(
            elapsed >= std::time::Duration::from_millis(waited_ms),
            "{}/{label}: the second timer waited its own duration, not the first's \
             recorded settlement: the turn took {elapsed:?}, below {waited_ms}ms",
            tier.name
        );
    }
    Ok(())
}

/// The same timer aggregate in two cells of one turn: each cell's await waits.
async fn a_timer_aggregate_in_each_of_two_cells_waits_twice(tier: &JournaledTier) -> Result<()> {
    let (run, elapsed) = elapsed_turn(
        tier,
        "aggregate-oracle-timer-identity-cells",
        vec![
            typescript_block(&format!("await Promise.race([sleep({TIMER_MS})]);")),
            typescript_block(&format!(
                "await Promise.race([sleep({TIMER_MS})]);\nfinish(\"done\");"
            )),
        ],
    )
    .await?;
    assert_eq!(
        run.final_value(),
        &serde_json::json!("done"),
        "{}: the second cell's timer settles",
        tier.name
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(2 * TIMER_MS),
        "{}: each cell's timer waited: the turn took {elapsed:?}",
        tier.name
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_each_timer_aggregate_in_a_cell_waits_on_its_own() -> Result<()> {
    each_timer_aggregate_in_a_cell_waits_on_its_own(&sqlite()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_timer_aggregate_in_each_of_two_cells_waits_twice() -> Result<()> {
    a_timer_aggregate_in_each_of_two_cells_waits_twice(&sqlite()).await
}
