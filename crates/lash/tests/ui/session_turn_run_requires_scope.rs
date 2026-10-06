async fn check(
    session: lash::LashSession,
    scope: lash::runtime::ActorContext,
) {
    let _ = session.turn(lash::TurnInput::text("hello")).run(scope).await;
}

fn main() {}
