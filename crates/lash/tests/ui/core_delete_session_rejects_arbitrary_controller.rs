async fn check(
    unrelated: lash::runtime::ActorContext,
) {
    let _ = lash::LashCore::delete_session("session-id", unrelated).await;
}

fn main() {}
