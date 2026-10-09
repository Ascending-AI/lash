use super::*;

#[test]
fn direct_process_handle_await_error_keeps_typed_provenance() {
    let source = r#"
        const worker = async () => { return null; };
        const handle = await processes.start({ definition: worker });
        finish(await handle);
    "#;
    let linked = lash_typescript::link(source, &process_environment()).expect("link await");
    let error = futures::executor::block_on(lash_vm::execute(
        &lash_vm::testing::harness::compile_linked_main(&linked),
        &mut State::new(),
        &ProcessAwaitFailureHost::Typed,
    ))
    .expect_err("uncaught process-await failure");
    let lash_vm::RuntimeError::UnwrappedHostToolResultFailed { source } = error else {
        panic!("process await must preserve its typed host failure");
    };
    let failure = source.tool_failure();
    assert!(matches!(
        failure,
        Some(effect)
            if effect.class == lash_sansio::ToolFailureClass::PermissionDenied
                && effect.code == "approval_denied"
                && effect.source == lash_sansio::ToolFailureSource::Policy
                && effect.replay_key == "await-effect-key"
    ));
}
