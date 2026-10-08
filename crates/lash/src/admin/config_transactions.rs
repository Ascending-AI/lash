//! The facade's config transactions (FIG-4379): a session's config changes
//! only through an ordered transaction of typed config commands, each owned by
//! the core owner or an installed plugin, written against the config revision
//! the caller read.
//!
//! On a store-backed session the transaction is a session command: the
//! writer is held only to submit it, and the shift applies it once no run
//! owns the head, so a running or parked run finishes under the config it
//! was admitted with and the next run executes under the transaction's. A
//! storeless session resolves and publishes it directly under the writer.

use super::*;
use host_commands::SubmittedCommand;

/// What a config transaction is written against: the caller's stable id for
/// it, which a resubmission reuses, and the config revision the caller read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigWrite {
    pub id: String,
    pub expected_revision: u64,
}

impl ConfigWrite {
    pub fn new(id: impl Into<String>, expected_revision: u64) -> Self {
        Self {
            id: id.into(),
            expected_revision,
        }
    }
}

/// How a submitted config transaction stands.
#[derive(Clone, Debug, PartialEq)]
pub enum ConfigSettlement {
    /// The transaction settled: applied with one revision step, stale, or
    /// refused by an owner.
    Settled(lash_core::ConfigTransactionOutcome),
    /// The transaction is durable and waits for the shift; its receipt
    /// settles it later, on any runtime.
    Pending(lash_core::runtime::SessionCommandReceipt),
    /// The transaction was withdrawn before a shift admitted it.
    Cancelled(lash_core::runtime::SessionCommandReceipt),
}

impl SessionConfigAdmin {
    /// The session's config revision: what a transaction written now is
    /// written against. It is read from the durable head's metadata, which
    /// is what the shift judges the transaction against, so it never waits
    /// behind a run that holds the session's runtime.
    pub async fn revision(&self) -> Result<u64> {
        let context = self.control.target.context().await?;
        let head = context
            .parts
            .store
            .load_session_head_meta()
            .await
            .map_err(EmbedError::Store)?
            .ok_or_else(|| EmbedError::UnknownSession {
                session_id: context.parts.session_id.clone(),
            })?;
        Ok(head.config.config_revision)
    }

    /// Every config command this session admits, generated from the
    /// installed registrations, with the config revision the catalog
    /// describes. Discovery only: an owner can still refuse arguments its
    /// schema admits.
    pub async fn commands(&self) -> Result<lash_core::ConfigCommandCatalog> {
        self.control
            .with_writer(async |runtime: &mut LashRuntime| {
                runtime.reload_invalidated_resident_session_state().await?;
                runtime
                    .config_command_catalog()
                    .map_err(EmbedError::Runtime)
            })
            .await
    }

    /// Submit `transaction` under `write` and wait for it to settle.
    ///
    /// A transaction the shift has not settled by the wait's deadline is
    /// refused [`SessionError::SessionCommandPending`] with its receipt; it
    /// stays durable, and [`Self::settle`] reads it later.
    pub async fn apply(
        &self,
        write: ConfigWrite,
        transaction: lash_core::ConfigTransaction,
    ) -> Result<lash_core::ConfigTransactionOutcome> {
        match Box::pin(self.submit(write, transaction)).await? {
            ConfigSettlement::Settled(outcome) => Ok(outcome),
            ConfigSettlement::Pending(receipt) => match Box::pin(self.settle(receipt)).await? {
                ConfigSettlement::Settled(outcome) => Ok(outcome),
                ConfigSettlement::Pending(receipt) => Err(EmbedError::Session(
                    SessionError::SessionCommandPending(receipt),
                )),
                ConfigSettlement::Cancelled(receipt) => Err(EmbedError::Session(
                    SessionError::SessionCommandCancelled(receipt),
                )),
            },
            ConfigSettlement::Cancelled(receipt) => Err(EmbedError::Session(
                SessionError::SessionCommandCancelled(receipt),
            )),
        }
    }

    /// Submit `transaction` under `write`, and return once it is durable:
    /// [`ConfigSettlement::Pending`] with its receipt on a store-backed
    /// session, [`ConfigSettlement::Settled`] on a storeless one, which
    /// applies it at once.
    ///
    /// A transaction that names an owner or command no installed plugin
    /// registers, or arguments that do not decode, is refused
    /// [`EmbedError::ConfigSubmit`] and nothing is enqueued; so is a
    /// resubmission under `write.id` whose content differs from the first.
    pub async fn submit(
        &self,
        write: ConfigWrite,
        transaction: lash_core::ConfigTransaction,
    ) -> Result<ConfigSettlement> {
        let submitted = self
            .control
            .with_writer(async |runtime: &mut LashRuntime| {
                if runtime.is_store_backed() {
                    return Box::pin(runtime.submit_config_transaction(
                        write.id,
                        write.expected_revision,
                        &transaction,
                    ))
                    .await
                    .map(SubmittedCommand::Queued)
                    .map_err(EmbedError::from);
                }
                Box::pin(runtime.apply_storeless_config_transaction(
                    write.id,
                    write.expected_revision,
                    &transaction,
                ))
                .await
                .map(SubmittedCommand::Applied)
                .map_err(EmbedError::from)
            })
            .await?;
        Ok(match submitted {
            SubmittedCommand::Applied(outcome) => ConfigSettlement::Settled(outcome),
            SubmittedCommand::Queued(receipt) => ConfigSettlement::Pending(receipt),
        })
    }

    /// Wait for the transaction `receipt` names to settle, and read how it
    /// settled.
    pub async fn settle(
        &self,
        receipt: lash_core::runtime::SessionCommandReceipt,
    ) -> Result<ConfigSettlement> {
        match Box::pin(self.control.await_command_settlement(receipt)).await? {
            lash_core::runtime::SessionCommandSettlement::Applied {
                outcome: lash_core::runtime::SessionCommandOutcome::ConfigTransaction { outcome },
                ..
            } => Ok(ConfigSettlement::Settled(outcome)),
            lash_core::runtime::SessionCommandSettlement::Pending(receipt) => {
                Ok(ConfigSettlement::Pending(receipt))
            }
            lash_core::runtime::SessionCommandSettlement::Cancelled(receipt) => {
                Ok(ConfigSettlement::Cancelled(receipt))
            }
            settlement => Err(host_commands::unsettled_command_error(settlement)),
        }
    }
}

impl From<lash_core::runtime::ConfigTransactionSubmitError> for EmbedError {
    fn from(error: lash_core::runtime::ConfigTransactionSubmitError) -> Self {
        match error {
            lash_core::runtime::ConfigTransactionSubmitError::Refused(refusal) => {
                Self::ConfigSubmit(refusal)
            }
            lash_core::runtime::ConfigTransactionSubmitError::Runtime(error) => {
                Self::Runtime(error)
            }
        }
    }
}
