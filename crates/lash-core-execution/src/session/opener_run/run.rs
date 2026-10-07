//! K6 ownership shared by every phase of one turn. A Run never crosses a
//! phase boundary.

use super::*;
use crate::tool_dispatch::{SingletonRunError, ToolRun};

impl OpenerState {
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
