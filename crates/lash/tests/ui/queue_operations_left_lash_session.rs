//! The queue and settled-read operations belong to the Durable Session
//! (FIG-3366, ADR 0097), and the cutover left no forwarding shim on
//! `LashSession` or `LashCore`. Reaching for them on either type must not
//! compile; `session.durable()` / `core.session(id).durable()` is the way.

async fn session_queue_is_gone(session: lash::LashSession) {
    let _ = session.pending_turn_inputs().await;
}

async fn core_reads_are_gone(core: lash::LashCore) {
    let _ = core.session_exists("s").await;
}

fn main() {}
