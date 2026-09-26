//! The recorded outcomes of a session drive's admission steps (FIG-3600) and
//! of a root's turn-config resolution (FIG-3600 S6).

use super::{RuntimeEffectControllerError, RuntimeEffectOutcome};
use crate::RuntimeEffectKind;

impl RuntimeEffectOutcome {
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
