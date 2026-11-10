//! The kernel machine ([`kernel::KernelMachine`]) runs the bytecode core
//! ([`crate::vm`]) over its slot heap ([`crate::heap`]) and seals and opens
//! through [`crate::durable`].

pub(crate) mod kernel;

/// What a law's hook has the run do at a statement boundary (FIG-6318).
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Boundary {
    /// A collection through the machine's collection point.
    Collect,
}
