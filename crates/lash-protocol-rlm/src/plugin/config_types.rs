use std::num::NonZeroU64;
use std::time::Duration;

/// Serialized shape shared by every execution bound: an explicit finite limit
/// or an explicit opt-out. Bounds are distinct Rust types so that an
/// instruction budget can never be passed where a memory limit is meant, but
/// they all speak one wire vocabulary.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum ExecutionBoundWire<T> {
    Bounded(T),
    Unbounded,
}

macro_rules! nonzero_bound_serde {
    ($bound:ty) => {
        impl serde::Serialize for $bound {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                match self.0 {
                    Some(value) => ExecutionBoundWire::Bounded(value).serialize(serializer),
                    None => ExecutionBoundWire::<NonZeroU64>::Unbounded.serialize(serializer),
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $bound {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                Ok(
                    match ExecutionBoundWire::<NonZeroU64>::deserialize(deserializer)? {
                        ExecutionBoundWire::Bounded(value) => Self(Some(value)),
                        ExecutionBoundWire::Unbounded => Self(None),
                    },
                )
            }
        }
    };
}

/// How many VM instructions (plus the collection work builtins charge) an
/// execution may run for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstructionBound(Option<NonZeroU64>);

impl InstructionBound {
    /// A finite instruction budget.
    ///
    /// # Panics
    ///
    /// Panics when `instructions` is zero: an execution that may run no
    /// instructions at all is a configuration mistake, not a bound.
    pub const fn instructions(instructions: u64) -> Self {
        match NonZeroU64::new(instructions) {
            Some(instructions) => Self(Some(instructions)),
            None => panic!("instruction budget must be non-zero"),
        }
    }

    /// An explicit opt-out: the host takes responsibility for stopping runaway
    /// executions by some other means.
    pub const fn unbounded() -> Self {
        Self(None)
    }

    /// The finite instruction budget, or `None` when unbounded.
    pub const fn limit(self) -> Option<NonZeroU64> {
        self.0
    }

    fn into_engine(self) -> lashlang::ExecutionBound<NonZeroU64> {
        match self.0 {
            Some(value) => lashlang::ExecutionBound::Bounded(value),
            None => lashlang::ExecutionBound::Unbounded,
        }
    }
}

nonzero_bound_serde!(InstructionBound);

/// How many live logical heap bytes an execution may hold, metered by the
/// Lashlang heap size schedule rather than by the allocator or RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBound(Option<NonZeroU64>);

impl MemoryBound {
    /// A finite logical heap limit, in bytes.
    ///
    /// # Panics
    ///
    /// Panics when `bytes` is zero.
    pub const fn bytes(bytes: u64) -> Self {
        match NonZeroU64::new(bytes) {
            Some(bytes) => Self(Some(bytes)),
            None => panic!("memory limit must be non-zero"),
        }
    }

    /// A finite logical heap limit expressed in mebibytes, which is how hosts
    /// usually think about it.
    ///
    /// # Panics
    ///
    /// Panics when `mebibytes` is zero or the byte count overflows `u64`.
    pub const fn mebibytes(mebibytes: u64) -> Self {
        match mebibytes.checked_mul(1024 * 1024) {
            Some(bytes) => Self::bytes(bytes),
            None => panic!("memory limit in mebibytes overflows a byte count"),
        }
    }

    /// An explicit opt-out: the execution may grow its logical heap without a
    /// protocol-enforced ceiling.
    pub const fn unbounded() -> Self {
        Self(None)
    }

    /// The finite logical heap limit in bytes, or `None` when unbounded.
    pub const fn limit(self) -> Option<NonZeroU64> {
        self.0
    }

    fn into_engine(self) -> lashlang::ExecutionBound<NonZeroU64> {
        match self.0 {
            Some(value) => lashlang::ExecutionBound::Bounded(value),
            None => lashlang::ExecutionBound::Unbounded,
        }
    }
}

nonzero_bound_serde!(MemoryBound);

/// How much active VM execution time an execution may consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WallClockBound(Option<Duration>);

impl WallClockBound {
    /// A finite execution deadline, in milliseconds.
    ///
    /// # Panics
    ///
    /// Panics when `milliseconds` is zero: an execution that may run for no
    /// time at all is a configuration mistake, not a deadline. Use
    /// [`WallClockBound::unbounded`] for the explicit opt-out.
    pub const fn millis(milliseconds: u64) -> Self {
        match NonZeroU64::new(milliseconds) {
            Some(_) => Self(Some(Duration::from_millis(milliseconds))),
            None => panic!("wall-clock deadline must be non-zero"),
        }
    }

    /// A finite execution deadline, in seconds.
    ///
    /// # Panics
    ///
    /// Panics when `seconds` is zero.
    pub const fn secs(seconds: u64) -> Self {
        match NonZeroU64::new(seconds) {
            Some(_) => Self(Some(Duration::from_secs(seconds))),
            None => panic!("wall-clock deadline must be non-zero"),
        }
    }

    /// An explicit opt-out: the execution is not stopped on elapsed time.
    pub const fn unbounded() -> Self {
        Self(None)
    }

    /// The finite deadline, or `None` when unbounded.
    pub const fn limit(self) -> Option<Duration> {
        self.0
    }

    fn into_engine(self) -> lashlang::ExecutionBound<Duration> {
        match self.0 {
            Some(value) => lashlang::ExecutionBound::Bounded(value),
            None => lashlang::ExecutionBound::Unbounded,
        }
    }
}

impl serde::Serialize for WallClockBound {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Some(value) => {
                let milliseconds =
                    u64::try_from(value.as_millis()).map_err(serde::ser::Error::custom)?;
                ExecutionBoundWire::Bounded(milliseconds).serialize(serializer)
            }
            None => ExecutionBoundWire::<u64>::Unbounded.serialize(serializer),
        }
    }
}

