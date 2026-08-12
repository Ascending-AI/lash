async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.processes().cancel("process-1").await;
}

fn main() {}
