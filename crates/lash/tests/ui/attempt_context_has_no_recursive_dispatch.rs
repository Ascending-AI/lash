async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.dispatch().batch(Vec::new()).await;
}

fn main() {}
