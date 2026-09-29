//! The perf guard that a tool batch costs linear time and memory in its width
//! (FIG-4068).
//!
//! ADR 0116 makes 64 the batch ceiling, so a host running a full batch pays
//! whatever a width-64 group costs. The guard holds a tier to the budget in
//! `scripts/perf_guard_budgets.json` (`tool_batch_scaling`): the large width's
//! turn time and peak RSS, divided by the small width's and by the width
//! ratio, stay under the budgeted normalized ratios. Linear growth scores 1;
//! quadratic growth from width 8 to 64 scores 8. The time is the CPU the
//! child process spent on the measured turn, as `string_scaling` measures
//! CPU rather than wall time: the guard runs beside other tests, and a
//! descheduled child is slow without the batch costing more. The turn's wall
//! time is reported beside it.
//!
//! Each width runs in a process of its own, a re-execution of the registering
//! test binary, because peak RSS is a process's high-water mark: measured in
//! one process, the small width's peak would hide inside the large width's,
//! and any test running beside it would count toward both. The child runs a
//! width-2 warm-up first, so neither width pays the process's one-time
//! initialisation, then measures one gated scenario of
//! [`crate::measure_gated_tool_batch`] and reports it on one stdout line.
//! Every width runs over the same catalog, the large width's: each child
//! records its session's tool surface, so a catalog that grew with the width
//! would charge every child for the width a second time, a cost of the
//! catalog a host chose, not of the batch.
//!
//! A registration supplies two tests in one module: the parent, which calls
//! [`assert_tool_batch_scales_linearly`], and an `#[ignore]`d child, which
//! calls [`run_tool_batch_scaling_child`] on its tier. Only the parent's
//! re-execution runs the child.
//!
//! A durable tier also counts resumptions (FIG-4088): the group dispatch's,
//! which starts the children, and the opener's, which consumes their
//! settlements. Each resumption replays the invocation's journal, which holds
//! an entry or more per child, so the replay costs the width once per
//! resumption: linear only while the resumptions stay bounded. A dispatch
//! that suspended once per child, or an opener that suspended once per rank,
//! replayed quadratically, and a width-64 group on an engine that replays at
//! every await ran for minutes.
//! [`measure_tool_batch_resumptions`] runs one gated scenario and reads the
//! tier's counts; [`assert_tool_batch_resumptions_bounded`] holds the large
//! width's counts to the small width's, unnormalized.

use std::sync::Arc;
use std::time::Duration;

/// The environment variable that tells a re-executed child which width to
/// measure.
pub const TOOL_BATCH_SCALING_WIDTH_ENV: &str = "LASH_TOOL_BATCH_SCALING_WIDTH";

/// The environment variable that tells a re-executed child how many tools
/// its session's catalog holds: the large width, whichever width it runs.
pub const TOOL_BATCH_SCALING_CATALOG_ENV: &str = "LASH_TOOL_BATCH_SCALING_CATALOG";

/// The marker of the one stdout line a child reports its measurement on.
const REPORT_PREFIX: &str = "LASH_TOOL_BATCH_SCALING ";

/// The small width's measurements the parent takes the least of: the small
/// run is short, so one descheduled run would flatter the ratio.
const SMALL_WIDTH_RUNS: usize = 3;

/// The checked-in `tool_batch_scaling` budget.
#[derive(Clone, Copy, Debug, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolBatchScalingBudget {
    /// The width the ratios are taken from.
    pub small_width: usize,
    /// The width held to the budget: the batch ceiling.
    pub large_width: usize,
    /// The most the large width's turn CPU time may be, over the small
    /// width's times the width ratio.
    pub max_normalized_time_ratio: f64,
    /// The same bound on the child process's peak RSS.
    pub max_normalized_peak_rss_ratio: f64,
    /// The most the large width's group dispatch may resume, over the small
    /// width's, with no width normalization: a dispatch that resumes a fixed
    /// number of times scores about one, and one that resumes per child
    /// scores the width ratio.
    pub max_dispatch_resumption_ratio: f64,
    /// The same bound on the opener's resumptions, the turn that opened the
    /// group and consumes its settlements.
    pub max_opener_resumption_ratio: f64,
}

impl ToolBatchScalingBudget {
    /// The `tool_batch_scaling` entry of the perf guard budgets JSON a
    /// registration reads with `include_str!`.
    ///
    /// # Panics
    ///
    /// When the JSON has no valid entry: the budget is checked in, so a
    /// missing one is a repository defect.
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
            budget.max_normalized_time_ratio.is_finite()
                && budget.max_normalized_time_ratio >= 1.0
                && budget.max_normalized_peak_rss_ratio.is_finite()
                && budget.max_normalized_peak_rss_ratio >= 1.0
                && budget.max_dispatch_resumption_ratio.is_finite()
                && budget.max_dispatch_resumption_ratio >= 1.0
                && budget.max_opener_resumption_ratio.is_finite()
                && budget.max_opener_resumption_ratio >= 1.0,
            "tool_batch_scaling ratios must be finite and at least one: {budget:?}"
        );
        budget
    }
}

