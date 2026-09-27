use super::*;

/// A scope admitted for a test that stands in for a host operation: on the
/// held double `core` runs over, the scope of a workflow handler opened for
/// it, as a Restate deployment serves a host operation from its handler; on
/// the core's own effect host otherwise.
///
/// The lent handler stays open for the rest of the test: the returned
/// controller borrows it, so it is leaked rather than closed.
pub(super) async fn host_scope(
    core: &LashCore,
    admitted: lash_core::AdmittedScope,
) -> lash_core::ScopedEffectController<'static> {
    match super::harness::held_double(core) {
        Some(double) => {
            let handler: &'static lash_restate_test::OpenHandler = Box::leak(Box::new(
                double
                    .open_handler(admitted)
                    .await
                    .unwrap_or_else(|error| panic!("open the host operation's handler: {error}")),
            ));
            handler.scoped()
        }
        None => core
            .effect_host()
            .scoped_static(admitted)
            .expect("host execution scope")
            .expect("effect host supplies an owned scope"),
    }
}

pub(super) async fn runtime_operation_scope(
    core: &LashCore,
    scope_id: impl Into<String>,
) -> lash_core::ScopedEffectController<'static> {
    host_scope(core, lash_core::AdmittedScope::runtime_operation(scope_id)).await
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

/// Delete `session_id` and answer what the deletion did: inside a
/// `SessionDelete` handler on the held double `core` runs over, as a
/// deployment's delete workflow runs it, or on the core's own host when no
/// double serves it.
pub(super) async fn delete_bound_session_outcome(
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeletion> {
    match super::harness::held_double(core) {
        Some(double) => delete_session_on(&double, core, session_id).await,
        None => {
            let administration = core.session_administration().await;
            let context = administration.delete_context(session_id.as_ref())?;
            LashCore::delete_session(context).await
        }
    }
}

/// A deletion's handler execution: the core's administration over the
/// handler's own controller.
struct HandlerExecution<'a> {
    administration: lash_core::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl lash_core::SessionDeleteExecution for HandlerExecution<'_> {
    fn administration(&self) -> &lash_core::SessionAdministration {
        &self.administration
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

/// Run `attempt` over a delete context for `session_id` inside one
/// `SessionDelete` handler on `double`, which `core` runs over.
pub(super) async fn in_delete_handler<T>(
    double: &lash_restate_test::RestateTestBackend,
    core: &LashCore,
    session_id: impl AsRef<str>,
    attempt: impl AsyncFnOnce(lash_core::SessionDeleteContext<'_>) -> Result<T>,
) -> Result<T> {
    let session_id = session_id.as_ref();
    let handler = double
        .open_handler(lash_core::AdmittedScope::session_delete(
            lash_core::SessionId::from(session_id),
        ))
        .await
        .unwrap_or_else(|error| panic!("open the delete handler: {error}"));
    let outcome = {
        let execution = HandlerExecution {
            administration: core.session_administration().await,
            scoped: handler.scoped(),
        };
        match lash_core::SessionDeleteContext::from_execution(&execution, session_id) {
            Ok(context) => attempt(context).await,
            Err(error) => Err(error.into()),
        }
    };
    handler
        .close()
        .await
        .unwrap_or_else(|error| panic!("close the delete handler: {error}"));
    outcome
}

/// Delete `session_id` inside one `SessionDelete` handler on `double`.
pub(super) async fn delete_session_on(
    double: &lash_restate_test::RestateTestBackend,
    core: &LashCore,
    session_id: impl AsRef<str>,
) -> Result<crate::SessionDeletion> {
    in_delete_handler(double, core, session_id, async |context| {
        LashCore::delete_session(context).await
    })
    .await
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
