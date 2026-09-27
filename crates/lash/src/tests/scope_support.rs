use super::*;

/// A scope admitted on the core's own effect host.
pub(super) fn host_scope(
    core: &LashCore,
    admitted: lash_core::AdmittedScope,
) -> lash_core::ScopedEffectController<'static> {
    core.effect_host()
        .scoped_static(admitted)
        .expect("host execution scope")
        .expect("effect host supplies an owned scope")
}

/// Process scope on the core's own effect host, for the process a registrar
/// minted `process_id` for. Tests that drive process entry points directly
/// stand in for the worker's admission.
pub(super) fn process_scope(
    core: &LashCore,
    process_id: &lash_core::ProcessId,
) -> lash_core::ScopedEffectController<'static> {
    host_scope(core, lash_core::AdmittedScope::process(process_id.clone()))
}

pub(super) fn runtime_operation_scope(
    core: &LashCore,
    scope_id: impl Into<String>,
) -> lash_core::ScopedEffectController<'static> {
    core.effect_host()
        .scoped_static(lash_core::AdmittedScope::runtime_operation(scope_id))
        .expect("runtime operation scope")
        .expect("effect host supplies an owned runtime operation scope")
}

/// Delete `session_id` and require the physical delete to have run in the
/// call: nothing its close left behind is undelivered.
pub(super) async fn delete_bound_session(
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeleteReport> {
    match delete_bound_session_outcome(core, session_id).await? {
        crate::SessionDeletion::Deleted(report) => Ok(report),
        other => panic!("the delete must run in the call, got {other:?}"),
    }
}

/// Delete `session_id` and answer what the deletion did.
pub(super) async fn delete_bound_session_outcome(
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeletion> {
    let administration = core.session_administration().await;
    let context = administration.delete_context(session_id.as_ref())?;
    LashCore::delete_session(context).await
}

pub(super) fn text_message(role: lash_core::MessageRole, text: &str) -> lash_core::Message {
    let id = "stored-message".to_string();
    lash_core::Message {
        id: id.clone(),
        role,
        parts: lash_core::facade_support::shared_parts(vec![lash_core::Part::text(
            format!("{id}.p0"),
            text.to_string(),
            None,
        )]),
        origin: None,
    }
}
