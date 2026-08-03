use lash::persistence::{PersistedSessionConfig, SessionHeadMeta};

fn main() {
    let _ = SessionHeadMeta {
        schema_version: 1,
        session_id: "session".to_string(),
        head_revision: 1,
        config: PersistedSessionConfig::default(),
        current_frame_node_id: None,
        checkpoint_ref: None,
        leaf_node_id: None,
    };
}
