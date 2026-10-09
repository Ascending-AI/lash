use lash::SessionId;
use lash::persistence::{PersistedSessionConfig, SessionHeadMeta};

fn main() {
    let _ = SessionHeadMeta {
        schema_version: 1,
        session_id: SessionId::from("session"),
        head_revision: 1,
        config: PersistedSessionConfig::new(lash::TurnBudget::Unbounded, lash::MaxToolCalls::new(1024),lash::NoProgressBudget::bounded(12), lash::plugins::SessionToolAccess::ambient()),
        current_frame_node_id: None,
        checkpoint_ref: None,
        leaf_node_id: None,
    };
}
