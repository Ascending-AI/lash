//! The outcomes of a session's own steps: an accepted turn input, its scope
//! close (S7) and a session's close.

use super::{RuntimeEffectControllerError, RuntimeEffectOutcome};
use crate::RuntimeEffectKind;

impl RuntimeEffectOutcome {
    pub fn into_accepted_turn_input(
        self,
    ) -> Result<crate::PendingTurnInput, RuntimeEffectControllerError> {
        match self {
            Self::AcceptTurnInput { accepted } => Ok(*accepted),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AcceptTurnInput,
                other.kind(),
            )),
        }
    }

    pub fn into_close_run_scope(self) -> Result<(), RuntimeEffectControllerError> {
        match self {
            Self::CloseRunScope => Ok(()),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::CloseRunScope,
                other.kind(),
            )),
        }
    }

    pub fn into_compaction_base(
        self,
    ) -> Result<super::CompactionBase, RuntimeEffectControllerError> {
        match self {
            Self::RecordCompactionBase { base } => Ok(*base),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::RecordCompactionBase,
                other.kind(),
            )),
        }
    }

    pub fn into_config_resolution(
        self,
    ) -> Result<crate::ConfigResolution, RuntimeEffectControllerError> {
        match self {
            Self::ResolveConfigTransaction { resolution } => Ok(*resolution),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ResolveConfigTransaction,
                other.kind(),
            )),
        }
    }

    pub fn into_session_command_run(
        self,
    ) -> Result<Vec<crate::QueuedWorkBatch>, RuntimeEffectControllerError> {
        match self {
            Self::ReadSessionCommandRun { batches } => Ok(batches),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ReadSessionCommandRun,
                other.kind(),
            )),
        }
    }
}
