//! Logical Run ownership and per-cell admission shared across execution phases.

pub(crate) mod run;
use super::runtime_ops::RuntimeExecutionContextRuntimeOps as _;

use std::sync::Arc;

use lash_sansio::sync::MutexExt;

use super::execution_context::RuntimeExecutionContext;
use crate::runtime::effect::executor::RuntimeEffectControllerError;

#[derive(Debug, Default)]
pub struct OpenerRunRegistry {
    run: Option<Box<crate::tool_run::RunTransfer>>,
    active_run: bool,
}

/// What an opener shares across the phase contexts it builds: the once-only
/// incorporation ledger and retained tool Run.
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

    /// Capture the logical Run's state without closing or consuming a group.
    #[must_use]
    pub fn snapshot(&self) -> crate::store::RunOpenerState {
        let registry = self.run_state.lock_recover();
        self.snapshot_with_registry(&registry)
    }

    fn snapshot_with_registry(&self, registry: &OpenerRunRegistry) -> crate::store::RunOpenerState {
        crate::store::RunOpenerState {
            run: registry.run.clone(),
            incorporation: self.ledger_snapshot(),
        }
    }

    /// Reattach the continuation's Run before the successor executes.
    pub fn from_snapshot(
        snapshot: crate::store::RunOpenerState,
    ) -> Result<Self, RuntimeEffectControllerError> {
        let registry = OpenerRunRegistry {
            run: snapshot.run,
            ..Default::default()
        };
        Ok(Self {
            ledger: Arc::new(std::sync::Mutex::new(snapshot.incorporation)),
            run_state: Arc::new(std::sync::Mutex::new(registry)),
        })
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
        } else if self.opener_state().holds_tool_run() {
            self.drive_tool_run(None, |context| async move {
                context
                    .tool_run
                    .as_ref()
                    .ok_or_else(|| {
                        RuntimeEffectControllerError::from(
                            crate::tool_run::ContinuationRefusal::NotQuiescent,
                        )
                    })?
                    .close()
                    .await
            })
            .await??;
        }
        Ok(())
    }
}

impl RuntimeExecutionContext<'_> {
    /// Keep the Run's acknowledged cut beside the opener's phase state.
    /// The caller quiesces and retains the coordinator before handing it here.
    /// No issued handle or future is stored in this registry.
    ///
    /// # Errors
    /// An owner, admitted environment, segment or capture refusal.
    pub fn retain_run_continuation(
        &self,
        transfer: crate::tool_run::RunTransfer,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let owner = self
            .process_id()
            .map(|id| crate::EffectOpener::process(id.clone()))
            .ok_or(crate::tool_run::ContinuationRefusal::ForeignOwner)?;
        if transfer.owner != owner {
            return Err(crate::tool_run::ContinuationRefusal::ForeignOwner);
        }
        self.validate_process_run(&transfer, false)?;
        transfer.check_capture(crate::tool_run::CutPhase::Capturable)?;
        self.opener_groups.lock_recover().run = Some(Box::new(transfer));
        Ok(())
    }

    /// Take the carried receipts to rebuild the successor's coordinator.
    pub fn take_run_continuation(&self) -> Option<crate::tool_run::RunTransfer> {
        self.opener_groups
            .lock_recover()
            .run
            .take()
            .map(|transfer| *transfer)
    }

    /// Capture a quiesced Run without closing the process's logical opener.
    ///
    /// # Errors
    /// A stale, unretained or unacknowledged continuation is refused.
    pub fn run_continuation_snapshot(
        &self,
    ) -> Result<Option<crate::tool_run::RunTransfer>, crate::tool_run::ContinuationRefusal> {
        let transfer = self.opener_groups.lock_recover().run.as_deref().cloned();
        if let Some(transfer) = &transfer {
            self.validate_process_run(transfer, false)?;
            transfer.check_capture(crate::tool_run::CutPhase::Capturable)?;
        }
        Ok(transfer)
    }

    /// Restore only the Run of this process and its admitted successor.
    /// The coordinator subsequently reads material through successor leases.
    ///
    /// # Errors
    /// An owner, admitted environment, segment or capture refusal.
    pub fn restore_run_continuation(
        &self,
        transfer: crate::tool_run::RunTransfer,
        successor: crate::tool_run::SegmentOrdinal,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let owner = self
            .process_id()
            .map(|id| crate::EffectOpener::process(id.clone()))
            .ok_or(crate::tool_run::ContinuationRefusal::ForeignOwner)?;
        let admitted = self
            .process_event_context()
            .and_then(|context| context.execution_write_authority.segment())
            .ok_or(crate::tool_run::ContinuationRefusal::MissingSegmentAuthority)?;
        if admitted != successor {
            return Err(crate::tool_run::ContinuationRefusal::SegmentFrontier);
        }
        self.validate_process_run(&transfer, true)?;
        transfer.check_capture(crate::tool_run::CutPhase::Capturable)?;
        // Keep the predecessor reference in the codec for the coordinator's
        // adoption. The registry validates authority without advancing twice.
        transfer
            .clone()
            .adopt(&owner, transfer.ledger()?.lifecycle(), successor)?;
        self.opener_groups.lock_recover().run = Some(Box::new(transfer));
        Ok(())
    }
    fn validate_process_run(
        &self,
        transfer: &crate::tool_run::RunTransfer,
        adopting: bool,
    ) -> Result<(), crate::tool_run::ContinuationRefusal> {
        let authority = self
            .process_event_context()
            .and_then(|context| context.execution_write_authority.segment())
            .ok_or(crate::tool_run::ContinuationRefusal::MissingSegmentAuthority)?;
        if !adopting && transfer.from != authority {
            return Err(crate::tool_run::ContinuationRefusal::SegmentFrontier);
        }
        if transfer.environment.as_ref() != self.inherited_process_execution_env_ref().as_ref() {
            return Err(crate::tool_run::ContinuationRefusal::ForeignEnvironment);
        }
        Ok(())
    }
}
