async fn check(
    unrelated: (),
) {
    let _ = lash::LashCore::delete_session("session-id", unrelated).await;
}

fn main() {}
