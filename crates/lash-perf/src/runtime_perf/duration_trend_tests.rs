//! Behaviour of the duration-trend verdict, its history file and the accepted
//! level shifts that reset a series' comparison window.
//!
//! A sibling file rather than an inline `mod tests`: `duration_trend.rs` is the
//! signal, and `scripts/check-production-file-size.py` budgets a production
//! source file at 1600 lines against 2500 for a `*_tests.rs` one.

use super::*;

/// A flat series at `value`, long enough to establish a baseline.
fn flat(value: f64, runs: usize) -> Vec<f64> {
    vec![value; runs]
}

#[test]
fn short_history_never_yields_a_verdict() {
    for runs in 0..=MIN_BASELINE_RUNS {
        let series = flat(10.0, runs);
        assert_eq!(
            verdict(&series),
            DriftVerdict::InsufficientData { runs },
            "{runs} run(s) must not produce a verdict"
        );
    }
    // One more run than the baseline minimum is the first judgeable point.
    assert_eq!(
        verdict(&flat(10.0, MIN_BASELINE_RUNS + 1)),
        DriftVerdict::Stable
    );
}

#[test]
fn a_single_spike_does_not_trip_the_signal() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    series.push(100.0);

    assert_eq!(verdict(&series), DriftVerdict::Elevated { streak: 1 });
    assert!(!verdict(&series).is_drifting());
}

#[test]
fn a_spike_that_recovers_leaves_no_streak_behind() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    series.push(100.0);
    series.push(10.0);

    assert_eq!(verdict(&series), DriftVerdict::Stable);
}

#[test]
fn drift_shorter_than_the_streak_requirement_stays_advisory_only() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS - 1));

    assert_eq!(
        verdict(&series),
        DriftVerdict::Elevated {
            streak: DRIFT_CONSECUTIVE_RUNS - 1
        }
    );
}

#[test]
fn sustained_drift_over_the_streak_requirement_trips_the_signal() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS));

    let result = verdict(&series);
    assert_eq!(
        result,
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        }
    );
    assert!(result.is_drifting());
}

#[test]
fn a_run_exactly_at_the_threshold_is_not_elevated() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    series.push(10.0 * (1.0 + DRIFT_THRESHOLD_PCT / 100.0));

    assert_eq!(verdict(&series), DriftVerdict::Stable);
}

#[test]
fn everyday_jitter_under_the_threshold_never_accumulates_a_streak() {
    let mut series = flat(10.0, TREND_WINDOW_RUNS);
    // Alternating ±40%: far noisier than a real runner, still under 50%.
    for index in 0..(DRIFT_CONSECUTIVE_RUNS * 4) {
        series.push(if index.is_multiple_of(2) { 14.0 } else { 6.0 });
    }

    assert_eq!(verdict(&series), DriftVerdict::Stable);
}

/// The streak is per-run-against-its-own-window, and this is the series
/// that proves it: replacing every window with one shared baseline taken
/// from the newest run turns a `Drifting` verdict into `Elevated`, because
/// the shared baseline is dragged up by the very drifted runs it judges.
/// Every other test in this module passes under that mutation.
#[test]
fn a_swinging_series_distinguishes_per_run_windows_from_one_shared_baseline() {
    let series = [
        9.53, 16.73, 23.65, 10.37, 9.86, 10.65, 22.57, 15.93, 25.57, 24.04, 10.52, 17.43, 9.21,
        10.0, 98.53, 103.81, 10.1, 10.53, 9.64, 10.11, 104.8, 98.89, 23.32, 22.7, 100.35, 106.62,
    ];

    // Per-run windows: the last six runs each cleared their own trailing
    // median, including the two ~23 ms runs whose own windows sat near
    // 13 ms. A single shared baseline of 20.0 (the newest run's window)
    // would score only the last two and report Elevated.
    assert_eq!(verdict(&series), DriftVerdict::Drifting { streak: 6 });

    let shared_baseline = baseline_median(&series, series.len() - 1).expect("baseline");
    let shared_streak = series
        .iter()
        .rev()
        .take_while(|value| exceeds_threshold(**value, shared_baseline))
        .count();
    assert_eq!(
        shared_streak, 2,
        "the shared-baseline reading must genuinely differ, or this test proves nothing"
    );
    assert!(shared_streak < DRIFT_CONSECUTIVE_RUNS);
}

