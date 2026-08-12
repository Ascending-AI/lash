use std::num::NonZeroU64;
use std::time::Duration;

/// An explicit finite execution limit or an explicit opt-out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionBound<T> {
    Bounded(T),
    Unbounded,
}

impl ExecutionBound<NonZeroU64> {
    /// Construct a finite instruction budget.
    ///
    /// # Panics
    ///
    /// Panics when `instructions` is zero.
    pub const fn instructions(instructions: u64) -> Self {
        match NonZeroU64::new(instructions) {
            Some(instructions) => Self::Bounded(instructions),
            None => panic!("instruction budget must be non-zero"),
        }
    }
}

impl From<lashlang::ExecutionBound<NonZeroU64>> for ExecutionBound<NonZeroU64> {
    fn from(value: lashlang::ExecutionBound<NonZeroU64>) -> Self {
        match value {
            lashlang::ExecutionBound::Bounded(value) => Self::Bounded(value),
            lashlang::ExecutionBound::Unbounded => Self::Unbounded,
        }
    }
}

impl ExecutionBound<Duration> {
    pub const fn millis(milliseconds: u64) -> Self {
        Self::Bounded(Duration::from_millis(milliseconds))
    }

    pub const fn secs(seconds: u64) -> Self {
        Self::Bounded(Duration::from_secs(seconds))
    }
}

impl From<lashlang::ExecutionBound<Duration>> for ExecutionBound<Duration> {
    fn from(value: lashlang::ExecutionBound<Duration>) -> Self {
        match value {
            lashlang::ExecutionBound::Bounded(value) => Self::Bounded(value),
            lashlang::ExecutionBound::Unbounded => Self::Unbounded,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionBoundWire<T> {
    Bounded(T),
    Unbounded,
}

impl serde::Serialize for ExecutionBound<NonZeroU64> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Bounded(value) => ExecutionBoundWire::Bounded(*value).serialize(serializer),
            Self::Unbounded => ExecutionBoundWire::<NonZeroU64>::Unbounded.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionBound<NonZeroU64> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match ExecutionBoundWire::deserialize(deserializer)? {
            ExecutionBoundWire::Bounded(value) => Self::Bounded(value),
            ExecutionBoundWire::Unbounded => Self::Unbounded,
        })
    }
}

impl serde::Serialize for ExecutionBound<Duration> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Bounded(value) => {
                let milliseconds =
                    u64::try_from(value.as_millis()).map_err(serde::ser::Error::custom)?;
                ExecutionBoundWire::Bounded(milliseconds).serialize(serializer)
            }
            Self::Unbounded => ExecutionBoundWire::<u64>::Unbounded.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for ExecutionBound<Duration> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match ExecutionBoundWire::<u64>::deserialize(deserializer)? {
                ExecutionBoundWire::Bounded(milliseconds) => Self::millis(milliseconds),
                ExecutionBoundWire::Unbounded => Self::Unbounded,
            },
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub instruction_budget: ExecutionBound<NonZeroU64>,
    pub deadline: ExecutionBound<Duration>,
    pub memory_limit: ExecutionBound<NonZeroU64>,
}

impl ExecutionBounds {
    pub const fn new(
        instruction_budget: ExecutionBound<NonZeroU64>,
        deadline: ExecutionBound<Duration>,
    ) -> Self {
        Self {
            instruction_budget,
            deadline,
            memory_limit: ExecutionBound::instructions(lashlang::DEFAULT_HEAP_LOGICAL_BYTE_LIMIT),
        }
    }

    pub const fn with_memory_limit(mut self, memory_limit: ExecutionBound<NonZeroU64>) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    pub const fn unbounded() -> Self {
        Self::new(ExecutionBound::Unbounded, ExecutionBound::Unbounded)
            .with_memory_limit(ExecutionBound::Unbounded)
    }

    pub(crate) fn into_engine(self) -> lashlang::ExecutionBounds {
        lashlang::ExecutionBounds::new(
            self.instruction_budget.into_engine(),
            self.deadline.into_engine(),
        )
        .with_memory_limit(self.memory_limit.into_engine())
    }
}

