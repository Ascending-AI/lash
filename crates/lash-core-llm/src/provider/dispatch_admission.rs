//! The gate every provider attempt passes before it is dispatched (ADR 0125).
//!
//! A provider attempt may be billed the moment it leaves the process, so the
//! obligation to account for it has to exist before it does. The gate is
//! where that obligation is taken: the durable kernel's usage run admits
//! itself to storage on its first attempt, and a refusal dispatches nothing.

use super::support::*;

/// One provider attempt about to be dispatched.
#[derive(Clone, Copy, Debug)]
pub struct ProviderDispatch<'a> {
    pub call_id: &'a LlmCallId,
    /// `AttemptRecord::ordinal` the attempt will be sealed under (1-based).
    pub attempt_ordinal: u32,
    pub model: &'a str,
}

/// Why the gate refused an attempt. Nothing was dispatched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchRefused {
    pub code: TurnFailureCode,
    pub message: String,
    pub retryable: bool,
}

/// The gate a provider attempt passes before it is dispatched.
#[async_trait]
pub trait DispatchAdmission: Send + Sync {
    /// Called before every provider attempt. A refusal dispatches nothing.
    async fn admit_dispatch(&self, dispatch: &ProviderDispatch<'_>) -> Result<(), DispatchRefused>;
}

/// The admission of a call no lash execution owns: the host that made it owns
/// its billing, and lash keeps no ledger row for it.
struct HostOwnedDispatch;

#[async_trait]
impl DispatchAdmission for HostOwnedDispatch {
    async fn admit_dispatch(
        &self,
        _dispatch: &ProviderDispatch<'_>,
    ) -> Result<(), DispatchRefused> {
        Ok(())
    }
}

static HOST_OWNED_DISPATCH: HostOwnedDispatch = HostOwnedDispatch;

impl dyn DispatchAdmission {
    /// The admission of a call made outside any lash execution — the
    /// standalone host client, and tests that exercise the transport alone.
    /// The host owns that call's billing.
    pub fn host_owned() -> &'static dyn DispatchAdmission {
        &HOST_OWNED_DISPATCH
    }
}

impl DispatchRefused {
    pub(super) fn into_transport_error(self) -> LlmTransportError {
        LlmTransportError::new(self.message)
            .with_kind(ProviderFailureKind::Validation)
            .with_lash_code(self.code)
            .with_retry_verdict(if self.retryable {
                TransportRetryVerdict::NotRetryable
            } else {
                TransportRetryVerdict::Forbidden
            })
    }
}
