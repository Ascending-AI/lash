use std::sync::Arc;

use super::control::AwaitEventResolver;

/// Whether turn-control reads participate in a durable controller journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnControlParticipation {
    Local,
    DurableJournaled,
}

/// Which durable authority owns the reserved turn-control promises.
///
/// Most effect hosts own their cancellation promises together with their
/// journal. The process-local Native host instead delegates only those three
/// reserved waits to the persistent session store when one is available.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnControlAuthorityOwner {
    EffectHost,
    SessionStore,
}

/// A reopenable authority for the reserved turn-cancellation promises.
///
/// The resolver is intentionally owned: reopening a session must recover the
/// same database-backed authority without borrowing an effect-host invocation.
#[derive(Clone)]
pub struct TurnCancellationAuthority {
    binding_id: String,
    resolver: Arc<dyn AwaitEventResolver>,
}

impl TurnCancellationAuthority {
    pub fn new(binding_id: impl Into<String>, resolver: Arc<dyn AwaitEventResolver>) -> Self {
        Self {
            binding_id: binding_id.into(),
            resolver,
        }
    }

    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    pub fn resolver(&self) -> Arc<dyn AwaitEventResolver> {
        Arc::clone(&self.resolver)
    }
}
