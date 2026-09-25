//! The recorded outcomes of a session drive's admission steps (FIG-3600).

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
