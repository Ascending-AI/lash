use super::*;
use lash_core::session_graph::RealizedNodeTimestamp;
use lash_sansio::SessionId;

/// Resident-graph sizes swept by the curve. The defect under measurement is
/// per-turn cost linear in resident history, so the sizes span two orders of
/// magnitude and include the empty graph.
const RESIDENT_GRAPH_SIZES: [usize; 4] = [0, 32, 128, 512];

/// Per-resident-node allocation slope caps per phase, in bytes. A
/// snapshot-forced copy of `Vec<Arc<SessionNodeRecord>>` is pointer-sized
/// per node; construction, remap, and timestamp application touch only the
/// appended tail, so their slopes should be near zero. Append additionally
/// copies the node-pointer vector and the active-path index vec (two
/// pointer-width copies plus vector growth slack), so its cap is wider.
/// `builder` borrows the resident id index outright; `snapshot_append`
/// pays the shared-cache detach (bounded id delta, index vec, node-pointer
/// vec) plus the append itself.
const MAX_SLOPE_BYTES_PER_RESIDENT_NODE: [(&str, f64); 8] = [
    ("construct", 32.0),
    ("cow", 32.0),
    ("append", 96.0),
    ("remap", 32.0),
    ("timestamps", 32.0),
    ("builder", 32.0),
    ("snapshot_append", 48.0),
    ("event_read", 64.0),
];

struct ResidentGraphFixture {
    resident_nodes: usize,
    graph: lash_core::SessionGraph,
}

fn resident_graph_phase(resident_nodes: usize, operation: &str) -> String {
    format!("resident_graph_append.n{resident_nodes}.{operation}")
}

fn seed_resident_graph(resident_nodes: usize) -> anyhow::Result<ResidentGraphFixture> {
    let mut graph = lash_core::SessionGraph::default();
    if resident_nodes > 0 {
        // One builder batch, not N append calls: per-message appends would
        // re-clone the resident id set on every message and make the seed
        // stage quadratic.
        let mut builder = graph.append_builder_in_namespace("perf-seed");
        let nodes = builder.append_messages_at(
            (0..resident_nodes).map(|index| {
                checkpoint_message(
                    format!("resident-msg-{resident_nodes}-{index}"),
                    if index.is_multiple_of(2) {
                        MessageRole::User
                    } else {
                        MessageRole::Assistant
                    },
                    format!("Resident graph fixture message {index} at size {resident_nodes}."),
                )
            }),
            "2026-09-12T00:00:00Z".to_string(),
        );
        graph
            .apply_append(&GraphAppend::Extend { nodes })
            .map_err(anyhow::Error::from)?;
    }
    // A resident graph at turn start has already served reads: its by_id /
    // active-path cache is warm, which is what makes the append's shared-cache
    // detach and the remap's by_id lookup representative.
    graph.read_model();
    Ok(ResidentGraphFixture {
        resident_nodes,
        graph,
    })
}

/// The batch-seeded fixture cannot see per-append index or read-model
/// drift: it lands all N nodes in one `apply_append`, so the id-index delta
/// and pending read-model tails are empty no matter how large N is. This
/// fixture warms the cache on an empty graph and then grows through the
/// incremental `append_message` path a live session actually uses, so
/// builder creation and held-snapshot appends are measured against the
/// structures real append-only use produces.
fn seed_growth_graph(resident_nodes: usize) -> anyhow::Result<ResidentGraphFixture> {
    let mut graph = lash_core::SessionGraph::default();
    graph.read_model();
    for index in 0..resident_nodes {
        graph.append_message(checkpoint_message(
            format!("growth-msg-{resident_nodes}-{index}"),
            if index.is_multiple_of(2) {
                MessageRole::User
            } else {
                MessageRole::Assistant
            },
            format!("Growth fixture message {index} at size {resident_nodes}."),
        ));
    }
    // Fold the appended read-model tail: a resident graph at turn start
    // holds a materialized read model, not N pending records.
    let _ = graph.read_model();
    Ok(ResidentGraphFixture {
        resident_nodes,
        graph,
    })
}

