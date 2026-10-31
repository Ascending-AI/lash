async fn check(
    session: lash::LashSession,
    scope: (),
) {
    let _ = session.turn(lash::TurnInput::text("hello")).run(scope).await;
}

fn main() {}