/// The closing edge the module documentation promises, pinned as
/// behaviour rather than left as prose: a regression is loud for a
/// bounded number of main runs and then becomes the new normal.
#[test]
fn a_step_change_is_loud_for_a_bounded_window_then_ages_into_the_baseline() {
    let stepped = |multiplier: f64, post_step_runs: usize| {
        let mut series = flat(10.0, TREND_WINDOW_RUNS * 2);
        series.extend(flat(10.0 * multiplier, post_step_runs));
        verdict(&series)
    };

    // A doubling: Elevated 1-4, DRIFTING 5-10, Stable from 11.
    for post_step_runs in 1..DRIFT_CONSECUTIVE_RUNS {
        assert_eq!(
            stepped(2.0, post_step_runs),
            DriftVerdict::Elevated {
                streak: post_step_runs
            },
            "post-step run {post_step_runs}"
        );
    }
    for post_step_runs in DRIFT_CONSECUTIVE_RUNS..=10 {
        assert_eq!(
            stepped(2.0, post_step_runs),
            DriftVerdict::Drifting {
                streak: post_step_runs
            },
            "post-step run {post_step_runs}"
        );
    }
    for post_step_runs in 11..=14 {
        assert_eq!(
            stepped(2.0, post_step_runs),
            DriftVerdict::Stable,
            "post-step run {post_step_runs}"
        );
    }

    // Magnitude buys exactly one extra run and no more, and the boundary
    // is not a tuning choice: at run 11 the window holds ten pre-step and
    // ten post-step runs, so the median is (B + E) / 2 and the run is
    // elevated iff E > 1.5 * (B + E) / 2, i.e. iff E > 3B. Pinned either
    // side of exactly 3x.
    assert_eq!(
        stepped(3.0, 11),
        DriftVerdict::Stable,
        "exactly 3x does not clear the straddling median"
    );
    assert_eq!(
        stepped(3.01, 11),
        DriftVerdict::Drifting { streak: 11 },
        "just past 3x does"
    );
    // The extra run is all it buys, at any magnitude.
    assert_eq!(stepped(3.01, 12), DriftVerdict::Stable);
    assert_eq!(stepped(10.0, 11), DriftVerdict::Drifting { streak: 11 });
    assert_eq!(stepped(10.0, 12), DriftVerdict::Stable);
    assert_eq!(stepped(100.0, 12), DriftVerdict::Stable);
}

#[test]
fn the_baseline_window_is_bounded_to_the_trailing_runs() {
    // A very old, very slow era must not hold the baseline up forever.
    let mut series = flat(1_000.0, TREND_WINDOW_RUNS * 2);
    series.extend(flat(10.0, TREND_WINDOW_RUNS));
    series.extend(flat(20.0, DRIFT_CONSECUTIVE_RUNS));

    assert_eq!(
        verdict(&series),
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        }
    );
}

#[test]
fn series_are_keyed_by_profile_and_scenario_together() {
    let mut history = Vec::new();
    for index in 0..(TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS) {
        let drifted = index >= TREND_WINDOW_RUNS;
        history.push(record("standard", "quick", index, 10.0));
        history.push(record(
            "standard",
            "full",
            index,
            if drifted { 200.0 } else { 100.0 },
        ));
    }

    let rows = trend_rows(&history, None);
    assert_eq!(rows.len(), 2);
    let full = rows.iter().find(|row| row.profile == "full").unwrap();
    let quick = rows.iter().find(|row| row.profile == "quick").unwrap();
    assert!(full.verdict.is_drifting(), "{:?}", full.verdict);
    assert_eq!(quick.verdict, DriftVerdict::Stable);
    assert_eq!(full.baseline_median_ms, Some(100.0));
    assert_eq!(full.delta_pct, Some(100.0));

    let filtered = trend_rows(&history, Some("quick"));
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].profile, "quick");
}

#[test]
fn history_round_trips_through_the_file_and_sorts_by_observation_time() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("nested").join("history.jsonl");

    assert!(
        load_history(&path)
            .expect("missing history is empty")
            .is_empty()
    );

    append_records(&path, &[record("standard", "quick", 2, 30.0)]).expect("append late");
    append_records(&path, &[record("standard", "quick", 1, 20.0)]).expect("append early");

    let loaded = load_history(&path).expect("history loads");
    assert_eq!(loaded.len(), 2);
    assert_eq!(loaded[0].total_ms, 20.0);
    assert_eq!(loaded[1].total_ms, 30.0);
    assert_eq!(loaded[0].total_p95_ms, Some(25.0));
}

