fn check(
    controller: &lash::runtime::ActorContext,
    process_id: lash::ProcessId,
) {
    // A process scope names a minted process, not an admission: a context is
    // scoped by an AdmittedScope, so a bare scope cannot scope one.
    let _ = controller.scoped(lash::runtime::ExecutionScope::process(process_id));
}

fn main() {}
