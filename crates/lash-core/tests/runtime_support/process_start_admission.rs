#[test]
fn process_engine_registration_rejects_a_kind_mismatch() {
    assert!(matches!(
        lash_core::ProcessEngineRegistration::new(
            Arc::new(PayloadGatedEngine),
            lash_core::ProcessEngineAdmission::accepting("different-kind"),
        ),
        Err(lash_core::PluginError::Registration(_))
    ));
}
