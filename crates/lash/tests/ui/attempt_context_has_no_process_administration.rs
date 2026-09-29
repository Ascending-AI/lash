// An attempt reads processes; it does not administer them. A start, cancel or
// signal is a declared intent the runtime realizes after the attempt commits,
// and a wait is a pending completion the runtime parks.
async fn attempt_body(context: &lash::tools::AttemptContext<'_>) {
    let processes = context.processes();
    let _ = processes
        .list_handles_filtered(&lash::process::ProcessListFilter::default())
        .await;
    let _ = processes.start();
    let _ = processes.cancel();
    let _ = processes.signal();
    let _ = processes.await_terminal();
}

fn main() {}