#[test]
fn a_record_missing_geometry_is_rejected_without_a_migration_arm() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    std::fs::write(
        &path,
        "{\"scenario\":\"standard\",\"profile\":\"quick\",\"commit\":\"abc\",\
         \"run_id\":\"1\",\"recorded_at\":\"2026-01-01T00:00:00Z\",\"total_ms\":10.0}\n",
    )
    .expect("write");

    assert!(load_history(&path).is_err());
    let loaded = load_history_lenient(&path).expect("history scan completes");
    assert!(loaded.records.is_empty());
    assert_eq!(loaded.skipped.len(), 1);
    assert!(loaded.preserved.is_empty());
}

#[test]
fn a_record_from_a_newer_schema_is_excluded_from_verdicts_but_not_dropped() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    let mut future = record("standard", "quick", 1, 10.0);
    future.version = HISTORY_RECORD_VERSION + 1;
    append_records(&path, &[future, record("standard", "quick", 2, 11.0)]).expect("append");

    let loaded = load_history_lenient(&path).expect("history loads");
    assert_eq!(loaded.records.len(), 1);
    assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    // Unreadable is not the same as unwanted: it is held, not counted.
    assert_eq!(loaded.preserved.len(), 1);
    assert!(loaded.preserved[0].contains(&format!("\"version\":{}", HISTORY_RECORD_VERSION + 1)));
}

/// An older build restored onto a newer history — a revert push, or a
/// rerun of a pre-bump commit — must not be the thing that destroys the
/// newer records, because `main` would save that loss on the very next
/// run. The rewrite carries them byte-for-byte.
#[test]
fn an_older_build_carries_newer_records_through_the_rewrite_verbatim() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");

    let mut future = record("standard", "quick", 1, 10.0);
    future.version = HISTORY_RECORD_VERSION + 1;
    let future_line = serde_json::to_string(&future).expect("serialize");
    // A readable record, an unreadable-schema record, and a corrupt line:
    // the rewrite must keep the first, keep the second untouched, and drop
    // only the third.
    std::fs::write(
        &path,
        format!(
            "{}\n{future_line}\nnot json\n",
            serde_json::to_string(&record("standard", "quick", 2, 11.0)).expect("serialize")
        ),
    )
    .expect("write");

    record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

    let rewritten = std::fs::read_to_string(&path).expect("read back");
    let lines = rewritten.lines().collect::<Vec<_>>();
    assert!(
        lines.contains(&future_line.as_str()),
        "newer record must survive byte-identical, got:\n{rewritten}"
    );
    assert!(!rewritten.contains("not json"), "{rewritten}");
    assert_eq!(lines.len(), 2, "{rewritten}");

    // And it is still not readable as a verdict input by this build.
    let loaded = load_history_lenient(&path).expect("history loads");
    assert_eq!(loaded.records.len(), 1);
    assert_eq!(loaded.preserved.len(), 1);
}

#[test]
fn a_scratch_file_left_by_a_killed_rewrite_is_swept_on_the_next_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    // A clean, short history: this run needs no compaction at all, so the
    // rewrite will not incidentally consume the orphan. Only the explicit
    // sweep can remove it, which is the whole point.
    append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
    std::fs::write(rewrite_temp_path(&path), "stale\n").expect("write orphan");

    record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

    assert!(
        !rewrite_temp_path(&path).exists(),
        "the scratch file is inside the directory CI caches; nothing else sweeps it"
    );
    // The history itself is untouched by the sweep.
    assert_eq!(load_history(&path).expect("history loads").len(), 1);
}

#[test]
fn the_strict_read_refuses_a_history_the_lenient_read_salvages() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
    std::fs::write(
        &path,
        format!(
            "{}truncated{{\n",
            std::fs::read_to_string(&path).expect("read")
        ),
    )
    .expect("write");

    let error = load_history(&path).expect_err("strict read must fail loudly");
    assert!(format!("{error:#}").contains("line 2"), "{error:#}");

    let loaded = load_history_lenient(&path).expect("lenient read salvages");
    assert_eq!(loaded.records.len(), 1);
    assert_eq!(loaded.skipped.len(), 1);
}