/// One child's measurement.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolBatchScalingSample {
    pub width: usize,
    /// The measured turn's wall time.
    pub turn_ms: f64,
    /// The CPU the process spent across the measured turn, every thread.
    pub cpu_ms: f64,
    pub peak_rss_kib: u64,
}

/// What a re-executed child measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ToolBatchScalingChild {
    /// The batch width.
    pub width: usize,
    /// The tools the session's catalog holds.
    pub catalog: usize,
}

/// What this process was re-executed to measure, when it is a child.
///
/// # Panics
///
/// When a variable is set to something other than a count.
pub fn tool_batch_scaling_child() -> Option<ToolBatchScalingChild> {
    let count = |name: &str| {
        std::env::var(name).ok().map(|value| {
            value
                .parse::<usize>()
                .unwrap_or_else(|_| panic!("{name}=`{value}` is not a count"))
        })
    };
    let width = count(TOOL_BATCH_SCALING_WIDTH_ENV)?;
    Some(ToolBatchScalingChild {
        width,
        catalog: count(TOOL_BATCH_SCALING_CATALOG_ENV).unwrap_or(width),
    })
}

/// The child side: a width-2 warm-up, then one measured gated scenario of
/// `width` over a catalog of `catalog` tools, reported on stdout with this
/// process's peak RSS.
pub async fn run_tool_batch_scaling_child(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    producer: &crate::ToolBatchProducer,
    width: usize,
    catalog: usize,
) {
    crate::measure_gated_tool_batch(
        &format!("{prefix}-warmup"),
        Arc::clone(&effect_host),
        Arc::clone(&stores),
        Arc::clone(&runner),
        producer,
        2,
        catalog,
    )
    .await;
    let cpu_before = process_cpu_ms();
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
    let sample = ToolBatchScalingSample {
        width,
        turn_ms: measured.turn.as_secs_f64() * 1000.0,
        cpu_ms: process_cpu_ms() - cpu_before,
        peak_rss_kib: peak_rss_kib(),
    };
    println!(
        "{REPORT_PREFIX}{}",
        serde_json::to_string(&sample).unwrap_or_default()
    );
}

/// This process's peak resident set, in KiB, from `/proc/self/status`.
fn peak_rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")?
                    .trim()
                    .strip_suffix("kB")?
                    .trim()
                    .parse()
                    .ok()
            })
        })
        .unwrap_or_else(|| panic!("/proc/self/status reports no VmHWM"))
}

/// The CPU this process has spent, every thread, user and system, from
/// `/proc/self/stat`, whose tick is Linux's fixed `USER_HZ` of 100.
fn process_cpu_ms() -> f64 {
    const MS_PER_TICK: f64 = 10.0;
    let stat = std::fs::read_to_string("/proc/self/stat")
        .unwrap_or_else(|error| panic!("read /proc/self/stat: {error}"));
    // The command name is parenthesised and may hold spaces: the fields
    // after it are `state` (3) onwards, so utime (14) and stime (15) are the
    // 12th and 13th after the closing parenthesis.
    let after_name = stat
        .rsplit_once(')')
        .map(|(_, rest)| rest)
        .unwrap_or_else(|| panic!("/proc/self/stat has no command name: {stat}"));
    let ticks = after_name
        .split_whitespace()
        .skip(11)
        .take(2)
        .map(|field| {
            field
                .parse::<u64>()
                .unwrap_or_else(|_| panic!("/proc/self/stat field `{field}` is not a tick count"))
        })
        .sum::<u64>();
    ticks as f64 * MS_PER_TICK
}

/// The ratios one pair of measurements scores, normalized by the width ratio.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ToolBatchScalingRatios {
    pub time: f64,
    pub peak_rss: f64,
}

impl ToolBatchScalingRatios {
    /// `large` over `small`, divided by the width ratio.
    pub fn of(small: &ToolBatchScalingSample, large: &ToolBatchScalingSample) -> Self {
        let widths = large.width as f64 / small.width as f64;
        Self {
            time: large.cpu_ms / small.cpu_ms / widths,
            peak_rss: large.peak_rss_kib as f64 / small.peak_rss_kib as f64 / widths,
        }
    }

    /// Whether both ratios are inside `budget`.
    pub fn within(&self, budget: &ToolBatchScalingBudget) -> bool {
        self.time <= budget.max_normalized_time_ratio
            && self.peak_rss <= budget.max_normalized_peak_rss_ratio
    }
}

