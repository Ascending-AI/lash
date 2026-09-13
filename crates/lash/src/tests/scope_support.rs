use super::*;

pub(super) fn native_scope(
    scope: lash_core::ExecutionScope,
) -> lash_core::ScopedEffectController<'static> {
    lash_core::ScopedEffectController::shared(
        Arc::new(lash_core::facade_support::NativeRuntimeEffectController::default()),
        scope,
    )
    .expect("native execution scope")
}

pub(super) fn turn_scope(session_id: &SessionId) -> lash_core::ScopedEffectController<'static> {
    native_scope(lash_core::ExecutionScope::turn(
        session_id,
        lash_core::TurnActivityId::new(uuid::Uuid::new_v4().to_string())
            .0
            .to_string(),
    ))
}

pub(super) fn runtime_operation_scope(
    core: &LashCore,
    scope_id: impl Into<String>,
) -> lash_core::ScopedEffectController<'static> {
    core.effect_host()
        .scoped_static(lash_core::ExecutionScope::runtime_operation(scope_id))
        .expect("runtime operation scope")
        .expect("effect host supplies an owned runtime operation scope")
}

pub(super) async fn delete_bound_session(
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeleteReport> {
    let administration = core.session_administration().await?;
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
