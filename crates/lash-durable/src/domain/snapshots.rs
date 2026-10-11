//! Execution roots (L7; L6 writes `p/<pid>` through L7's store): the latest
//! root per execution, compare-and-set on its revision (ADR 0132 §8). The
//! row is [`ExecRootRow`](super::ExecRootRow); its root is an ExecRoot
//! checkpoint graph node.

/// A root's revision within its execution, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotRev(pub u64);
