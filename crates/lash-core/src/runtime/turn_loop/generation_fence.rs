//! The turn's executable-generation fence (FIG-3571).
//!
//! A turn's admission records the executable generation its code executor
//! runs cells under ([`CodeExecutorPlugin::executable_generation`]), and every
//! redrive of that admission checks it before anything else of the turn runs:
//! a build of another generation would compile, key or meter the turn's cells
//! differently from the journal it replays, so it refuses the turn typed,
//! before any model, tool or provider effect, and the turn parks for a build of
//! its own generation ([`ParkReason::RetiredGeneration`]).
//!
//! This file is the whole fence: [`current`] is the one place the stamp is
//! read from the build, and [`admit`] the one place it is checked. Each
//! admission site calls them with one line; FIG-3600 moves those calls onto
//! the one driver claim.
//!
//! [`CodeExecutorPlugin::executable_generation`]: crate::plugin::CodeExecutorPlugin::executable_generation
//! [`ParkReason::RetiredGeneration`]: crate::store::ParkReason::RetiredGeneration

use super::*;

/// The executable generation this build runs `runtime`'s turns under: the one
/// a new admission records.
/// FIG-3795 E: this is where the build-generation stamp (`park_build_generation`) is also written.
pub(super) fn current(runtime: &LashRuntime) -> Option<crate::ExecutableGeneration> {
    runtime
        .session
        .as_ref()
        .and_then(|session| session.plugins().code_executor())
        .and_then(|executor| executor.executable_generation())
}

/// Admits a turn whose admission recorded `recorded` into this build, or
/// refuses it with the typed [`RuntimeErrorCode::RetiredGeneration`]
/// the turn parks on. An admission that recorded no generation is admitted
/// only by a build whose executor names none: a missing stamp is never taken
/// for this build's.
pub(super) fn admit(
    runtime: &LashRuntime,
    recorded: Option<&crate::ExecutableGeneration>,
) -> Result<(), RuntimeError> {
    let current = current(runtime);
    if recorded == current.as_ref() {
        return Ok(());
    }
    Err(RuntimeError::retired_generation(
        crate::ExecutableGenerationRefusal {
            found: recorded.cloned(),
            current,
        },
    ))
}
