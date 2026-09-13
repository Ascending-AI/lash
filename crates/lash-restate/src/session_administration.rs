use std::sync::Arc;

use lash_core::{
    ExecutionScope, RuntimeError, ScopedEffectController, SessionAdministration,
    SessionDeleteContext, SessionDeleteExecution,
};

use crate::{
    RestateAuthorityId, RestateConnection, RestateControllerContext, RestateEffectHost,
    RestateRuntimeEffectController,
};

/// Session administration installed for one Restate deployment.
///
/// Construction replaces the generic deployment effect host with one built
/// from `connection`, while retaining the catalog, process, and trigger
/// services selected together by the facade. The host must route invocation
/// contexts passed to [`Self::for_invocation`] to this same deployment; the
/// Restate SDK does not expose an identity that Rust can verify here.
#[derive(Clone)]
pub struct RestateSessionAdministration {
    administration: SessionAdministration,
    authority_id: RestateAuthorityId,
}

impl RestateSessionAdministration {
    pub fn new(
        administration: SessionAdministration,
        connection: impl Into<RestateConnection>,
        authority_id: RestateAuthorityId,
    ) -> Self {
        let connection = connection.into();
        let effect_host = Arc::new(RestateEffectHost::new(connection, authority_id.clone()));
        Self {
            administration: administration.with_effect_host(effect_host),
            authority_id,
        }
    }

    /// Bind this deployment's administration services to one handler context.
    pub fn for_invocation<'a, 'ctx, C>(
        &'a self,
        context: C,
    ) -> RestateSessionDeleteExecution<'a, 'ctx, C>
    where
        C: RestateControllerContext<'ctx>,
    {
        RestateSessionDeleteExecution {
            administration: &self.administration,
            controller: RestateRuntimeEffectController::new(context, self.authority_id.clone()),
        }
    }
}

/// Invocation-borrowed issuer for a single Restate session deletion.
pub struct RestateSessionDeleteExecution<'a, 'ctx, C> {
    administration: &'a SessionAdministration,
    controller: RestateRuntimeEffectController<'ctx, C>,
}

impl<'a, 'ctx, C> RestateSessionDeleteExecution<'a, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    #[doc(hidden)]
    pub fn controller(&self) -> &RestateRuntimeEffectController<'ctx, C> {
        &self.controller
    }

    pub fn delete_context<'run>(
        &'run self,
        session_id: &str,
    ) -> Result<SessionDeleteContext<'run>, RuntimeError> {
        SessionDeleteContext::from_execution(self, session_id)
    }
}

impl<'a, 'ctx, C> SessionDeleteExecution for RestateSessionDeleteExecution<'a, 'ctx, C>
where
    C: RestateControllerContext<'ctx>,
{
    fn administration(&self) -> &SessionAdministration {
        self.administration
    }

    fn scoped<'run>(
        &'run self,
        scope: ExecutionScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.controller.scoped_effect_controller(scope)
    }
}
