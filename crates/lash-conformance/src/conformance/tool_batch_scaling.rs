//! Count-based tool-batch scaling guard (FIG-4132).
//!
//! Each group dispatch and opener invocation journals work proportional to
//! the batch width. A suspension replays that journal, so a dispatch that
//! suspends once per child or an opener that reads once per rank does
//! quadratic work. The guard compares their resumption counts at widths 8
//! and 64 on the Restate server double with replay at every await. Wall time
//! is reported for diagnosis but never decides the result.

use std::sync::Arc;
use std::time::Duration;

/// The checked-in `tool_batch_scaling` budget.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBatchScalingBudget {
    pub small_width: usize,
    pub large_width: usize,
    /// Maximum large/small dispatch resumption count ratio.
    pub max_dispatch_resumption_ratio: f64,
    /// Maximum large/small opener resumption count ratio.
    pub max_opener_resumption_ratio: f64,
}

impl ToolBatchScalingBudget {
    /// Read the `tool_batch_scaling` entry of the checked-in budgets JSON.
    ///
    /// # Panics
    ///
    /// When the checked-in budget is invalid.
    #[expect(
        clippy::expect_used,
        reason = "the checked-in budgets JSON parses once per guard run, per the message"
    )]
    pub fn from_perf_guard_budgets(json: &str) -> Self {
        #[derive(serde::Deserialize)]
        struct Budgets {
            tool_batch_scaling: ToolBatchScalingBudget,
        }
        let budget = serde_json::from_str::<Budgets>(json)
            .expect("scripts/perf_guard_budgets.json must budget tool_batch_scaling")
            .tool_batch_scaling;
        assert!(
            budget.small_width >= 1 && budget.large_width > budget.small_width,
            "tool_batch_scaling must measure a larger width against a smaller one: {budget:?}"
        );
        assert!(
            budget.max_dispatch_resumption_ratio.is_finite()
                && budget.max_dispatch_resumption_ratio >= 1.0
                && budget.max_opener_resumption_ratio.is_finite()
                && budget.max_opener_resumption_ratio >= 1.0,
            "tool_batch_scaling ratios must be finite and at least one: {budget:?}"
        );
        budget
    }
}

/// A tier's running resumption counts (FIG-4088).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ToolBatchResumptionCounts {
    /// Every group dispatch's suspensions.
    pub dispatch: u64,
    /// Every opener's suspensions: the invocations that open groups and
    /// consume their settlements.
    pub opener: u64,
}

impl From<(u64, u64)> for ToolBatchResumptionCounts {
    fn from((dispatch, opener): (u64, u64)) -> Self {
        Self { dispatch, opener }
    }
}

/// How often one width's group dispatch and opener resumed on a tier that
/// replays at every await (FIG-4088). Each resumption replays the whole
/// journal of the invocation it resumes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolBatchResumptions {
    pub width: usize,
    pub counts: ToolBatchResumptionCounts,
    /// The scenario turn's wall time, reported beside the counts.
    pub turn: Duration,
}

impl ToolBatchResumptions {
    /// `large`'s count over `small`'s, unnormalized, for the dispatch and the
    /// opener. A count of none counts as one, so a side that never suspends
    /// scores one.
    pub fn ratios(small: &Self, large: &Self) -> (f64, f64) {
        let ratio = |small: u64, large: u64| large.max(1) as f64 / small.max(1) as f64;
        (
            ratio(small.counts.dispatch, large.counts.dispatch),
            ratio(small.counts.opener, large.counts.opener),
        )
    }

    /// Whether both ratios are inside `budget`.
    pub fn within(small: &Self, large: &Self, budget: &ToolBatchScalingBudget) -> bool {
        let (dispatch, opener) = Self::ratios(small, large);
        dispatch <= budget.max_dispatch_resumption_ratio
            && opener <= budget.max_opener_resumption_ratio
    }
}

