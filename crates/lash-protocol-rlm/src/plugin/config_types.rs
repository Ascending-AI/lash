pub use lash_kernel_vm::{Charge, Durability};
use std::num::NonZeroU64;

pub use lash_kernel_vm::MemoryLimit;

// Keep the existing recorded spelling while the builder takes the kernel policy.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(
    remote = "Durability",
    rename = "Durability",
    rename_all = "snake_case"
)]
pub(super) enum DurabilityWire {
    Durable,
    Ephemeral,
}

pub(super) fn is_durable(policy: &Durability) -> bool {
    matches!(policy, Durability::Durable)
}

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

/// How much kernel charge (`K-COST-001`: steps, plus the work library
/// functions charge) an execution may run for.
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

    /// The standard instruction budget, set from measurement: 20,000,000
    /// charge units. A host that wants another states it with
    /// [`Self::instructions`].
    ///
    /// Measured with kernel_perf over the repository's model-written cells,
    /// each run with its tool calls answered at once (FIG-5822): the
    /// lash-perf scenarios, the examples' and the sim's scripted cells and
    /// the facade's and durable tests' fixtures, 112 cells that run outside
    /// their hosts. Their median charge is 380. The costliest builds 3,000
    /// rows, prints them and finishes with them, at 9,097,832; the next is
    /// the large-print scenario at 2,121,577. Twice the costliest, rounded
    /// up to the next five million, is 20,000,000: about half a second of
    /// kernel work on the measuring host. `kernel_perf 3 rows_print_3000`
    /// measures that cell: 9,217,786 under FIG-5825's charges. The lash
    /// crate's law
    /// `tests::cell_budget::the_costliest_measured_cell_fits_half_the_standard_instruction_budget`
    /// keeps it within half this budget, so the derivation holds.
    pub const fn standard() -> Self {
        Self::instructions(20_000_000)
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
}

nonzero_bound_serde!(InstructionBound);

/// The conservative bound on logical bytes an execution may hold, metered by the
/// kernel's size schedule rather than by the allocator or RSS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryBound(Option<NonZeroU64>);

impl MemoryBound {
    /// A finite logical heap limit, in bytes.
    ///
    /// Named *logical* on purpose: the ceiling is metered by the kernel's
    /// size schedule, not by the allocator or by RSS. A host reading `bytes(..)` at
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
}

nonzero_bound_serde!(MemoryBound);

/// The two independent bounds every code mode execution must choose
/// explicitly, and whether its run can be saved (durable unless stated).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionBounds {
    pub charge: Charge,
    pub instruction_limit: InstructionBound,
    pub memory_limit: MemoryBound,
    pub durability: Durability,
    pub on_memory_limit: MemoryLimit,
}

impl ExecutionBounds {
    pub const fn new(instruction_limit: InstructionBound, memory_limit: MemoryBound) -> Self {
        Self {
            charge: Charge::Exact,
            instruction_limit,
            memory_limit,
            durability: Durability::Durable,
            on_memory_limit: MemoryLimit::Recount,
        }
    }

    pub const fn with_charge(mut self, charge: Charge) -> Self {
        self.charge = charge;
        self
    }

    pub const fn with_memory_limit(mut self, memory_limit: MemoryBound) -> Self {
        self.memory_limit = memory_limit;
        self
    }

    pub const fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }

    /// Choose what happens at the conservative memory bound; Recount is the default.
    pub const fn on_memory_limit(mut self, policy: MemoryLimit) -> Self {
        self.on_memory_limit = policy;
        self
    }

    pub const fn unbounded() -> Self {
        Self::new(InstructionBound::unbounded(), MemoryBound::unbounded())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.0-BASELINE: kernel policy ownership keeps the existing config encoding,
    /// including the omitted durable default, on creation and recorded reopen.
    #[test]
    fn a_recorded_durability_policy_keeps_its_existing_encoding() {
        for (policy, encoded) in [
            (Durability::Durable, None),
            (Durability::Ephemeral, Some(serde_json::json!("ephemeral"))),
        ] {
            let config = crate::RlmProtocolPluginConfig::standard()
                .channel(crate::RlmChannel::Cell)
                .instruction_limit(InstructionBound::unbounded())
                .memory_limit(MemoryBound::unbounded())
                .durability(policy)
                .build();
            let value = serde_json::to_value(&config).expect("config encodes");
            assert_eq!(value.get("durability"), encoded.as_ref());
            let restored: crate::RlmProtocolPluginConfig =
                serde_json::from_value(value).expect("existing config decodes");
            assert_eq!(restored.execution_bounds().durability, policy);
            let recorded = config
                .recorded_behaviour(&crate::CellDialect::typescript())
                .expect("the shipped presenter admits its config");
            let value = serde_json::to_value(&recorded).expect("recorded config encodes");
            assert_eq!(value.get("durability"), encoded.as_ref());
            let reopened: crate::RlmRecordedBehaviour =
                serde_json::from_value(value).expect("existing recorded config decodes");
            assert_eq!(reopened.durability, policy);
        }
        // Serde's enum name is also observable to name-sensitive formats
        // and to deserializers reporting an invalid value.
        let invalid = || serde::de::value::BoolDeserializer::<serde::de::value::Error>::new(false);
        let baseline = <lash_vm_client::Durability as serde::Deserialize>::deserialize(invalid())
            .expect_err("a boolean is not a durability policy");
        let restored = DurabilityWire::deserialize(invalid())
            .expect_err("the kernel policy rejects the same invalid value");
        assert_eq!(restored.to_string(), baseline.to_string());
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
