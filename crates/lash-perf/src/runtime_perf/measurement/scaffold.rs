use super::*;

/// One closed span boundary: wall clock since the meter opened, the
/// allocation delta across it, and the memory sampled at both ends.
///
/// The sample order matches every migrated site: the meter opens with
/// `allocator_stats()`, `process_memory_sample()`, then `Instant::now()`;
/// it closes with `elapsed_ms`, `allocator_stats()`, then
/// `process_memory_sample()`.
pub(crate) struct MeasuredSpan {
    pub(crate) duration_ms: f64,
    pub(crate) allocations: RuntimePerfAllocationDelta,
    pub(crate) memory_before: ProcessMemorySample,
    pub(crate) memory_after: ProcessMemorySample,
}

impl MeasuredSpan {
    fn stage_result(&self) -> RuntimePerfStageRunResult {
        RuntimePerfStageRunResult::measured(
            self.duration_ms,
            self.allocations.clone(),
            self.memory_after.rss_kb,
        )
    }
}

/// Opens a measured span; `finish` closes it.
pub(crate) struct SpanMeter {
    pub(crate) started: Instant,
    pub(crate) alloc_before: Stats,
    pub(crate) memory_before: ProcessMemorySample,
}

impl SpanMeter {
    pub(crate) fn start() -> Self {
        let alloc_before = allocator_stats();
        let memory_before = process_memory_sample();
        Self {
            started: Instant::now(),
            alloc_before,
            memory_before,
        }
    }

    pub(crate) fn finish(self) -> MeasuredSpan {
        let duration_ms = elapsed_ms(self.started);
        let allocations = alloc_delta(self.alloc_before, allocator_stats());
        let memory_after = process_memory_sample();
        MeasuredSpan {
            duration_ms,
            allocations,
            memory_before: self.memory_before,
            memory_after,
        }
    }
}

/// Per-turn payload a `run` closure hands back to [`RunRecorder::turn`]:
/// the produced value plus the turn-level fields that are known inside the
/// turn span. Fields measured after the await span (usage diffs, probe
/// drains) are patched in through the `post` hook of `turn_then`.
#[derive(Default)]
pub(crate) struct TurnTail {
    pub(crate) phase_profile: BTreeMap<String, RuntimePerfPhaseRunResult>,
    pub(crate) turn_usage: TokenUsage,
    pub(crate) usage_delta: SessionUsageReport,
    pub(crate) cumulative_usage: SessionUsageReport,
}

pub(crate) struct TurnRun<T> {
    pub(crate) value: T,
    pub(crate) tail: TurnTail,
}

/// The two spans a turn measured, handed to the `post` hook so turn-level
/// phase entries derived from the turn's own timing (rather than sub-phase
/// probes) can be built without re-measuring.
pub(crate) struct TurnSpans {
    pub(crate) run: MeasuredSpan,
    pub(crate) await_background_work: MeasuredSpan,
}

/// The run-level fields assembled at the tail of `run_once*`: everything
/// the scaffold cannot measure itself. `None` fields fall back to the
/// measured defaults — `memory` spans the whole run, `phase_profile` sums
/// the turn profiles, `total_stage` closes the total meter.
#[derive(Default)]
pub(crate) struct RunTail {
    pub(crate) session_nodes: usize,
    pub(crate) active_path_messages: usize,
    pub(crate) extra_counters: BTreeMap<String, u64>,
    pub(crate) metric_samples: BTreeMap<String, Vec<f64>>,
    pub(crate) metric_samples_ms: BTreeMap<String, Vec<f64>>,
    pub(crate) stack_profile: Option<StackProfile>,
    pub(crate) memory: Option<RuntimePerfMemoryRunResult>,
    pub(crate) phase_profile: Option<BTreeMap<String, RuntimePerfPhaseRunResult>>,
    /// Overrides the total span's allocation delta for sites that keep
    /// measuring work after the last recorded stage boundary.
    pub(crate) total_alloc: Option<RuntimePerfAllocationDelta>,
    pub(crate) total_stage: Option<RuntimePerfStageRunResult>,
    pub(crate) cumulative_usage: SessionUsageReport,
}

