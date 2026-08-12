async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.process_events().emit("event", ()).await;
    let _ = context.process_events().wait_event_after("event", 0).await;
}

fn main() {}