impl<'de> serde::Deserialize<'de> for WallClockBound {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(
            match ExecutionBoundWire::<u64>::deserialize(deserializer)? {
                // A zero deadline is rejected rather than accepted as an
                // instant timeout, matching the constructors; decoding must
                // report it, never panic.
                ExecutionBoundWire::Bounded(0) => {
                    return Err(serde::de::Error::custom(
                        "wall-clock deadline must be non-zero",
                    ));
                }
                ExecutionBoundWire::Bounded(milliseconds) => Self::millis(milliseconds),
                ExecutionBoundWire::Unbounded => Self::unbounded(),
            },
        )
    }
}

/// The three independent bounds every RLM execution must choose explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub instruction_limit: InstructionBound,
    pub wall_clock: WallClockBound,
    pub memory_limit: MemoryBound,
}

impl ExecutionBounds {
    pub const fn new(
        instruction_limit: InstructionBound,
        wall_clock: WallClockBound,
        memory_limit: MemoryBound,
    ) -> Self {
        Self {
            instruction_limit,
            wall_clock,
            memory_limit,
        }
    }

    pub const fn with_memory_limit(mut self, memory_limit: MemoryBound) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    pub const fn unbounded() -> Self {
        Self::new(
            InstructionBound::unbounded(),
            WallClockBound::unbounded(),
            MemoryBound::unbounded(),
        )
    }

    pub(crate) fn into_engine(self) -> lashlang::ExecutionBounds {
        lashlang::ExecutionBounds::new(
            self.instruction_limit.into_engine(),
            self.wall_clock.into_engine(),
            self.memory_limit.into_engine(),
        )
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
        let instruction_limit = InstructionBound::instructions(1_000_000);
        let wall_clock = WallClockBound::millis(30_000);
        let memory_limit = MemoryBound::mebibytes(64);
        assert_eq!(
            serde_json::to_value(instruction_limit).expect("protocol instruction budget"),
            serde_json::to_value(lashlang::ExecutionBound::instructions(1_000_000))
                .expect("engine instruction budget")
        );
        assert_eq!(
            serde_json::to_value(wall_clock).expect("protocol deadline"),
            serde_json::to_value(lashlang::ExecutionBound::millis(30_000))
                .expect("engine deadline")
        );
        assert_eq!(
            serde_json::to_value(memory_limit).expect("protocol memory limit"),
            serde_json::to_value(lashlang::ExecutionBound::instructions(64 * 1024 * 1024))
                .expect("engine memory limit")
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

    #[test]
    fn bounds_round_trip_their_values() {
        assert_eq!(
            InstructionBound::instructions(1_000_000).limit(),
            NonZeroU64::new(1_000_000)
        );
        assert_eq!(InstructionBound::unbounded().limit(), None);

        assert_eq!(
            MemoryBound::bytes(64 * 1024 * 1024).limit(),
            NonZeroU64::new(64 * 1024 * 1024)
        );
        assert_eq!(
            MemoryBound::mebibytes(64),
            MemoryBound::bytes(64 * 1024 * 1024)
        );
        assert_eq!(MemoryBound::unbounded().limit(), None);

        assert_eq!(
            WallClockBound::secs(30).limit(),
            Some(Duration::from_secs(30))
        );
        assert_eq!(WallClockBound::secs(30), WallClockBound::millis(30_000));
        assert_eq!(WallClockBound::unbounded().limit(), None);
    }

    #[test]
    fn execution_bounds_keep_each_limit_on_its_own_axis() {
        let bounds = ExecutionBounds::new(
            InstructionBound::instructions(7),
            WallClockBound::secs(11),
            MemoryBound::mebibytes(13),
        );

        assert_eq!(bounds.instruction_limit.limit(), NonZeroU64::new(7));
        assert_eq!(bounds.wall_clock.limit(), Some(Duration::from_secs(11)));
        assert_eq!(
            bounds.memory_limit.limit(),
            NonZeroU64::new(13 * 1024 * 1024)
        );
        assert_eq!(
            bounds
                .with_memory_limit(MemoryBound::unbounded())
                .memory_limit,
            MemoryBound::unbounded()
        );
    }

    #[test]
    fn every_finite_bound_rejects_zero() {
        // Zero is not a bound on any axis: `unbounded()` is the explicit
        // opt-out, so a zero argument is always a configuration mistake.
        for (label, construct) in [
            (
                "instruction budget must be non-zero",
                (|| {
                    InstructionBound::instructions(0);
                }) as fn(),
            ),
            ("memory limit must be non-zero", || {
                MemoryBound::bytes(0);
            }),
            ("memory limit must be non-zero", || {
                MemoryBound::mebibytes(0);
            }),
            ("wall-clock deadline must be non-zero", || {
                WallClockBound::millis(0);
            }),
            ("wall-clock deadline must be non-zero", || {
                WallClockBound::secs(0);
            }),
        ] {
            let panic = std::panic::catch_unwind(construct).expect_err(label);
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str))
                .expect("panic payload");
            assert_eq!(message, label);
        }
    }

    #[test]
    fn a_zero_wall_clock_wire_value_decodes_to_an_error_not_a_panic() {
        let error = serde_json::from_value::<WallClockBound>(serde_json::json!({ "bounded": 0 }))
            .expect_err("zero deadline must be rejected");
        assert!(
            error
                .to_string()
                .contains("wall-clock deadline must be non-zero"),
            "unexpected error: {error}"
        );
    }
}