/// The parent side: re-executes this test binary as the child test `child`
/// of `module_path` (the registering module's `module_path!()`) once per run
/// of each width, and fails when the large width outgrows `budget`.
///
/// # Panics
///
/// When a child fails or reports nothing, or the ratios exceed the budget.
pub fn assert_tool_batch_scales_linearly(
    label: &str,
    module_path: &str,
    child: &str,
    budget: ToolBatchScalingBudget,
) {
    // The harness names a test by its path inside the crate.
    let child_test = match module_path.split_once("::") {
        Some((_crate, path)) => format!("{path}::{child}"),
        None => child.to_owned(),
    };
    let child_test = child_test.as_str();
    let small = (0..SMALL_WIDTH_RUNS)
        .map(|_| run_child(child_test, budget.small_width, budget.large_width))
        .reduce(|least, sample| ToolBatchScalingSample {
            width: least.width,
            turn_ms: least.turn_ms.min(sample.turn_ms),
            cpu_ms: least.cpu_ms.min(sample.cpu_ms),
            peak_rss_kib: least.peak_rss_kib.min(sample.peak_rss_kib),
        })
        .unwrap_or_else(|| panic!("{label}: no small-width run"));
    let large = run_child(child_test, budget.large_width, budget.large_width);
    let ratios = ToolBatchScalingRatios::of(&small, &large);
    println!(
        "{label}: width {} {:.0} ms CPU ({:.0} ms wall) {} KiB peak RSS; width {} {:.0} ms \
         CPU ({:.0} ms wall) {} KiB peak RSS; normalized time {:.2} (budget {}), peak RSS \
         {:.2} (budget {})",
        small.width,
        small.cpu_ms,
        small.turn_ms,
        small.peak_rss_kib,
        large.width,
        large.cpu_ms,
        large.turn_ms,
        large.peak_rss_kib,
        ratios.time,
        budget.max_normalized_time_ratio,
        ratios.peak_rss,
        budget.max_normalized_peak_rss_ratio,
    );
    assert!(
        ratios.within(&budget),
        "{label}: a width-{} tool batch must cost at most {}x (time) and {}x (peak RSS) \
         {} times the width-{} figures; it cost {:.2}x and {:.2}x ({small:?}, {large:?})",
        large.width,
        budget.max_normalized_time_ratio,
        budget.max_normalized_peak_rss_ratio,
        large.width / small.width,
        small.width,
        ratios.time,
        ratios.peak_rss,
    );
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

/// Bounds one child: a wedged scenario fails the guard instead of hanging it.
const CHILD_TIMEOUT: Duration = Duration::from_secs(600);

fn run_child(child_test: &str, width: usize, catalog: usize) -> ToolBatchScalingSample {
    let exe = std::env::current_exe()
        .unwrap_or_else(|error| panic!("locate this test binary to re-execute: {error}"));
    let mut child = std::process::Command::new(exe)
        .args([
            child_test,
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(TOOL_BATCH_SCALING_WIDTH_ENV, width.to_string())
        .env(TOOL_BATCH_SCALING_CATALOG_ENV, catalog.to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("re-execute `{child_test}` at width {width}: {error}"));
    let started = std::time::Instant::now();
    while child
        .try_wait()
        .unwrap_or_else(|error| panic!("wait on the width-{width} child: {error}"))
        .is_none()
    {
        if started.elapsed() > CHILD_TIMEOUT {
            let _ = child.kill();
            panic!("the width-{width} child `{child_test}` ran past {CHILD_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("collect the width-{width} child's output: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "the width-{width} child `{child_test}` failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    stdout
        .lines()
        // libtest may print the test's `test <name> ... ` prefix on the
        // report's own line.
        .find_map(|line| line.split_once(REPORT_PREFIX).map(|(_, report)| report))
        .map(|report| {
            serde_json::from_str::<ToolBatchScalingSample>(report).unwrap_or_else(|error| {
                panic!("the width-{width} child's report `{report}` does not decode: {error}")
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "the width-{width} child `{child_test}` reported no measurement; did the \
                 filter name it?\n{stdout}"
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: ToolBatchScalingBudget = ToolBatchScalingBudget {
        small_width: 8,
        large_width: 64,
        max_normalized_time_ratio: 2.5,
        max_normalized_peak_rss_ratio: 2.5,
        max_dispatch_resumption_ratio: 2.0,
        max_opener_resumption_ratio: 2.0,
    };

    fn sample(width: usize, cpu_ms: f64, peak_rss_kib: u64) -> ToolBatchScalingSample {
        ToolBatchScalingSample {
            width,
            turn_ms: cpu_ms,
            cpu_ms,
            peak_rss_kib,
        }
    }

    #[test]
    fn linear_growth_passes_and_quadratic_growth_trips_the_budget() {
        let small = sample(8, 100.0, 100_000);
        let linear = sample(64, 800.0, 800_000);
        assert!(ToolBatchScalingRatios::of(&small, &linear).within(&BUDGET));
        // FIG-4068's measurement: 8.5 s at width 32 and about 61 s at 64 is
        // superlinear, and the quadratic cost of 8x the width is 64x.
        let quadratic_time = sample(64, 6_400.0, 800_000);
        assert!(!ToolBatchScalingRatios::of(&small, &quadratic_time).within(&BUDGET));
        let quadratic_memory = sample(64, 800.0, 6_400_000);
        assert!(!ToolBatchScalingRatios::of(&small, &quadratic_memory).within(&BUDGET));
    }

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
