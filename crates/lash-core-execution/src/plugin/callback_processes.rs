use std::sync::Arc;

use lash_execution_contracts::plugin_error::PluginError;
use lash_execution_contracts::process::{
    ProcessCancelReceipt, ProcessHandleView, ProcessStartRequest,
};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::{ProcessId, RuntimeOwner};

/// Process operations bound to the callback that received this capability.
#[derive(Clone)]
pub struct CallbackProcesses {
    pub(super) session: Option<SessionId>,
    pub(super) service: Arc<dyn crate::ProcessService>,
    pub(super) scope: crate::ProcessOpScope<'static>,
}

impl CallbackProcesses {
    fn session(&self) -> Result<&SessionId, PluginError> {
        self.session.as_ref().ok_or_else(|| {
            PluginError::Session("this callback has no session process authority".into())
        })
    }

    /// The admitted lifetime choices available to a child of this callback.
    pub fn start_cx(
        &self,
    ) -> Result<
        Option<lash_execution_contracts::process::StartCx>,
        lash_execution_contracts::process::StartCxError,
    > {
        self.scope.start_cx()
    }

    pub async fn start(
        &self,
        request: ProcessStartRequest,
    ) -> Result<ProcessHandleView, PluginError> {
        let request = self.scope.effect_controller.key_host_start(
            lash_execution_contracts::process::model::key_start_request(request, None),
        )?;
        self.service
            .start_from_request(self.session()?, request, self.scope.clone())
            .await
    }

    pub async fn cancel(&self, process: &ProcessId) -> Result<ProcessCancelReceipt, PluginError> {
        let owner = RuntimeOwner::Session(self.session()?.clone());
        self.service
            .validate_visible(&owner, std::slice::from_ref(process), self.scope.clone())
            .await?;
        ProcessCancelReceipt::from_observed(
            self.service
                .cancel(&owner, process, self.scope.clone())
                .await?,
        )
    }

    pub async fn cancel_all(&self) -> Result<Vec<ProcessCancelReceipt>, PluginError> {
        self.service
            .cancel_all_visible(self.session()?, self.scope.clone())
            .await
    }
}
