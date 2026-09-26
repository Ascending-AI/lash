//! The recorded outcomes of a session drive's steps (FIG-3600): its
//! admission, root start and seal, a root's input claim, its turn-config
//! resolution (S6) and its scope close (S7).

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

    pub fn into_draw_root_start(
        self,
    ) -> Result<crate::engine::RootStartNonce, RuntimeEffectControllerError> {
        match self {
            Self::DrawRootStart { root_start } => Ok(root_start),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::DrawRootStart,
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

    pub fn into_resolve_turn_config(
        self,
    ) -> Result<crate::PersistedSessionConfig, RuntimeEffectControllerError> {
        match self {
            Self::ResolveTurnConfig { config } => Ok(*config),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ResolveTurnConfig,
                other.kind(),
            )),
        }
    }
}
