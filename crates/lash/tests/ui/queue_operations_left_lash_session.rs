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

// `await_queued_work_batch` is gone from the Durable Session too: it polled
// `queued_work()` — which hosts already have — and answered "no longer
// pending", which a claim makes true before the work ran. The Session
// Observation stream carries the outcome instead.
async fn queued_work_wait_is_gone(durable: lash::DurableSession) {
    let _ = durable
        .await_queued_work_batch(&lash::BatchId::from("qwb:batch"))
        .await;
}

// FIG-3373, ADR 0089: the child-administration facade is gone as well — a
// host-run related session is an ordinary session opened with
// `core.session(id).parent(parent)`, so `SessionAdmin::children` and
// `ChildSessionAdmin` must stay removed.
async fn child_admin_is_gone(session: lash::LashSession) {
    let _ = session.admin().children();
}

fn child_admin_type_is_gone(_: lash::admin::ChildSessionAdmin) {}

fn main() {}
