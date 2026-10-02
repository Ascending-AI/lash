//! The recorded outcomes of a session shift's steps (FIG-3600): its
//! admission, run start and seal, a run's admission, a follow-on
//! recovery run's decision (FIG-4361), its turn-config
//! resolution (S6), its scope close (S7) and a session's close.

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

    pub fn into_run_admission(
        self,
    ) -> Result<crate::store::RunAdmissionAnswer, RuntimeEffectControllerError> {
        match self {
            Self::AdmitRun { answer } => Ok(answer),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AdmitRun,
                other.kind(),
            )),
        }
    }

    pub fn into_follow_on_recovery(
        self,
    ) -> Result<crate::store::FollowOnRecoveryAnswer, RuntimeEffectControllerError> {
        match self {
            Self::RecoverFollowOn { answer } => Ok(*answer),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::RecoverFollowOn,
                other.kind(),
            )),
        }
    }

    pub fn into_admit_shift(
        self,
    ) -> Result<crate::engine::AdmitVerdict, RuntimeEffectControllerError> {
        match self {
            Self::AdmitShift { verdict } => Ok(*verdict),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::AdmitShift,
                other.kind(),
            )),
        }
    }

    pub fn into_draw_run_start(
        self,
    ) -> Result<crate::engine::RunStartNonce, RuntimeEffectControllerError> {
        match self {
            Self::DrawRunStart { run_start } => Ok(run_start),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::DrawRunStart,
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

    pub fn into_begin_session_close(
        self,
    ) -> Result<Option<crate::store::ControlIntent>, RuntimeEffectControllerError> {
        match self {
            Self::BeginSessionClose { intent } => Ok(intent.map(|intent| *intent)),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::BeginSessionClose,
                other.kind(),
            )),
        }
    }

    pub fn into_seal_shift_admission(
        self,
    ) -> Result<crate::engine::SealVerdict, RuntimeEffectControllerError> {
        match self {
            Self::SealShiftAdmission { verdict } => Ok(*verdict),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::SealShiftAdmission,
                other.kind(),
            )),
        }
    }

    pub fn into_resolve_turn_config(
        self,
    ) -> Result<crate::ResolvedRun, RuntimeEffectControllerError> {
        match self {
            Self::ResolveTurnConfig { resolved } => Ok(*resolved),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::ResolveTurnConfig,
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

    pub fn into_compaction_prompt(
        self,
    ) -> Result<Option<std::sync::Arc<str>>, RuntimeEffectControllerError> {
        match self {
            Self::RenderCompactionPrompt { system_prompt } => Ok(system_prompt),
            other => Err(RuntimeEffectControllerError::wrong_outcome(
                RuntimeEffectKind::RenderCompactionPrompt,
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
