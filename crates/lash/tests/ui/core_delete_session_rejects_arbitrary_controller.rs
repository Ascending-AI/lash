async fn check(
    unrelated: lash::runtime::ScopedEffectController<'_>,
) {
    let _ = lash::LashCore::delete_session("session-id", unrelated).await;
}

fn main() {}
