async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.triggers().emit("trigger", ()).await;
}

fn main() {}
