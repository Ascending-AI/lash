use lash::plugins::AgentFrameId;

fn main() {
    let frame_key: AgentFrameId = "caller-provided-frame-key".to_string();

    let _ = lash::persistence::QueuedWorkPayload::agent_frame_task(
        frame_key,
        "durable follow-on task",
        None,
    );
}