#[test]
fn a_poisoned_history_is_healed_instead_of_carried_forward() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    append_records(&path, &[record("standard", "quick", 1, 10.0)]).expect("append");
    std::fs::write(
        &path,
        format!(
            "{}not json\n",
            std::fs::read_to_string(&path).expect("read")
        ),
    )
    .expect("write");

    // The run path must return normally *and* leave a clean file behind,
    // or the bad line rides into every future cache entry.
    record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

    let healed = load_history(&path).expect("history is parseable again");
    assert_eq!(healed.len(), 1);
    assert_eq!(healed[0].total_ms, 10.0);
}

#[test]
fn an_unwritable_history_disables_the_signal_without_failing_the_run() {
    let dir = tempfile::tempdir().expect("temp dir");
    // A directory where the file should be: every write fails, and the run
    // must still return normally.
    let path = dir.path().join("history.jsonl");
    std::fs::create_dir(&path).expect("occupy the path");

    record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

    assert!(run_duration_trend_cli(&path, Some("quick")).is_err());
}

#[test]
fn rewriting_bounds_each_series_without_touching_a_verdict() {
    let readable = TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS;
    assert!(
        RETAINED_RUNS_PER_SERIES > readable,
        "retention must exceed what a verdict reads, or truncation changes verdicts"
    );
    assert!(
        RETAINED_RUNS_PER_SERIES <= 4 * readable,
        "retention must stay a small multiple of what a verdict reads: the history \
         lives in a cache entry every main run touches, so nothing evicts it but this"
    );

    let mut history = Vec::new();
    for index in 0..(RETAINED_RUNS_PER_SERIES * 2) {
        // Two series, so retention is proven to be per-series and not global.
        history.push(record("standard", "quick", index, 10.0));
        history.push(record("rlm", "quick", index, 20.0));
    }
    // The tail that any verdict can see, before and after truncation.
    history.extend(
        (0..DRIFT_CONSECUTIVE_RUNS).map(|index| record("standard", "quick", 10_000 + index, 40.0)),
    );

    let before = verdict(
        &history
            .iter()
            .filter(|record| record.scenario == "standard")
            .map(|record| record.total_ms)
            .collect::<Vec<_>>(),
    );
    let retained = retained_records(&history);
    let after = verdict(
        &retained
            .iter()
            .filter(|record| record.scenario == "standard")
            .map(|record| record.total_ms)
            .collect::<Vec<_>>(),
    );

    assert_eq!(retained.len(), RETAINED_RUNS_PER_SERIES * 2);
    assert_eq!(
        retained
            .iter()
            .filter(|record| record.scenario == "standard")
            .count(),
        RETAINED_RUNS_PER_SERIES
    );
    assert_eq!(before, after);
    assert!(before.is_drifting(), "{before:?}");
}

#[test]
fn a_long_history_is_truncated_on_disk_by_the_run_path() {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("history.jsonl");
    let overlong = (0..(RETAINED_RUNS_PER_SERIES + 20))
        .map(|index| record("standard", "quick", index, 10.0))
        .collect::<Vec<_>>();
    append_records(&path, &overlong).expect("append");

    record_and_report(&path, "quick", DurationTrendGeometry::current(2, 0, 3), &[]);

    let healed = load_history(&path).expect("history loads");
    assert_eq!(healed.len(), RETAINED_RUNS_PER_SERIES);
}

#[test]
fn the_committed_fixture_demonstrates_every_verdict() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join("duration-trend-history.jsonl");
    let history = load_history(&path).expect("fixture loads");
    let rows = trend_rows(&history, Some("quick"));

    let by_scenario = rows
        .iter()
        .map(|row| (row.scenario.as_str(), row.verdict))
        .collect::<BTreeMap<_, _>>();
    assert_eq!(by_scenario["standard"], DriftVerdict::Stable);
    assert_eq!(by_scenario["rlm"], DriftVerdict::Elevated { streak: 1 });
    assert_eq!(
        by_scenario["deep_turn_composition"],
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        }
    );
    assert!(matches!(
        by_scenario["store_reopen"],
        DriftVerdict::InsufficientData { .. }
    ));

    let table = render_trend_table(&rows);
    assert!(table.contains("deep_turn_composition"), "{table}");
    assert!(table.contains("DRIFTING"), "{table}");
    assert!(table.contains("insufficient data"), "{table}");
}