/// The shared measurement scaffold behind every `run_once*` site: opens the
/// total meter, records the named `build`/`seed`/`export` spans and each
/// turn's `run`/`await_background_work` spans, then `finish` folds the
/// turns into the run-level stage map and emits the run tail.
///
/// `total_alloc` and `last_memory` are re-sampled at every span close —
/// matching the sites, which read `allocator_stats()` once more for the
/// total delta right after the final span's memory sample — so `finish`
/// reports the values at the last closed boundary.
pub(crate) struct RunRecorder {
    scenario: RuntimePerfScenario,
    chat_turns: usize,
    total: SpanMeter,
    stage_entries: Vec<(&'static str, RuntimePerfStageRunResult)>,
    turns: Vec<RuntimePerfTurnResult>,
    last_memory: ProcessMemorySample,
    total_alloc: RuntimePerfAllocationDelta,
}

impl RunRecorder {
    pub(crate) fn start(scenario: RuntimePerfScenario, chat_turns: usize) -> Self {
        let total = SpanMeter::start();
        Self {
            scenario,
            chat_turns,
            last_memory: total.memory_before,
            total,
            stage_entries: Vec::new(),
            turns: Vec::with_capacity(chat_turns),
            total_alloc: zero_allocation_delta(),
        }
    }

    async fn measured_span<T, F>(&mut self, f: F) -> anyhow::Result<(T, MeasuredSpan)>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let meter = SpanMeter::start();
        let value = f.await?;
        let span = meter.finish();
        self.total_alloc = alloc_delta(self.total.alloc_before, allocator_stats());
        self.last_memory = span.memory_after;
        Ok((value, span))
    }

    /// Records one named run-level stage span (`build_runtime`,
    /// `seed_state`, `export_state`, ...).
    pub(crate) async fn stage<T, F>(&mut self, name: &'static str, f: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        let (value, span) = self.measured_span(f).await?;
        self.stage_entries.push((name, span.stage_result()));
        Ok(value)
    }

