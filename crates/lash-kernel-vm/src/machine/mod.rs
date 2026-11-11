//! The kernel machine ([`kernel::KernelMachine`]) runs the bytecode core
//! ([`crate::vm`]) over its slot heap ([`crate::heap`]) and seals and opens
//! through [`crate::durable`].

pub(crate) mod kernel;
