//! A segment pins its process scope until its runner can issue no more effects.

use lash_core::{ExecutionScope, PluginError, ProcessId};
use restate_sdk::context::WorkflowContext;
use restate_sdk::errors::{HandlerResult, TerminalError};

use crate::durable_wait::{
    RestateDurableWaitProcessJournalRequest, durable_wait_index_key_for_scope,
};

pub(super) struct ProcessJournalPin {
    index_key: String,
    request: RestateDurableWaitProcessJournalRequest,
}

impl ProcessJournalPin {
    pub(super) async fn register(
        ctx: &WorkflowContext<'_>,
        namespace: &crate::RestateNamespace,
        process_id: &ProcessId,
    ) -> HandlerResult<Self> {
        let scope = ExecutionScope::process(process_id);
        let pin = Self {
            index_key: durable_wait_index_key_for_scope(&scope),
            request: RestateDurableWaitProcessJournalRequest {
                process_id: process_id.clone(),
                invocation_id: crate::RestateInvocationId::new(ctx.invocation_id().to_string()),
            },
        };
        if !namespace
            .durable_wait_registry(ctx, pin.index_key.clone())
            .register_process_journal(pin.request.clone())
            .call()
            .await?
            .into_body()
        {
            let identity = scope
                .journal_identity()
                .map_err(TerminalError::from_error)?;
            return Err(crate::process::handler_error_from_plugin(
                PluginError::RuntimeEffectController(
                    lash_core::facade_support::scope_status::scope_retired(identity.key()),
                ),
            ));
        }
        Ok(pin)
    }

    /// The accepted successor gets the same fixed invocation pin before this
    /// segment releases. Its own registration is idempotent under this id.
    pub(super) async fn transfer_to(
        self,
        ctx: &WorkflowContext<'_>,
        namespace: &crate::RestateNamespace,
        invocation_id: &str,
    ) -> HandlerResult<()> {
        let request = RestateDurableWaitProcessJournalRequest {
            process_id: self.request.process_id.clone(),
            invocation_id: crate::RestateInvocationId::new(invocation_id.to_owned()),
        };
        if !namespace
            .durable_wait_registry(ctx, self.index_key.clone())
            .register_process_journal(request)
            .call()
            .await?
            .into_body()
        {
            return Err(
                TerminalError::new("the accepted process successor's scope is retired").into(),
            );
        }
        self.release(ctx, namespace).await
    }

    pub(super) async fn release(
        self,
        ctx: &WorkflowContext<'_>,
        namespace: &crate::RestateNamespace,
    ) -> HandlerResult<()> {
        namespace
            .durable_wait_registry(ctx, self.index_key)
            .release_process_journal(self.request)
            .call()
            .await?;
        Ok(())
    }
}
