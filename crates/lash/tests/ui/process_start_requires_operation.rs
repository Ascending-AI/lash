async fn check(session: lash::LashSession, request: lash::process::ProcessStartRequest) {
    let _ = session.admin().processes().start(request).await;
}

fn main() {}
