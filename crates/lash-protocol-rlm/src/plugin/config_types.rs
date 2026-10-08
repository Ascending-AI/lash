use std::num::NonZeroU64;

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
    /// Named for the engine's own `ExecutionBound::logical_bytes`, and named
    /// *logical* on purpose: the ceiling is metered by the Lashlang heap size
    /// schedule, not by the allocator or by RSS. A host reading `bytes(..)` at
    /// a call site had to know which axis the value landed on to know what it
    /// meant; this one says so.
    ///
    /// # Panics
    ///
    /// Panics when `bytes` is zero.
    pub const fn logical_bytes(bytes: u64) -> Self {
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
            Some(bytes) => Self::logical_bytes(bytes),
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

/// The two independent bounds every RLM execution must choose explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
}

impl ExecutionBounds {
    pub const fn new(instruction_limit: InstructionBound, memory_limit: MemoryBound) -> Self {
        Self {
            instruction_limit,
            memory_limit,
        }
    }

    pub const fn with_memory_limit(mut self, memory_limit: MemoryBound) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    pub const fn unbounded() -> Self {
        Self::new(InstructionBound::unbounded(), MemoryBound::unbounded())
    }

    pub(crate) fn into_engine(self) -> lashlang::ExecutionBounds {
        lashlang::ExecutionBounds::new(
            self.instruction_limit.into_engine(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                MemoryBound::logical_bytes(0);
            }),
            ("memory limit must be non-zero", || {
                MemoryBound::mebibytes(0);
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
}
