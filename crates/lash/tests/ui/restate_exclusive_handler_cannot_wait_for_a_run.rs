// FIG-3837: a Restate host waits for a root only on a handler that holds no
// exclusive object lock. A turn may call its host's own virtual object; a host
// waiting inside that object's exclusive handler would hold the lock the
// turn's call queues behind, and neither would ever finish. An exclusive
// handler accepts and returns the receipt; a shared handler waits.
use lash::restate::RestateWait;
use lash_restate::restate_sdk::context::ObjectContext;

async fn wait_holding_the_lock(handle: lash::SendHandle, ctx: ObjectContext<'_>) {
    let _ = handle.outcome_restate(&ctx, RestateWait::new()).await;
}

fn main() {
    let _ = wait_holding_the_lock;
}
