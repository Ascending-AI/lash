//! The engine-neutral served-only contract a local executor carries
//! (FIG-3587, FIG-3719).

use std::sync::Arc;

use super::{CommandJournalGuard, RuntimeEffectControllerError};

/// A served-only effect's refusal, and the command guard it trips
/// (FIG-3719). An engine that cannot tell before it starts an effect whether
/// its journal holds the outcome — one that learns only as its replay reaches
/// the effect — keeps this handle and refuses with it once it knows the
/// effect would run live.
#[derive(Clone)]
pub struct ServedOnly {
    pub(super) refusal: RuntimeEffectControllerError,
    pub(super) guard: Arc<CommandJournalGuard>,
}

impl ServedOnly {
    /// Refuses the effect: trips the command's guard, so the run stops on
    /// the refusal however the effect's caller shapes the error, and returns
    /// the refusal for the engine to return in place of an outcome.
    pub fn refuse(&self) -> RuntimeEffectControllerError {
        self.guard.trip(&self.refusal);
        self.refusal.clone()
    }
}