/// The F3 N-curve: against a resident graph whose snapshot is still held
/// (the frozen read view and rollback holder are legitimate by design), run
/// the four graph writes a durable one-message turn performs — editor
/// construction, append adoption, draft-id remap, and realized-timestamp
/// application — plus the isolated record copy-on-write — and assert every
/// phase's allocated bytes stay flat in resident size.
pub(super) async fn run_once_resident_graph_append_curve(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let mut run = RunRecorder::start(RuntimePerfScenario::ResidentGraphAppendCurve, chat_turns);
    let (fixtures, mut growth_fixtures) = run
        .seed(async {
            let fixtures = RESIDENT_GRAPH_SIZES
                .iter()
                .map(|resident_nodes| seed_resident_graph(*resident_nodes))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let growth_fixtures = RESIDENT_GRAPH_SIZES
                .iter()
                .map(|resident_nodes| seed_growth_graph(*resident_nodes))
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok((fixtures, growth_fixtures))
        })
        .await?;

    for turn_index in 0..chat_turns {
        let fixtures = &fixtures;
        run.turn(
            turn_index,
            async {
                let mut phase_profile = BTreeMap::new();
                let session_id = SessionId::from("perf-resident-graph-append");
                for fixture in fixtures {
                    let resident_nodes = fixture.resident_nodes;
                    // The pre-turn snapshot outlives the whole adoption
                    // sequence: the frozen read view and the rollback holder
                    // both share the base graph's records.
                    let pre_turn_snapshot = Arc::new(fixture.graph.clone());
                    let mut adopted = Arc::unwrap_or_clone(Arc::clone(&pre_turn_snapshot));

                    let (appended, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "construct"),
                        || {
                            let mut builder = adopted.append_builder_in_namespace(format!(
                                "perf-turn-{turn_index}"
                            ));
                            Ok::<_, anyhow::Error>(builder.append_messages_at(
                                [checkpoint_message(
                                    format!("append-msg-{resident_nodes}-{turn_index}"),
                                    MessageRole::Assistant,
                                    format!(
                                        "Measured one-node append at resident size {resident_nodes}."
                                    ),
                                )],
                                "2026-09-12T00:00:00Z".to_string(),
                            ))
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    // The record COW in isolation: cloning the graph while
                    // its snapshot is held and taking `data_mut` forces the
                    // `Vec<Arc<SessionNodeRecord>>` copy that used to be N
                    // whole records.
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "cow"),
                        || {
                            let mut cow = Arc::unwrap_or_clone(Arc::clone(&pre_turn_snapshot));
                            let _ = cow.data_mut();
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    let draft_node_id = appended[0].node_id.clone();
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "append"),
                        || {
                            adopted
                                .apply_append(&GraphAppend::Extend { nodes: appended })
                                .map_err(anyhow::Error::from)
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    let derived_node_id = lash_core::NodeId::from(format!(
                        "perf-derived/{resident_nodes}/{turn_index}"
                    ));
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "remap"),
                        || {
                            adopted.remap_node_ids(
                                &session_id,
                                &[(draft_node_id, derived_node_id.clone())],
                            );
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    // Between remap and receipt application the store reads
                    // frame records, which rebuilds the by_id cache the
                    // timestamp lookup resolves through.
                    assert!(adopted.find_node(derived_node_id.as_str()).is_some());

                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "timestamps"),
                        || {
                            adopted.apply_realized_node_timestamps(&[RealizedNodeTimestamp {
                                node_id: derived_node_id,
                                timestamp: "2026-09-12T00:00:01Z".to_string(),
                            }]);
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);
                }
                for fixture in growth_fixtures.iter_mut() {
                    let resident_nodes = fixture.resident_nodes;
                    // Builder creation on an incrementally grown warm graph:
                    // the id index must be borrowed, not re-cloned.
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "builder"),
                        || {
                            let _builder = fixture
                                .graph
                                .append_builder_in_namespace(format!(
                                    "perf-growth-{turn_index}"
                                ));
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    // A single append while a snapshot pins the grown graph:
                    // the detach must not re-clone accumulated id-index or
                    // read-model state linear in resident size.
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "snapshot_append"),
                        || {
                            let mut adopted = fixture.graph.clone();
                            adopted.append_message(checkpoint_message(
                                format!("growth-append-{resident_nodes}-{turn_index}"),
                                MessageRole::Assistant,
                                format!(
                                    "Measured held-snapshot append at resident size {resident_nodes}."
                                ),
                            ));
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);

                    // An event-only append followed by a read on the warm
                    // graph itself: materialization extends the events vec
                    // in place while the message vec and its render cache
                    // stay untouched. Unconditionally cloning either shared
                    // vec shows up as a full per-node copy here.
                    let (_, phase) = measure_runtime_perf_phase(
                        &resident_graph_phase(resident_nodes, "event_read"),
                        || {
                            fixture.graph.append_protocol_event(
                                lash_core::ProtocolEvent::typed(
                                    "perf_growth_event",
                                    serde_json::json!({"turn": turn_index}),
                                )
                                .map_err(anyhow::Error::from)?,
                            );
                            let read = fixture
                                .graph
                                .read_model();
                            std::hint::black_box(read.messages.len());
                            Ok(())
                        },
                    )?;
                    phase_profile.insert(phase.0, phase.1);
                }
                Ok(TurnRun {
                    value: (),
                    tail: TurnTail {
                        phase_profile,
                        ..TurnTail::default()
                    },
                })
            },
            async {
                tokio::task::yield_now().await;
                Ok(())
            },
        )
        .await?;
    }

    let exported = run
        .export(async {
            serde_json::to_vec(&fixtures[fixtures.len() - 1].graph).map_err(anyhow::Error::from)
        })
        .await?;

    let result = run.finish(RunTail {
        session_nodes: fixtures
            .iter()
            .chain(growth_fixtures.iter())
            .map(|fixture| fixture.graph.nodes.len())
            .sum(),
        active_path_messages: fixtures
            .iter()
            .chain(growth_fixtures.iter())
            .map(|fixture| fixture.graph.nodes.len())
            .sum(),
        ..RunTail::default()
    });
    assert_allocations_flat_in_resident_size(&result.phase_profile)?;
    let _ = exported;
    Ok(result)
}

