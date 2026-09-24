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

/// Process scope on the core's own effect host, pinned to the incarnation a
/// registry's first registration mints (registration sequence 1). Tests that
/// drive process entry points directly stand in for the worker's admission.
pub(super) fn process_scope(
    core: &LashCore,
    process_id: impl Into<lash_core::ProcessId>,
) -> lash_core::ScopedEffectController<'static> {
    host_scope(
        core,
        lash_core::AdmittedScope::process(lash_core::ProcessRef::new(
            process_id,
            lash_core::ProcessIncarnation::from_registration_sequence(1),
        )),
    )
}

/// Turn scope admitted on the core's own effect host — a turn's group
/// children resolve the opener registry and env store the host installed, so
/// a foreign (bare native) controller leaves them unroutable (ADR 0099 §2,§3).
pub(super) fn turn_scope(
    core: &LashCore,
    session_id: &SessionId,
) -> lash_core::ScopedEffectController<'static> {
    core.effect_host()
        .scoped_static(lash_core::AdmittedScope::turn(
            session_id,
            lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
                .0
                .to_string(),
        ))
        .expect("turn scope")
        .expect("effect host supplies an owned turn scope")
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

pub(super) async fn delete_bound_session(
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeleteReport> {
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
