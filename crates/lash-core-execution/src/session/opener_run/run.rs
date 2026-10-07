//! K6 ownership shared by every phase of one turn. A Run never crosses a
//! phase boundary.

use super::*;
use crate::tool_dispatch::{SingletonRunError, ToolRun};

impl OpenerState {
    /// The opener state a physical boundary carries.
    ///
    /// # Errors
    ///
    /// `InvocationFailed` while a tool Run is open: a boundary never cuts
    /// one.
    pub fn boundary_snapshot(
        &self,
    ) -> Result<crate::store::RunOpenerState, RuntimeEffectControllerError> {
        let registry = self.run_state.lock_recover();
        if registry.active_run {
            return Err(
                SingletonRunError::from(crate::tool_run::RunCutRefusal::InvocationFailed)
                    .into_controller_error(),
            );
        }
        Ok(self.snapshot_with_registry(&registry))
    }

    /// The opener a continuation of `_owner` carried.
    pub fn from_snapshot_for(
        snapshot: crate::store::RunOpenerState,
        _owner: &crate::EffectOpener,
    ) -> Result<Self, RuntimeEffectControllerError> {
        Self::from_snapshot(snapshot)
    }

    /// Open the opener's logical Run before admitting work.
    ///
    /// # Errors
    ///
    /// `InvocationFailed` while another frame holds the Run open.
    pub fn open_run<'a>(
        &self,
        scope: crate::ExecutionScope,
        clock: Arc<dyn crate::Clock>,
    ) -> Result<ToolRun<'a>, SingletonRunError> {
        let mut registry = self.run_state.lock_recover();
        if registry.active_run {
            return Err(crate::tool_run::RunCutRefusal::InvocationFailed.into());
        }
        registry.active_run = true;
        Ok(ToolRun::new(scope, clock))
    }

    pub fn holds_tool_run(&self) -> bool {
        self.run_state.lock_recover().active_run
    }

    pub(crate) fn finish_run(&self) {
        self.run_state.lock_recover().active_run = false;
    }
}
