async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let _ = context.sessions().create_session(()).await;
    let _ = context.sessions().close_session("child").await;
    let _ = context.sessions().start_turn("child", "turn", ()).await;
    let _ = context.sessions().set_tool_membership(&[], true).await;
}

fn main() {}
