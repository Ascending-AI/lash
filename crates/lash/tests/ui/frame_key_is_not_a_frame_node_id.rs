use lash::FrameKey;

fn main() {
    let frame_key = FrameKey::from_caller_material("caller-provided-frame-key").unwrap();

    let _ = lash::persistence::QueuedWorkPayload::agent_frame_task(
        frame_key,
        "durable follow-on task",
        None,
    );
}
