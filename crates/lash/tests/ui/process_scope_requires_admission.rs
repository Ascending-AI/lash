fn check(
    controller: &dyn lash::runtime::RuntimeEffectController,
    process_id: lash::ProcessId,
) {
    // A process scope names a minted process, not an admission: the
    // controller constructors take an AdmittedScope, so a bare scope cannot
    // build one.
    let _ = lash::runtime::ScopedEffectController::borrowed(
        controller,
        lash::runtime::ExecutionScope::process(process_id),
    );
}

fn main() {}