    pub(crate) async fn build<T, F>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        self.stage(stage::BUILD_RUNTIME, f).await
    }

    pub(crate) async fn seed<T, F>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        self.stage(stage::SEED_STATE, f).await
    }

    pub(crate) async fn export<T, F>(&mut self, f: F) -> anyhow::Result<T>
    where
        F: Future<Output = anyhow::Result<T>>,
    {
        self.stage(stage::EXPORT_STATE, f).await
    }

    /// Measures one turn as a `run` span followed by an
    /// `await_background_work` span, then pushes the folded
    /// `RuntimePerfTurnResult`. The `run` closure returns the produced
    /// value together with the `TurnTail` fields known inside the span.
    pub(crate) async fn turn<T, R, A>(
        &mut self,
        turn_index: usize,
        run: R,
        await_background_work: A,
    ) -> anyhow::Result<T>
    where
        R: Future<Output = anyhow::Result<TurnRun<T>>>,
        A: Future<Output = anyhow::Result<()>>,
    {
        self.turn_then(turn_index, run, await_background_work, |_, _, _| Ok(()))
            .await
    }

    /// [`RunRecorder::turn`] with a `post` hook that runs after the await
    /// span closed and may still patch the [`TurnTail`] — for the fields a
    /// site can only know once the drain finished (usage reports, probe
    /// drains, phase entries derived from the turn span itself).
    pub(crate) async fn turn_then<T, R, A, P>(
        &mut self,
        turn_index: usize,
        run: R,
        await_background_work: A,
        post: P,
    ) -> anyhow::Result<T>
    where
        R: Future<Output = anyhow::Result<TurnRun<T>>>,
        A: Future<Output = anyhow::Result<()>>,
        P: FnOnce(&T, &TurnSpans, &mut TurnTail) -> anyhow::Result<()>,
    {
        let run_meter = SpanMeter::start();
        let TurnRun { value, mut tail } = run.await?;
        let run_span = run_meter.finish();
        self.total_alloc = alloc_delta(self.total.alloc_before, allocator_stats());
        self.last_memory = run_span.memory_after;

        let await_meter = SpanMeter::start();
        await_background_work.await?;
        let await_span = await_meter.finish();
        self.total_alloc = alloc_delta(self.total.alloc_before, allocator_stats());
        self.last_memory = await_span.memory_after;

        let spans = TurnSpans {
            run: run_span,
            await_background_work: await_span,
        };
        post(&value, &spans, &mut tail)?;

        let turn_total_alloc = sum_allocation_deltas([
            &spans.run.allocations,
            &spans.await_background_work.allocations,
        ]);
        self.turns.push(RuntimePerfTurnResult {
            turn_index,
            stages: turn_stages(
                spans.run.stage_result(),
                Some(spans.await_background_work.stage_result()),
                RuntimePerfStageRunResult::measured(
                    round3(spans.run.duration_ms + spans.await_background_work.duration_ms),
                    turn_total_alloc,
                    spans.await_background_work.memory_after.rss_kb,
                ),
            ),
            memory: memory_span(
                spans.run.memory_before,
                spans.await_background_work.memory_after,
            ),
            phase_profile: tail.phase_profile,
            turn_usage: tail.turn_usage,
            usage_delta: tail.usage_delta,
            cumulative_usage: tail.cumulative_usage,
        });
        Ok(value)
    }

    /// The turns recorded so far — for tails that aggregate over them
    /// beyond the default phase-profile fold.
    pub(crate) fn turns(&self) -> &[RuntimePerfTurnResult] {
        &self.turns
    }

    /// The total allocation delta sampled now, for sites that keep
    /// measuring past the last stage boundary.
    pub(crate) fn total_alloc_snapshot(&self) -> RuntimePerfAllocationDelta {
        alloc_delta(self.total.alloc_before, allocator_stats())
    }

    /// Pushes a pre-measured stage entry — for sites that fabricate rather
    /// than measure their spans (the report test fixture).
    #[cfg(test)]
    pub(crate) fn record_stage(&mut self, name: &'static str, result: RuntimePerfStageRunResult) {
        self.stage_entries.push((name, result));
    }

    /// Pushes a pre-assembled turn — the fabricated counterpart of
    /// [`RunRecorder::turn`].
    #[cfg(test)]
    pub(crate) fn record_turn(&mut self, turn: RuntimePerfTurnResult) {
        self.turns.push(turn);
    }

    /// Closes the total meter and emits the run tail: `total` covers the
    /// whole run, run-level `run_turn`/`await_background_work` entries are
    /// the sum over the recorded turns, and the closing memory reading is
    /// the last span boundary's.
    pub(crate) fn finish(self, tail: RunTail) -> RuntimePerfRunResult {
        let Self {
            scenario,
            chat_turns,
            total,
            mut stage_entries,
            turns,
            last_memory,
            total_alloc,
        } = self;
        let total_stage = tail.total_stage.unwrap_or_else(|| {
            RuntimePerfStageRunResult::measured(
                elapsed_ms(total.started),
                tail.total_alloc.clone().unwrap_or(total_alloc),
                last_memory.rss_kb,
            )
        });
        stage_entries.push((stage::TOTAL, total_stage));
        RuntimePerfRunResult {
            scenario: scenario.name().to_string(),
            scenario_harness: scenario.scenario_harness().name().to_string(),
            chat_turns,
            stack_profile: tail.stack_profile,
            stages: run_stages(stage_entries, &turns),
            session_nodes: tail.session_nodes,
            active_path_messages: tail.active_path_messages,
            extra_counters: tail.extra_counters,
            metric_samples: tail.metric_samples,
            metric_samples_ms: tail.metric_samples_ms,
            memory: tail
                .memory
                .unwrap_or_else(|| memory_span(total.memory_before, last_memory)),
            phase_profile: tail.phase_profile.unwrap_or_else(|| {
                sum_phase_profiles(turns.iter().map(|turn| &turn.phase_profile))
            }),
            turns,
            cumulative_usage: tail.cumulative_usage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_fold_sums_turn_spans_and_mirrors_last_turn_memory() {
        let scenario = RuntimePerfScenario::TurnCheckpoint;
        let mut recorder = RunRecorder::start(scenario, 2);
        for turn_index in 0..2 {
            recorder
                .turn(
                    turn_index,
                    async {
                        tokio::task::yield_now().await;
                        Ok(TurnRun {
                            value: (),
                            tail: TurnTail::default(),
                        })
                    },
                    async { Ok(()) },
                )
                .await
                .unwrap();
        }
        // No export span: the last recorded boundary is the last turn's
        // await close, and the run tail must mirror it.
        let result = recorder.finish(RunTail::default());
        assert_eq!(result.turns.len(), 2);

        // Run-level stage sums equal the sum of the turn spans.
        let run_turn = result.stage(stage::RUN_TURN).expect("run_turn folded");
        let expected_ms = round3(
            result
                .turns
                .iter()
                .map(|turn| turn.stage(stage::RUN_TURN).unwrap().duration_ms)
                .sum(),
        );
        assert_eq!(run_turn.duration_ms, expected_ms);
        let await_stage = result
            .stage(stage::AWAIT_BACKGROUND_WORK)
            .expect("await_background_work folded");
        let expected_await_ms = round3(
            result
                .turns
                .iter()
                .map(|turn| {
                    turn.stage(stage::AWAIT_BACKGROUND_WORK)
                        .unwrap()
                        .duration_ms
                })
                .sum(),
        );
        assert_eq!(await_stage.duration_ms, expected_await_ms);

        // The post-turn memory reading mirrors the last turn.
        let last_turn = result.turns.last().unwrap();
        assert_eq!(
            result.memory.peak_hwm_after_kb,
            last_turn.memory.peak_hwm_after_kb
        );
        assert_eq!(
            result.stage(stage::TOTAL).unwrap().rss_after_kb,
            last_turn.stage(stage::TOTAL).unwrap().rss_after_kb
        );
    }
}
