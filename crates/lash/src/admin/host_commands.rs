use super::AdminContext;
use crate::support::{EmbedError, LashRuntime, Result, SessionError};
use lash_sansio::SessionId;

#[derive(Clone)]
/// Facade handle for session command administration.
pub struct SessionCommandAdmin {
    pub(super) context: AdminContext,
}

impl SessionCommandAdmin {
    /// Submit `command` to the session's command lane under a stable
    /// `idempotency_key` and return its durable receipt, before it applies
    /// (FIG-4202). The session's shift applies it at a turn boundary; a
    /// resubmission under the same key names the same command. Await its
    /// outcome with [`Self::settle`], from this handle or any other that holds
    /// the receipt.
    pub async fn submit(
        &self,
        command: lash_core::facade_support::SessionCommand,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::facade_support::SessionCommandReceipt> {
        let idempotency_key = idempotency_key.into();
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                Box::pin(runtime.submit_session_command(command, idempotency_key))
                    .await
                    .map_err(Into::into)
            })
            .await
    }

    /// Await the settlement of the command `receipt` names, reattaching by
    /// its receipt (FIG-4202): applied with its typed outcome, a refusal
    /// among them; or cancelled when it was withdrawn. Dropping this
    /// await withdraws nothing: the command stays durable and settles.
    /// A settled command adopts and publishes the current durable head into
    /// this open session, including changes committed by later writers.
    pub async fn settle(
        &self,
        receipt: lash_core::facade_support::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        let pacing = self.context.target.observer_pacing().admin;
        let mut pause = pacing.initial();
        loop {
            let settlement = self.status(receipt.clone()).await?;
            let pending = matches!(
                settlement,
                lash_core::runtime::SessionCommandSettlement::Pending(_)
            );
            if !pending {
                return Ok(settlement);
            }
            tokio::time::sleep(pause).await;
            pause = pacing.next(pause);
        }
    }

    /// Withdraw the command `receipt` names (FIG-4202). A command no shift
    /// has admitted is withdrawn transactionally and never applies; one a
    /// shift already read, or that already settled, answers
    /// [`SessionCommandWithdrawal::AlreadyAdmitted`] and settles as that
    /// shift applies it.
    pub async fn withdraw(
        &self,
        receipt: &lash_core::facade_support::SessionCommandReceipt,
    ) -> Result<SessionCommandWithdrawal> {
        self.require_own_command(receipt)?;
        let withdrawn = self
            .context
            .runtime
            .cancel_queued_work_batch(receipt.batch_id.as_str())
            .await
            .map_err(EmbedError::Runtime)?;
        Ok(match withdrawn {
            Some(_) => SessionCommandWithdrawal::Withdrawn,
            None => SessionCommandWithdrawal::AlreadyAdmitted,
        })
    }

    /// The command drains asynchronously and recomputes the surface from live sources, so it
    /// takes no generation guard — any generation observed at enqueue time could legitimately
    /// have advanced by drain time.
    pub async fn refresh_tool_catalog(
        &self,
        reason: impl Into<String>,
        idempotency_key: impl Into<String>,
    ) -> Result<lash_core::facade_support::SessionCommandReceipt> {
        self.submit(
            lash_core::facade_support::SessionCommand::RefreshToolCatalog {
                reason: reason.into(),
            },
            idempotency_key,
        )
        .await
    }

    pub(super) async fn status(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
    ) -> Result<lash_core::runtime::SessionCommandSettlement> {
        self.context
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime
                    .settle_session_command(receipt)
                    .await
                    .map_err(EmbedError::from)
            })
            .await
    }

    /// Refuse a host operation on a command of another session than this
    /// one.
    fn require_own_command(
        &self,
        receipt: &lash_core::runtime::SessionCommandReceipt,
    ) -> Result<()> {
        let session_id = SessionId::from(self.context.runtime.observe().session_id());
        if session_id != receipt.session_id {
            return Err(EmbedError::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::StoreCommitFailed,
                lash_core::StoreError::ForeignSessionRequest {
                    view_session_id: session_id,
                    request_session_id: receipt.session_id.clone(),
                }
                .to_string(),
            )));
        }
        Ok(())
    }
}

/// What withdrawing a submitted session command did (FIG-4202).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionCommandWithdrawal {
    /// No shift had admitted the command: it is withdrawn, transactionally,
    /// and never applies.
    Withdrawn,
    /// A shift already read the command, or it already settled: the
    /// withdrawal lost the race, and the command settles as that execute
    /// applies it.
    AlreadyAdmitted,
}

/// A host head write as its submission left it (FIG-4201, FIG-4202):
/// queued on a store-backed session's command lane, or applied directly on
/// a storeless one, with what it applied as.
pub(super) enum SubmittedCommand<T> {
    Queued(lash_core::runtime::SessionCommandReceipt),
    Applied(T),
}

/// The error a command's caller answers when its settlement is not one its
/// command applies with: withdrawn, rejected before
/// acceptance, or another command's settlement shape.
pub(super) fn unsettled_command_error(
    settlement: lash_core::runtime::SessionCommandSettlement,
) -> EmbedError {
    match settlement {
        lash_core::runtime::SessionCommandSettlement::Cancelled(receipt) => {
            EmbedError::Session(SessionError::SessionCommandCancelled(receipt))
        }
        lash_core::runtime::SessionCommandSettlement::Rejected(error) => EmbedError::Runtime(error),
        // A command that could not apply settles with its typed failure.
        lash_core::runtime::SessionCommandSettlement::Applied {
            outcome: lash_core::runtime::SessionCommandOutcome::Failed { refusal },
            ..
        } => EmbedError::Runtime(refusal.into()),
        settlement => EmbedError::Session(SessionError::Protocol(format!(
            "a session command settled with another command's settlement: {settlement:?}"
        ))),
    }
}
