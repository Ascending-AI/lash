async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.processes().start(()).await;
    let _ = context.processes().cancel("process-1").await;
    let _ = context.processes().signal("process-1").await;
    let _ = context.processes().await_process("process-1").await;
    let _ = context.processes().complete_external("process-1").await;
}

fn main() {}
