//! ADR 0099 §9 on the product path: a completed group retires as a whole while
//! its opener stays live (FIG-3397).
//!
//! A race whose consumer stopped at its winner leaves the group with the
//! opener, and every child it admitted counts against the opener's retained
//! work until the group retires. Retiring only at the opener's end would make
//! a long-lived loop of races refuse itself once it had formed half the bound's
//! width in groups — here, at the 513th race of the default 1024-child bound —
//! although every one of those groups finished long ago.

use super::*;

/// Races of two quick leaves, far more of them than the opener's default bound
/// admits at once. Every loser settles, so every group can retire, and the
/// loop runs to its end.
async fn a_loop_of_races_well_past_the_bound_runs_to_its_end(tier: &JournaledTier) -> Result<()> {
    const RACES: usize = 600;
    // Every race journals its group and both leaves, so the loop is a
    // throughput case: it gets a deadlock budget sized for it. Alone it takes
    // about a minute on the pool; beside the rest of the unit suite it can
    // take more than two, so the budget leaves room for that contention while
    // staying inside the unit-test target's 300 s timeout.
    const LOOP_BUDGET: std::time::Duration = std::time::Duration::from_secs(240);
    let run = drive_cells(
        tier,
        "aggregate-oracle-opener-bound-loop",
        vec![typescript_block(&format!(
            r#"let won = 0;
for (let i = 0; i < {RACES}; i++) {{
  await Promise.race([oracle.step({{ id: "winner" }}), oracle.step({{ id: "loser" }})]);
  won = won + 1;
}}
finish(won);"#
        ))],
    )
    .await?
    .finish_within(LOOP_BUDGET)
    .await?;
    assert_eq!(
        run.final_value().as_f64(),
        Some(RACES as f64),
        "{}: every race in the loop was admitted; none was refused by the bound, got {}",
        tier.name,
        run.final_value()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sqlite_a_loop_of_races_well_past_the_bound_runs_to_its_end() -> Result<()> {
    a_loop_of_races_well_past_the_bound_runs_to_its_end(&sqlite()).await
}
