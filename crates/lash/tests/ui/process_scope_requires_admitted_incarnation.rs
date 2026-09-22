fn check(controller: &lash::runtime::NativeRuntimeEffectController) {
    // A process scope is a reusable name, not an admission: the controller
    // constructors take an AdmittedScope, so a bare scope cannot build one.
    let _ = lash::runtime::ScopedEffectController::borrowed(
        controller,
        lash::runtime::ExecutionScope::process("unadmitted"),
    );
}

fn repin(scoped: &lash::runtime::ScopedEffectController<'_>) {
    // Nor can a same-name successor's incarnation be pinned onto a controller
    // after construction: there is no post-admission pin call.
    let _ = scoped.repin_incarnation(lash::process::ProcessRef::new(
        "unadmitted",
        lash::process::ProcessIncarnation::from_registration_sequence(2),
    ));
}

fn main() {}