#[test]
fn an_empty_history_renders_a_table_rather_than_a_verdict() {
    let table = render_trend_table(&[]);
    assert!(table.contains("no history records"), "{table}");
}

#[test]
fn same_labels_with_different_geometry_are_distinct_series() {
    let mut debug = record("standard", "quick", 1, 10.0);
    debug.runs = 2;
    debug.warmups = 0;
    debug.turns = 3;
    debug.build_mode = BuildMode::Debug;

    let mut release = record("standard", "quick", 2, 20.0);
    release.runs = 5;
    release.warmups = 1;
    release.turns = 12;
    release.build_mode = BuildMode::Release;

    let rows = trend_rows(&[debug, release], Some("quick"));
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.geometry
        == DurationTrendGeometry {
            runs: 2,
            warmups: 0,
            turns: 3,
            build_mode: BuildMode::Debug,
        }));
    assert!(rows.iter().any(|row| row.geometry
        == DurationTrendGeometry {
            runs: 5,
            warmups: 1,
            turns: 12,
            build_mode: BuildMode::Release,
        }));
}

/// A series that has stepped and is shouting about it, plus the timestamp
/// of the first observation at the new level.
fn stepped_series(scenario: &str, profile: &str) -> (Vec<DurationHistoryRecord>, String) {
    let mut history = (0..TREND_WINDOW_RUNS)
        .map(|index| record(scenario, profile, index, 10.0))
        .collect::<Vec<_>>();
    history.extend(
        (0..DRIFT_CONSECUTIVE_RUNS)
            .map(|index| record(scenario, profile, TREND_WINDOW_RUNS + index, 20.0)),
    );
    let step_at = history[TREND_WINDOW_RUNS].recorded_at.clone();
    (history, step_at)
}

fn acceptance(scenario: Option<&str>, effective_from: &str) -> LevelShift {
    LevelShift {
        profile: Some("quick".to_string()),
        scenario: scenario.map(str::to_string),
        metric: None,
        commit: "2dea44485f0".to_string(),
        effective_from: effective_from.to_string(),
        reason: "FIG-3157 runs a second physical turn on purpose".to_string(),
    }
}

fn only_row(rows: &[DurationTrendRow], scenario: &str) -> DurationTrendRow {
    rows.iter()
        .find(|row| row.scenario == scenario)
        .expect("one row per scenario")
        .clone()
}

/// `report_drift` prints exactly the rows whose verdict is `Drifting`, so
/// counting them is counting the warnings an operator would see.
fn warnings(rows: &[DurationTrendRow]) -> usize {
    rows.iter().filter(|row| row.verdict.is_drifting()).count()
}

#[test]
fn an_accepted_level_shift_stops_its_own_warning_on_the_very_next_read() {
    let (history, step_at) = stepped_series("standard", "quick");

    let unaccepted = trend_rows_against(&history, Some("quick"), &[]);
    assert_eq!(
        only_row(&unaccepted, "standard").verdict,
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        },
        "the step must be loud before it is accepted, or this proves nothing"
    );
    assert_eq!(warnings(&unaccepted), 1);

    let accepted = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &step_at)],
    );
    let row = only_row(&accepted, "standard");
    // Everything before the marker left the window, so the series is as
    // young as its post-marker observations and issues no verdict at all.
    assert_eq!(
        row.verdict,
        DriftVerdict::InsufficientData {
            runs: DRIFT_CONSECUTIVE_RUNS
        }
    );
    assert_eq!(warnings(&accepted), 0);
    // The run itself is still reported; only the window it is judged
    // against moved.
    assert_eq!(row.current_ms, 20.0);
    assert_eq!(row.baseline_median_ms, None);
    assert_eq!(
        row.baseline_reset,
        Some(BaselineReset {
            commit: "2dea44485".to_string(),
            reason: "FIG-3157 runs a second physical turn on purpose".to_string(),
        })
    );
}

#[test]
fn drift_that_arrives_after_an_acceptance_still_fires() {
    let (mut history, step_at) = stepped_series("standard", "quick");
    // A second, unaccepted step on top of the accepted one.
    history.extend((0..DRIFT_CONSECUTIVE_RUNS).map(|index| {
        record(
            "standard",
            "quick",
            TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS + index,
            40.0,
        )
    }));

    let accepted = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &step_at)],
    );
    let row = only_row(&accepted, "standard");
    assert_eq!(
        row.verdict,
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        }
    );
    assert_eq!(warnings(&accepted), 1);
    // Judged against the accepted level, not against the pre-acceptance
    // one: 40 against a trailing median of 20, not of 10.
    assert_eq!(row.baseline_median_ms, Some(20.0));
    assert_eq!(row.delta_pct, Some(100.0));
}

