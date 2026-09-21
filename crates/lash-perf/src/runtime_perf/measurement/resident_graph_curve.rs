use super::*;
use lash_core::session_graph::RealizedNodeTimestamp;
use lash_sansio::SessionId;

/// Resident-graph sizes swept by the curve. The defect under measurement is
/// per-turn cost linear in resident history, so the sizes span two orders of
/// magnitude and include the empty graph.
const RESIDENT_GRAPH_SIZES: [usize; 4] = [0, 32, 128, 512];

/// Per-resident-node slope cap for the record-vector copy-on-write phase, in
/// allocated bytes. A snapshot-forced copy of `Vec<Arc<SessionNodeRecord>>`
/// is pointer-sized per node (8 bytes plus vector growth slack); the pre-Arc
/// layout copied whole `SessionNodeRecord`s at orders of magnitude more, so
/// this ceiling fails that regression outright.
const MAX_COW_BYTES_PER_RESIDENT_NODE: f64 = 32.0;

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
    graph.read_model(None).map_err(anyhow::Error::from)?;
    Ok(ResidentGraphFixture {
        resident_nodes,
        graph,
    })
}

/// The F3 N-curve: against a resident graph whose snapshot is still held
/// (the frozen read view and rollback holder are legitimate by design), run
/// the four graph writes a durable one-message turn performs — editor
/// construction, append adoption, draft-id remap, and realized-timestamp
/// application — plus the isolated record copy-on-write — and assert the
/// COW's allocated bytes stay flat in resident size.
pub(super) async fn run_once_resident_graph_append_curve(
    chat_turns: usize,
) -> anyhow::Result<RuntimePerfRunResult> {
    let mut run = RunRecorder::start(RuntimePerfScenario::ResidentGraphAppendCurve, chat_turns);
    let fixtures = run
        .seed(async {
            RESIDENT_GRAPH_SIZES
                .iter()
                .map(|resident_nodes| seed_resident_graph(*resident_nodes))
                .collect::<anyhow::Result<Vec<_>>>()
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
            .map(|fixture| fixture.graph.nodes.len())
            .sum(),
        active_path_messages: fixtures
            .iter()
            .map(|fixture| fixture.graph.nodes.len())
            .sum(),
        ..RunTail::default()
    });
    assert_cow_allocations_flat_in_resident_size(&result.phase_profile)?;
    let _ = exported;
    Ok(result)
}

/// The record COW must not scale per resident record: a snapshot-forced copy
/// of `Vec<Arc<SessionNodeRecord>>` is pointer-sized, so the per-node
/// allocation slope stays under [`MAX_COW_BYTES_PER_RESIDENT_NODE`].
fn assert_cow_allocations_flat_in_resident_size(
    phase_profile: &BTreeMap<String, RuntimePerfPhaseRunResult>,
) -> anyhow::Result<()> {
    let mean_bytes = |resident_nodes: usize| -> anyhow::Result<f64> {
        let phase = phase_profile
            .get(&resident_graph_phase(resident_nodes, "cow"))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "resident graph append curve emitted no cow phase at size {resident_nodes}"
                )
            })?;
        Ok(phase.allocations.bytes_allocated as f64 / phase.samples as f64)
    };
    let baseline = mean_bytes(RESIDENT_GRAPH_SIZES[0])?;
    for resident_nodes in &RESIDENT_GRAPH_SIZES[1..] {
        let slope = (mean_bytes(*resident_nodes)? - baseline) / *resident_nodes as f64;
        if slope > MAX_COW_BYTES_PER_RESIDENT_NODE {
            anyhow::bail!(
                "record copy-on-write allocation grew {slope:.1} bytes per resident node at size {resident_nodes} \
                 (cap {MAX_COW_BYTES_PER_RESIDENT_NODE}); the whole-record deep copy is back"
            );
        }
    }
    Ok(())
}
