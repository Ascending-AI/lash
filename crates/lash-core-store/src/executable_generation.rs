//! The executable generation a durable run was admitted under (FIG-3571).
//!
//! A turn journals its admission, and a process its start, and a replay of
//! that journal is correct only when the build replaying it runs the same code
//! the admitting build ran: the same compile of the same program, keyed and
//! metered the same way. The code executor, or the process engine, names that
//! as one opaque generation. The admission records the generation it ran
//! under, and a redrive under any other generation is refused before its first
//! effect, typed, so the run parks for a build of its own generation rather
//! than re-issuing effects its journal already holds.

use serde::{Deserialize, Serialize};

/// The executable generation a code executor runs under. Opaque to everything
/// but equality.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
pub struct ExecutableGeneration(String);

impl ExecutableGeneration {
    pub fn new(generation: impl Into<String>) -> Self {
        Self(generation.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExecutableGeneration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The generations an admission check refused: the one the run's admission
/// recorded and the one this build runs. `None` names no generation: an
/// admission recorded without one, or a build whose executor names none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableGenerationRefusal {
    pub found: Option<ExecutableGeneration>,
    pub current: Option<ExecutableGeneration>,
}

impl ExecutableGenerationRefusal {
    /// The spelling a message and a park reason carry for one side.
    #[must_use]
    pub fn spell(generation: Option<&ExecutableGeneration>) -> String {
        generation.map_or_else(|| "none".to_string(), ToString::to_string)
    }

    /// Admits a run whose admission recorded `recorded` into a build that runs
    /// `current`, or refuses it. A run recorded with no generation is admitted
    /// only by a build that names none: a missing stamp is never taken for
    /// this build's.
    pub fn check(
        recorded: Option<&ExecutableGeneration>,
        current: Option<ExecutableGeneration>,
    ) -> Result<(), Self> {
        if recorded == current.as_ref() {
            return Ok(());
        }
        Err(Self {
            found: recorded.cloned(),
            current,
        })
    }
}
