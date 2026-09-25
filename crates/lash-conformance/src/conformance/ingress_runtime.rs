//! The runtime laws of the one session ingress (ADR 0101 §16, FIG-3600 S8).
//!
//! These are the laws the store cannot answer alone: they drive turns through
//! a tier's [`ConformanceTurnRunner`](crate::ConformanceTurnRunner) and read
//! what the runtime claimed, rendered, settled and refused. The fixture is
//! the drive-admission one: a guard, a prefix, the tier's effect host, the
//! store set under test and its runner.
//!
//! Owed here, each registered through [`ingress_runtime_tests!`](crate::ingress_runtime_tests):
//! - ADR 0101 §16 runtime laws 4, 6, 7, 9–11, 17, 18, 21 and 22;
//! - a mutation under a superseded drive epoch is refused before any I/O;
//! - a later epoch adopts a root an earlier one already committed;
//! - every root, queued or not, runs under `Turn(root)`;
//! - a turn renders inputs, then wakes;
//! - commands drain at the boundary before the turn lane is claimed;
//! - an interrupted hold at the lane head is re-derived exactly by the next
//!   admission.
