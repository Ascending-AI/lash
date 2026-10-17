#[test]
fn process_engine_registration_rejects_a_kind_mismatch() {
    assert!(matches!(
        lash_core::ProcessEngineRegistration::new(
            Arc::new(PayloadGatedEngine),
            lash_execution_contracts::process::ProcessEngineAdmission::accepting("different-kind"),
        ),
        Err(lash_execution_contracts::plugin_error::PluginError::Registration(_))
    ));
}