#[test]
fn an_acceptance_dated_before_the_step_accepts_nothing() {
    let (history, _) = stepped_series("standard", "quick");
    let before_everything = history[0].recorded_at.clone();

    let rows = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &before_everything)],
    );
    // A marker is a window trim, not a mute: dated before the step, it
    // removes nothing and the step is still drift.
    assert_eq!(
        only_row(&rows, "standard").verdict,
        DriftVerdict::Drifting {
            streak: DRIFT_CONSECUTIVE_RUNS
        }
    );
    assert_eq!(warnings(&rows), 1);
}

#[test]
fn an_acceptance_covers_only_the_series_it_names() {
    let (mut history, step_at) = stepped_series("standard", "quick");
    let (other, _) = stepped_series("rlm", "quick");
    history.extend(other);

    let named = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &step_at)],
    );
    assert!(only_row(&named, "rlm").verdict.is_drifting());
    assert!(!only_row(&named, "standard").verdict.is_drifting());
    assert_eq!(warnings(&named), 1);
    assert_eq!(only_row(&named, "rlm").baseline_reset, None);

    // An omitted scenario selector is the runner-swap case: it covers
    // every scenario in the profile.
    let unnamed = trend_rows_against(&history, Some("quick"), &[acceptance(None, &step_at)]);
    assert_eq!(warnings(&unnamed), 0);
}

#[test]
fn an_acceptance_does_not_reach_another_profile() {
    let (quick, step_at) = stepped_series("standard", "quick");
    let (full, _) = stepped_series("standard", "full");
    let history = [quick, full].concat();

    let rows = trend_rows_against(&history, None, &[acceptance(None, &step_at)]);
    let quick_row = rows
        .iter()
        .find(|row| row.profile == "quick")
        .expect("quick row");
    let full_row = rows
        .iter()
        .find(|row| row.profile == "full")
        .expect("full row");
    assert!(!quick_row.verdict.is_drifting());
    // Durations are only comparable within one benchmark geometry, so an
    // acceptance taken on one profile says nothing about another.
    assert!(full_row.verdict.is_drifting());
    assert_eq!(full_row.baseline_reset, None);
}

#[test]
fn the_newest_applicable_acceptance_sets_the_window() {
    let (mut history, first_step_at) = stepped_series("standard", "quick");
    history.extend((0..DRIFT_CONSECUTIVE_RUNS).map(|index| {
        record(
            "standard",
            "quick",
            TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS + index,
            40.0,
        )
    }));
    let second_step_at = history[TREND_WINDOW_RUNS + DRIFT_CONSECUTIVE_RUNS]
        .recorded_at
        .clone();

    let mut newer = acceptance(Some("standard"), &second_step_at);
    newer.commit = "9f3c1ab72".to_string();
    newer.reason = "the second step is deliberate too".to_string();
    // File order is the older marker first, so a first-match reading would
    // pick the wrong one.
    let rows = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &first_step_at), newer],
    );

    let row = only_row(&rows, "standard");
    assert_eq!(
        row.baseline_reset
            .as_ref()
            .map(|reset| reset.commit.as_str()),
        Some("9f3c1ab72")
    );
    // Only the five post-acceptance observations remain, so the older
    // acceptance cannot hold a stale window open.
    assert_eq!(
        row.verdict,
        DriftVerdict::InsufficientData {
            runs: DRIFT_CONSECUTIVE_RUNS
        }
    );
}

#[test]
fn an_acceptance_ahead_of_every_observation_still_reports_the_run() {
    let (history, _) = stepped_series("standard", "quick");

    let rows = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), "2099-01-01T00:00:00Z")],
    );
    // The marker moves the window a run is judged against; it cannot
    // delete the run, so the series must not vanish from the table.
    let row = only_row(&rows, "standard");
    assert_eq!(row.verdict, DriftVerdict::InsufficientData { runs: 1 });
    assert_eq!(row.current_ms, 20.0);
}

