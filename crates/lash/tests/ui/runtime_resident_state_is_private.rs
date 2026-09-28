fn cannot_write_resident_state(
    runtime: &mut lash::runtime::LashRuntime,
    state: lash::persistence::RuntimeSessionState,
) {
    runtime.state = state;
    runtime.state.authority = Default::default();
}

fn main() {}
