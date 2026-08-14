fn external_implementation(
) -> std::sync::Arc<dyn lash_core::facade_support::OrchestratingToolImplementation> {
    panic!("compile-only fixture")
}

fn main() {
    let _forged = lash_core::facade_support::OrchestratingToolDef::from_first_party(
        external_implementation(),
    );
}
