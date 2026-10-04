//! The turn's executable-generation fence (FIG-3571).
//!
//! A turn's plugin transition records the executable generation its code executor
//! runs cells under ([`CodeExecutorPlugin::executable_generation`]), and every
//! redrive checks it after publication and before the turn's body runs:
//! a build of another generation would compile, key or meter the turn's cells
//! differently from the journal it replays, so it refuses the turn typed,
//! before any model, tool or provider effect, and the turn parks for a build of
//! its own generation ([`ParkReason::RetiredGeneration`]).
//!
//! [`current`] reads the execution's bound executor, and [`admit`] checks it
//! against the generation the plugin transition recorded after conversion
//! and executor binding. Publication checks the fence once before Run effects.
//!
//! [`CodeExecutorPlugin::executable_generation`]: crate::plugin::CodeExecutorPlugin::executable_generation
//! [`ParkReason::RetiredGeneration`]: crate::store::ParkReason::RetiredGeneration

use super::*;

/// The executable generation this build runs `runtime`'s turns under: the one
/// a new plugin transition records.
/// FIG-3795 E: this is where the build-generation stamp (`park_build_generation`) is also written.
pub(in crate::runtime) fn current(runtime: &LashRuntime) -> Option<crate::ExecutableGeneration> {
    runtime.services.plugins.is_materialized().then(|| {
        runtime
            .services
            .plugins
            .code_executor()
            .and_then(|executor| executor.executable_generation())
    })?
}

/// Admits a turn whose plugin transition recorded `recorded` into this build, or
/// refuses it with the typed [`RuntimeErrorCode::RetiredGeneration`]
/// the turn parks on. A transition that recorded no generation is admitted
/// only by a build whose executor names none: a missing stamp is never taken
/// for this build's.
pub(in crate::runtime) fn admit(
    runtime: &LashRuntime,
    recorded: Option<&crate::ExecutableGeneration>,
) -> Result<(), RuntimeError> {
    crate::ExecutableGenerationRefusal::check(recorded, current(runtime))
        .map_err(RuntimeError::retired_generation)
}
