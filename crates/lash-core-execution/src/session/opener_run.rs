//! Logical Run ownership and per-cell admission shared across execution phases.

pub(crate) mod run;

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::execution_context::RuntimeExecutionContext;
use crate::runtime::effect::executor::RuntimeEffectControllerError;

#[derive(Debug, Default)]
pub struct OpenerRunRegistry {
    active_run: bool,
}

/// What an opener shares across the phase contexts it builds: the once-only
/// incorporation ledger and whether its tool Run is open.
///
/// Cloning shares them; [`OpenerState::default`] starts a fresh opener.
#[derive(Clone, Debug, Default)]
pub struct OpenerState {
    pub(crate) ledger: Arc<std::sync::Mutex<super::IncorporationLedger>>,
    pub(crate) run_state: Arc<std::sync::Mutex<OpenerRunRegistry>>,
}

impl OpenerState {
    /// What the opener has incorporated so far.
    #[must_use]
    pub fn ledger_snapshot(&self) -> super::IncorporationLedger {
        self.ledger.lock_recover().clone()
    }

    /// Adopt a ledger a replayed boundary carried: the incorporations a
    /// journal-served phase made before its worker died (ADR 0099 §6).
    pub fn absorb_ledger(&self, ledger: super::IncorporationLedger) {
        self.ledger.lock_recover().absorb(ledger);
    }
}

impl<'run> RuntimeExecutionContext<'run> {
    /// The prefix every group key this opener forms carries: `{scope}:group:`.
    pub(crate) fn own_group_key_prefix(&self) -> String {
        format!("{}:group:", self.execution_scope_id())
    }

    /// The key of the group a language command forms (FIG-3586): the opener's
    /// group prefix, then its positional key. The prefix is a frozen logical
    /// identity of aggregate admission.
    pub(crate) fn command_group_key(&self, command: &crate::CommandReplayKey) -> String {
        format!("{}{command}", self.own_group_key_prefix())
    }

    /// The latest `max_tool_calls` refusal this execution met, typed. A
    /// language runtime whose run failed on the refusal reads it here to
    /// report the failure with its typed cause.
    #[must_use]
    pub fn tool_call_limit_refusal(&self) -> Option<crate::ToolCallLimitExceeded> {
        *self.tool_call_limit_refusal.lock_recover()
    }

    /// Close the logical Run, drain its accepted finals and incorporate them
    /// into this opener's ledger before the owner finishes.
    pub async fn close_tool_run(&self) -> Result<(), RuntimeEffectControllerError> {
        if let Some(run) = &self.tool_run {
            run.close().await?;
        }
        Ok(())
    }
}
