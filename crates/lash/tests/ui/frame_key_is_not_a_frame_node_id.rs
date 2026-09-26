use lash::FrameKey;
use lash::persistence::SessionGraph;
use lash::plugins::{AgentFrameAssignment, AgentFrameReason};

fn open_frame(graph: &mut SessionGraph, assignment: AgentFrameAssignment) {
    let frame_key = FrameKey::from_caller_material("caller-provided-frame-key").unwrap();

    graph.append_frame_open_with_id_at(
        frame_key,
        FrameKey::from_caller_material("caller-provided-frame-key").unwrap(),
        AgentFrameReason::initial(),
        assignment,
        Default::default(),
        String::new(),
    );
}

fn main() {
    let _ = open_frame;
}