/// Runs one gated width-`width` scenario of [`crate::measure_gated_tool_batch`]
/// and reports how often its group dispatch and its opener resumed: `counts`
/// reads the tier's running counts, before and after the scenario, and waits
/// for the scenario's dispatch to finish before it answers.
#[expect(
    clippy::too_many_arguments,
    reason = "the scenario's tier handles beside the tier's own resumption counts"
)]
pub async fn measure_tool_batch_resumptions<F, Fut>(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: &crate::ToolBatchProducer,
    width: usize,
    catalog: usize,
    counts: F,
) -> ToolBatchResumptions
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ToolBatchResumptionCounts>,
{
    let before = counts().await;
    let measured = crate::measure_gated_tool_batch(
        prefix,
        effect_host,
        stores,
        runner,
        producer,
        width,
        catalog,
    )
    .await;
    let after = counts().await;
    ToolBatchResumptions {
        width,
        counts: ToolBatchResumptionCounts {
            dispatch: after.dispatch.saturating_sub(before.dispatch),
            opener: after.opener.saturating_sub(before.opener),
        },
        turn: measured.turn,
    }
}

/// Fails when the large width's group dispatch or opener resumed more than
/// `budget` allows over the small width's.
///
/// # Panics
///
/// When a ratio exceeds `max_dispatch_resumption_ratio` or
/// `max_opener_resumption_ratio`.
pub fn assert_tool_batch_resumptions_bounded(
    label: &str,
    small: ToolBatchResumptions,
    large: ToolBatchResumptions,
    budget: ToolBatchScalingBudget,
) {
    let (dispatch, opener) = ToolBatchResumptions::ratios(&small, &large);
    println!(
        "{label}: width {} resumed its dispatch {} and its opener {} times ({:?} turn); width \
         {} resumed them {} and {} times ({:?} turn); ratios {dispatch:.2} (budget {}) and \
         {opener:.2} (budget {})",
        small.width,
        small.counts.dispatch,
        small.counts.opener,
        small.turn,
        large.width,
        large.counts.dispatch,
        large.counts.opener,
        large.turn,
        budget.max_dispatch_resumption_ratio,
        budget.max_opener_resumption_ratio,
    );
    assert!(
        ToolBatchResumptions::within(&small, &large, &budget),
        "{label}: a width-{} group's dispatch and opener must resume at most {}x and {}x as \
         often as a width-{} group's; they resumed {dispatch:.2}x and {opener:.2}x ({small:?}, \
         {large:?}). Every resumption replays a journal that grows with the width, so \
         resumptions that grow with it cost quadratic replay.",
        large.width,
        budget.max_dispatch_resumption_ratio,
        budget.max_opener_resumption_ratio,
        small.width,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: ToolBatchScalingBudget = ToolBatchScalingBudget {
        small_width: 8,
        large_width: 64,
        max_dispatch_resumption_ratio: 3.0,
        max_opener_resumption_ratio: 2.0,
    };

    #[test]
    fn bounded_resumptions_pass_and_per_child_resumptions_trip_the_budget() {
        let resumed = |width, dispatch, opener| ToolBatchResumptions {
            width,
            counts: ToolBatchResumptionCounts { dispatch, opener },
            turn: Duration::ZERO,
        };
        let within = |small, large| ToolBatchResumptions::within(&small, &large, &BUDGET);
        assert!(within(resumed(8, 7, 30), resumed(64, 9, 40)));
        assert!(within(resumed(8, 0, 0), resumed(64, 0, 0)));
        // FIG-4088's dispatch: an invocation-id await and a record call per
        // child, two resumptions each.
        assert!(!within(
            resumed(8, 2 * 8 + 5, 30),
            resumed(64, 2 * 64 + 5, 30)
        ));
        // Its opener: a read and a payload get per rank.
        assert!(!within(
            resumed(8, 7, 2 * 8 + 50),
            resumed(64, 7, 2 * 64 + 50)
        ));
    }
}
