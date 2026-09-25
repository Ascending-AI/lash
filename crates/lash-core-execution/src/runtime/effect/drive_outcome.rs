//! The recorded outcomes of a session drive's steps (FIG-3600): its
//! admission and seal, a root's input claim and a root's scope close.

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

    pub fn into_accepted_turn_input_drive(
        self,
    ) -> Result<crate::AcceptedTurnInputDrive, RuntimeEffectControllerError> {
        match self {
            Self::ClaimAcceptedTurnInput { drive } => Ok(drive),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ClaimAcceptedTurnInput,
                other.kind(),
            )),
        }
    }

    pub fn into_admit_drive(
        self,
    ) -> Result<crate::engine::AdmitVerdict, RuntimeEffectControllerError> {
        match self {
            Self::AdmitDrive { verdict } => Ok(*verdict),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AdmitDrive,
                other.kind(),
            )),
        }
    }

    pub fn into_close_root_scope(
        self,
    ) -> Result<crate::store::RootTerminal, RuntimeEffectControllerError> {
        match self {
            Self::CloseRootScope { terminal } => Ok(*terminal),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::CloseRootScope,
                other.kind(),
            )),
        }
    }

    pub fn into_seal_drive_admission(
        self,
    ) -> Result<crate::engine::SealVerdict, RuntimeEffectControllerError> {
        match self {
            Self::SealDriveAdmission { verdict } => Ok(*verdict),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::SealDriveAdmission,
                other.kind(),
            )),
        }
    }
}