impl ExecutionBound<NonZeroU64> {
    fn into_engine(self) -> lashlang::ExecutionBound<NonZeroU64> {
        match self {
            Self::Bounded(value) => lashlang::ExecutionBound::Bounded(value),
            Self::Unbounded => lashlang::ExecutionBound::Unbounded,
        }
    }
}

impl ExecutionBound<Duration> {
    fn into_engine(self) -> lashlang::ExecutionBound<Duration> {
        match self {
            Self::Bounded(value) => lashlang::ExecutionBound::Bounded(value),
            Self::Unbounded => lashlang::ExecutionBound::Unbounded,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmLanguageFeatures {
    pub label_annotations: bool,
}

impl RlmLanguageFeatures {
    pub fn union(self, other: Self) -> Self {
        Self {
            label_annotations: self.label_annotations || other.label_annotations,
        }
    }

    pub fn satisfies(self, required: Self) -> bool {
        !required.label_annotations || self.label_annotations
    }

    pub fn with_label_annotations(mut self) -> Self {
        self.label_annotations = true;
        self
    }

    pub(crate) fn into_engine(self) -> lashlang::LashlangLanguageFeatures {
        lashlang::LashlangLanguageFeatures {
            label_annotations: self.label_annotations,
        }
    }
}

impl From<lashlang::LashlangLanguageFeatures> for RlmLanguageFeatures {
    fn from(value: lashlang::LashlangLanguageFeatures) -> Self {
        Self {
            label_annotations: value.label_annotations,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct RlmAbilities {
    pub processes: bool,
    pub sleep: bool,
    pub process_signals: bool,
    pub triggers: bool,
}

impl RlmAbilities {
    pub fn union(self, other: Self) -> Self {
        Self {
            processes: self.processes || other.processes,
            sleep: self.sleep || other.sleep,
            process_signals: self.process_signals || other.process_signals,
            triggers: self.triggers || other.triggers,
        }
    }

    pub fn satisfies(self, required: Self) -> bool {
        (!required.processes || self.processes)
            && (!required.sleep || self.sleep)
            && (!required.process_signals || self.process_signals)
            && (!required.triggers || self.triggers)
    }

    pub fn with_processes(mut self) -> Self {
        self.processes = true;
        self
    }

    pub fn with_sleep(mut self) -> Self {
        self.sleep = true;
        self
    }

    pub fn with_process_signals(mut self) -> Self {
        self.process_signals = true;
        self
    }

    pub fn with_triggers(mut self) -> Self {
        self.triggers = true;
        self
    }

    pub fn all() -> Self {
        Self::default()
            .with_sleep()
            .with_processes()
            .with_process_signals()
            .with_triggers()
    }

    pub(crate) fn into_engine(self) -> lashlang::LashlangAbilities {
        lashlang::LashlangAbilities {
            processes: self.processes,
            sleep: self.sleep,
            process_signals: self.process_signals,
            triggers: self.triggers,
        }
    }
}

impl From<lashlang::LashlangAbilities> for RlmAbilities {
    fn from(value: lashlang::LashlangAbilities) -> Self {
        Self {
            processes: value.processes,
            sleep: value.sleep,
            process_signals: value.process_signals,
            triggers: value.triggers,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_owned_types_preserve_the_engine_wire_shape() {
        let instruction_budget = ExecutionBound::instructions(1_000_000);
        let deadline = ExecutionBound::millis(30_000);
        assert_eq!(
            serde_json::to_value(instruction_budget).expect("protocol instruction budget"),
            serde_json::to_value(lashlang::ExecutionBound::instructions(1_000_000))
                .expect("engine instruction budget")
        );
        assert_eq!(
            serde_json::to_value(deadline).expect("protocol deadline"),
            serde_json::to_value(lashlang::ExecutionBound::millis(30_000))
                .expect("engine deadline")
        );

        let abilities = RlmAbilities::all();
        assert_eq!(
            serde_json::to_value(abilities).expect("protocol abilities"),
            serde_json::to_value(lashlang::LashlangAbilities::all()).expect("engine abilities")
        );

        let language_features = RlmLanguageFeatures::default().with_label_annotations();
        assert_eq!(
            serde_json::to_value(language_features).expect("protocol language features"),
            serde_json::to_value(
                lashlang::LashlangLanguageFeatures::default().with_label_annotations()
            )
            .expect("engine language features")
        );
    }
}
