async fn check(
    session: lash::LashSession,
    events: &dyn lash::TurnActivitySink,
    scope: lash::runtime::ActorContext,
) {
    let _ = session
        .turn(lash::TurnInput::text("hello"))
        .stream(events, scope)
        .await;
}

fn main() {}
