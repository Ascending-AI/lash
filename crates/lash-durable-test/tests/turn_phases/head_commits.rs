//! The L3 scenario's refused head commits (FIG-5352): a turn's own commit
//! rewritten so the store must refuse it, its graph nodes carried as built.

use super::*;

/// [`Mode::CollidingCommit`]'s head commit: `commit` with its last two
/// appended nodes replaced by two opens of one frame. Each derives the
/// frame's node id, so the commit passes the store's derivation check and is
/// refused for the collision.
pub(super) fn colliding(commit: TurnCommit) -> Result<TurnCommit, TurnError> {
    let store = |error: lash_core_store::store::StoreError| TurnError::Exec(error.to_string());
    let mut runtime =
        lash_core_store::store::decode_session_commit(&commit.commit.json).map_err(store)?;
    let frame_key = lash_core::FrameKey::from_caller_material("l3-collide")
        .map_err(|error| TurnError::Exec(error.to_string()))?;
    let frame = lash_core::NodeId::from(lash_core_store::session_graph::frame_node_id(
        &session(),
        frame_key.as_str(),
    ));
    let nodes = runtime.graph.nodes_mut();
    let [.., first, second] = nodes else {
        return Err(TurnError::Exec(format!(
            "the turn's commit appends {} nodes, not the two it collides",
            nodes.len()
        )));
    };
    for node in [&mut *first, &mut *second] {
        node.node_id = frame.clone();
        node.payload = lash_core::SessionNodePayload::FrameOpen {
            frame_key: frame_key.clone(),
            reason: lash_core::AgentFrameReason::default(),
            assignment: lash_core::AgentFrameAssignment::unconfigured(
                lash_core::SessionPolicy::new(
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(16),
                    lash_core::NoProgressBudget::bounded(12),
                ),
            ),
        };
    }
    second.parent_node_id = Some(frame);
    Ok(TurnCommit {
        postimage: commit.postimage,
        expected_head: commit.expected_head,
        commit: lash_core_store::store::EncodedSessionCommit {
            descriptor: commit.commit.descriptor,
            ..lash_core_store::store::encode_session_commit(&runtime).map_err(store)?
        },
    })
}

/// The plugin [`Mode::OutsideWriterRange`]'s head commit stamps.
const WRITER_PLUGIN: &str = "l3-plugin";

/// [`Mode::OutsideWriterRange`]'s head commit: `commit` recording
/// [`WRITER_PLUGIN`]'s config at its first format, run under that plugin's
/// config at format 2. The first stamp provisions the plugin's range
/// `[1, 1]`, so the store refuses the second as outside it.
pub(super) fn outside_writer_range(commit: TurnCommit) -> Result<TurnCommit, TurnError> {
    let store = |error: lash_core_store::store::StoreError| TurnError::Exec(error.to_string());
    let mut runtime =
        lash_core_store::store::decode_session_commit(&commit.commit.json).map_err(store)?;
    let mut execution = runtime.config.clone();
    runtime
        .config
        .plugin_config
        .insert_versioned(
            WRITER_PLUGIN,
            lash_core_ids::FormatVersion::ONE,
            serde_json::json!({}),
        )
        .unwrap();
    execution
        .plugin_config
        .insert_versioned(
            WRITER_PLUGIN,
            lash_core_ids::FormatVersion::new(2).expect("2 is a format version"),
            serde_json::json!({}),
        )
        .unwrap();
    runtime.execution_config = Some(Box::new(execution));
    Ok(TurnCommit {
        postimage: commit.postimage,
        expected_head: commit.expected_head,
        commit: lash_core_store::store::EncodedSessionCommit {
            descriptor: commit.commit.descriptor,
            ..lash_core_store::store::encode_session_commit(&runtime).map_err(store)?
        },
    })
}