/// Every phase of a one-node-append turn must stay flat in resident size:
/// the record COW copies pointers, construction borrows the resident index,
/// and append/remap/timestamps touch only the appended tail. A slope over
/// the phase's cap means a whole-resident clone or scan is back.
fn assert_allocations_flat_in_resident_size(
    phase_profile: &BTreeMap<String, RuntimePerfPhaseRunResult>,
) -> anyhow::Result<()> {
    let mean_bytes = |resident_nodes: usize, operation: &str| -> anyhow::Result<f64> {
        let phase = phase_profile
            .get(&resident_graph_phase(resident_nodes, operation))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "resident graph append curve emitted no {operation} phase at size {resident_nodes}"
                )
            })?;
        Ok(phase.allocations.bytes_allocated as f64 / phase.samples as f64)
    };
    for (operation, cap) in MAX_SLOPE_BYTES_PER_RESIDENT_NODE {
        let baseline = mean_bytes(RESIDENT_GRAPH_SIZES[0], operation)?;
        for resident_nodes in &RESIDENT_GRAPH_SIZES[1..] {
            let slope =
                (mean_bytes(*resident_nodes, operation)? - baseline) / *resident_nodes as f64;
            if slope > cap {
                anyhow::bail!(
                    "{operation} allocation grew {slope:.1} bytes per resident node at size {resident_nodes} \
                     (cap {cap}); a whole-resident clone or scan is back"
                );
            }
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod commit_scaling_tests {
    use super::*;
    use lash_sansio::core_support::MessageSequenceCoreSupport;

    #[expect(unsafe_code, reason = "clock_gettime writes to this local timespec")]
    fn thread_cpu_ns() -> anyhow::Result<u64> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: time is a valid writable timespec for this call.
        if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(u64::try_from(time.tv_sec)? * 1_000_000_000 + u64::try_from(time.tv_nsec)?)
    }

    fn commit(
        state: &mut RuntimeSessionState,
        turn: usize,
    ) -> anyhow::Result<lash_core::SessionReadView> {
        let base_graph = Arc::new(state.session_graph.clone());
        let base = base_graph.read_model();
        let message =
            checkpoint_message(format!("turn-{turn}"), MessageRole::User, "one turn".into());
        let messages =
            lash_sansio::MessageSequence::from_base_and_delta(base.messages, vec![message.clone()]);
        let view = lash_core::SessionReadView::derived_from_persisted_state(
            state,
            state.policy.clone(),
            turn,
            state.effective_protocol_turn_options(),
            base_graph.clone(),
            messages,
        );
        // The editor releases its base before the resident adopts its appended tail.
        drop(base_graph);
        let mut graph = state.session_graph.clone();
        let nodes = graph
            .append_builder_in_namespace(format!("turn-{turn}"))
            .append_messages_at([message], "2026-09-30T00:00:00Z".into());
        let draft = nodes[0].node_id.clone();
        graph.apply_append(&GraphAppend::Extend { nodes })?;
        graph.remap_node_ids(
            &state.session_id,
            &[(draft, format!("committed-{turn}").into())],
        );
        graph.apply_realized_node_timestamps(&[RealizedNodeTimestamp {
            node_id: format!("committed-{turn}").into(),
            timestamp: "2026-09-30T00:00:01Z".into(),
        }]);
        let mut snapshot = state.to_snapshot();
        snapshot.session_graph = graph;
        state.adopt_snapshot(snapshot);
        std::hint::black_box(state.read_model());
        Ok(view)
    }

    fn measure(turns: usize) -> anyhow::Result<(f64, f64, f64)> {
        const SAMPLES: usize = 256;
        let mut state = RuntimeSessionState::new(lash_core::SessionPolicy::new(
            lash_core::TurnBudget::Unbounded,
        ));
        state.ensure_agent_frame_initialized();
        let mut held = Vec::with_capacity(turns + SAMPLES);
        for turn in 0..turns {
            held.push(commit(&mut state, turn)?);
        }
        let before = allocator_stats();
        let cpu_before = thread_cpu_ns()?;
        for turn in turns..turns + SAMPLES {
            held.push(commit(&mut state, turn)?);
        }
        let cpu = (thread_cpu_ns()? - cpu_before) as f64 / SAMPLES as f64;
        let allocations = alloc_delta(before, allocator_stats());
        assert_eq!(state.agent_frames.len(), 1, "one frame without compaction");
        assert_eq!(held[turns - 1].messages().len(), turns);
        assert_eq!(held[turns + SAMPLES - 1].messages().len(), turns + SAMPLES);
        Ok((
            cpu,
            allocations.bytes_allocated as f64 / SAMPLES as f64,
            allocations.allocations as f64 / SAMPLES as f64,
        ))
    }

    #[test]
    fn one_lane_commit_allocations_stay_flat_at_200_and_1000_turns() -> anyhow::Result<()> {
        // Alternate the sizes to avoid attributing allocator warmup to frame size.
        let mut small = Vec::new();
        let mut large = Vec::new();
        for _ in 0..3 {
            small.push(measure(200)?);
            large.push(measure(1000)?);
        }
        let mean = |samples: &[(f64, f64, f64)]| {
            (
                samples.iter().map(|s| s.0).sum::<f64>() / samples.len() as f64,
                samples.iter().map(|s| s.1).sum::<f64>() / samples.len() as f64,
                samples.iter().map(|s| s.2).sum::<f64>() / samples.len() as f64,
            )
        };
        let small = mean(&small);
        let large = mean(&large);
        println!(
            "one-lane commit: 200 turns {small:?}; 1000 turns {large:?} (diagnostic CPU ns, bytes, allocations per commit)"
        );
        println!(
            "measurement host: loadavg={}; available_parallelism={:?}",
            std::fs::read_to_string("/proc/loadavg")
                .unwrap_or_else(|error| format!("unavailable: {error}"))
                .trim(),
            std::thread::available_parallelism(),
        );
        assert_commit_allocations_flat(small, large);
        Ok(())
    }

    // Thread CPU time excludes descheduling, but still depends on CPU frequency
    // and cache contention. This short shared-host measurement cannot separate
    // those effects from history growth, so CPU stays diagnostic. Allocation
    // counters gate this law; CPU scaling needs a controlled FIG-4439 benchmark.
    fn assert_commit_allocations_flat(small: (f64, f64, f64), large: (f64, f64, f64)) {
        assert!(
            large.1 <= small.1 * 1.4,
            "allocated bytes grow with frame length: {small:?} -> {large:?}"
        );
        assert!(
            large.2 <= small.2 * 1.4,
            "allocation count grows with frame length: {small:?} -> {large:?}"
        );
    }

    #[test]
    fn commit_flatness_verdict_ignores_cpu_noise_and_preserves_allocation_limits() {
        // FIG-4497: the observed failure had flat allocations under host load.
        assert_commit_allocations_flat((57_463.0, 35_009.0, 121.38), (104_100.0, 38_058.0, 130.32));
        let small = (1.0, 100.0, 100.0);
        for cpu in [0.0, 1.0, 1_000_000.0] {
            assert_commit_allocations_flat(small, (cpu, 140.0, 140.0));
            assert!(
                std::panic::catch_unwind(|| {
                    assert_commit_allocations_flat(small, (cpu, 141.0, 100.0));
                })
                .is_err(),
                "byte growth above 1.4x must fail regardless of CPU"
            );
            assert!(
                std::panic::catch_unwind(|| {
                    assert_commit_allocations_flat(small, (cpu, 100.0, 141.0));
                })
                .is_err(),
                "allocation growth above 1.4x must fail regardless of CPU"
            );
        }
    }
}