#[test]
fn an_acceptance_is_named_in_the_table_and_under_it() {
    let (history, step_at) = stepped_series("standard", "quick");
    let rows = trend_rows_against(
        &history,
        Some("quick"),
        &[acceptance(Some("standard"), &step_at)],
    );

    let table = render_trend_table(&rows);
    assert!(table.contains("reset_at"), "{table}");
    assert!(table.contains("2dea44485"), "{table}");
    assert!(table.contains("accepted level shifts in effect"), "{table}");
    assert!(
        table.contains("FIG-3157 runs a second physical turn on purpose"),
        "{table}"
    );

    // A table with no acceptance in it says nothing about acceptances: a
    // short baseline there is an evicted cache, not a decision.
    let plain = render_trend_table(&trend_rows_against(&history, Some("quick"), &[]));
    assert!(
        !plain.contains("accepted level shifts in effect"),
        "{plain}"
    );
    assert!(plain.contains("reset_at"), "{plain}");
}

#[test]
fn the_checked_in_acceptances_parse_and_are_complete() {
    let shifts = parse_level_shifts(PERF_DURATION_LEVEL_SHIFTS_JSON)
        .expect("scripts/perf_duration_level_shifts.json must parse");
    for shift in &shifts {
        assert!(!shift.commit.trim().is_empty());
        assert!(!shift.reason.trim().is_empty());
        assert!(shift.effective_from().is_some());
    }
    // The loader reads the same file, so a build that gets this far cannot
    // panic on first use in CI.
    assert_eq!(checked_in_level_shifts().len(), shifts.len());
}

#[test]
fn an_acceptance_nobody_can_audit_is_refused() {
    let cases = [
        (
            "missing commit",
            r#"{"level_shifts":[{"effective_from":"2026-01-01T00:00:00Z","reason":"why"}]}"#,
        ),
        (
            "blank commit",
            r#"{"level_shifts":[{"commit":"  ","effective_from":"2026-01-01T00:00:00Z","reason":"why"}]}"#,
        ),
        (
            "missing reason",
            r#"{"level_shifts":[{"commit":"abc","effective_from":"2026-01-01T00:00:00Z"}]}"#,
        ),
        (
            "blank reason",
            r#"{"level_shifts":[{"commit":"abc","effective_from":"2026-01-01T00:00:00Z","reason":" "}]}"#,
        ),
        (
            "unparseable effective_from",
            r#"{"level_shifts":[{"commit":"abc","effective_from":"yesterday","reason":"why"}]}"#,
        ),
        (
            "empty selector instead of an omitted one",
            r#"{"level_shifts":[{"scenario":"","commit":"abc","effective_from":"2026-01-01T00:00:00Z","reason":"why"}]}"#,
        ),
        (
            "unknown field",
            r#"{"level_shifts":[{"until":"2026-02-01T00:00:00Z","commit":"abc","effective_from":"2026-01-01T00:00:00Z","reason":"why"}]}"#,
        ),
    ];
    for (name, json) in cases {
        assert!(
            parse_level_shifts(json).is_err(),
            "{name} must be refused, not silently accepted"
        );
    }

    // And the shape that carries its audit trail is accepted.
    let accepted = parse_level_shifts(
        r#"{"level_shifts":[{"profile":"full","scenario":"deep_turn_composition","commit":"2dea44485","effective_from":"2026-09-16T12:30:19+00:00","reason":"FIG-3157"}]}"#,
    )
    .expect("a complete marker parses");
    assert_eq!(accepted.len(), 1);
    assert!(accepted[0].matches("full", "deep_turn_composition", "total_ms"));
    assert!(!accepted[0].matches("quick", "deep_turn_composition", "total_ms"));
    assert!(!accepted[0].matches("full", "standard", "total_ms"));
}

fn record(scenario: &str, profile: &str, index: usize, total_ms: f64) -> DurationHistoryRecord {
    DurationHistoryRecord {
        version: HISTORY_RECORD_VERSION,
        scenario: scenario.to_string(),
        profile: profile.to_string(),
        runs: 2,
        warmups: 0,
        turns: 3,
        build_mode: BuildMode::current(),
        commit: format!("commit{index:04}"),
        run_id: format!("{index}"),
        recorded_at: format!("2026-01-01T{:02}:{:02}:00Z", index / 60, index % 60),
        total_ms,
        total_p95_ms: Some(total_ms + 5.0),
        duration_metrics_ms: BTreeMap::new(),
    }
}
