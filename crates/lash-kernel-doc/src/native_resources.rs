//! Logical obligations of one body-less native invocation.
//!
//! Owners are local to the invocation. A source description qualifies them with
//! its exact FunctionId and canonical call coordinate. These declarations do not
//! reserve physical nursery room or change guest admission.

use crate::LogicalHeldV1;

/// A native's canonical allocation or scratch site, qualified by its caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NativeResourceOwner(pub u32);

/// Which semantic exit transfers the output occurrence to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceExit {
    Returned,
    Raised,
}

/// Mutable identity is distinct from an immutable occurrence's storage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceOrigin {
    /// A scalar or recursively immutable value, priced at this occurrence.
    Immutable,
    /// An existing mutable entity. Its body remains owned by the input graph.
    Alias,
    /// A new mutable entity declared by the named allocation event.
    Fresh(NativeResourceOwner),
}

/// A value at one semantic location. Immutable children are included in bytes;
/// mutable children contribute an occurrence, with their bodies priced once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeResourceHolding {
    pub bytes: u64,
    pub origin: NativeResourceOrigin,
}

/// The kind of a fresh mutable entity, independently of arena representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceEntity {
    List,
    Map,
    Set,
    Record,
    Closure,
    Variable,
}

/// A scratch lease ends before the named event, or before exit publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeResourceRelease {
    Before(NativeResourceOwner),
    Exit,
}

/// One event in canonical native order. A member's byte count includes all of
/// its immutable descendants. Record field names are excluded by the algebra.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeResourceEvent {
    Entity {
        owner: NativeResourceOwner,
        kind: NativeResourceEntity,
        members: Vec<NativeResourceHolding>,
    },
    Scratch {
        owner: NativeResourceOwner,
        bytes: u64,
        release: NativeResourceRelease,
    },
}

/// An input-evaluated contract for one normal or raised native exit. Its
/// evaluator must use logical input facts, never residency or nursery room.
/// Retries and declines publish no exit and therefore have no contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeResourceContract {
    pub exit: NativeResourceExit,
    pub events: Vec<NativeResourceEvent>,
    pub output: NativeResourceHolding,
}

impl NativeResourceContract {
    /// A result with no new mutable entities or logical scratch obligation.
    pub fn value(exit: NativeResourceExit, bytes: u64, origin: NativeResourceOrigin) -> Self {
        Self {
            exit,
            events: Vec::new(),
            output: NativeResourceHolding { bytes, origin },
        }
    }

    /// The output occurrence and each fresh entity's direct members, once.
    /// Scratch is released before publication and never remains in this sum.
    pub fn retained_bytes(&self) -> Option<u64> {
        self.events
            .iter()
            .try_fold(self.output.bytes, |held, event| match event {
                NativeResourceEvent::Entity { members, .. } => {
                    let members = members
                        .iter()
                        .try_fold(0_u64, |sum, member| sum.checked_add(member.bytes))?;
                    held.checked_add(LogicalHeldV1::OBJECT)?
                        .checked_add(members)
                }
                NativeResourceEvent::Scratch { .. } => Some(held),
            })
    }

    /// Checked peak of simultaneously live scratch obligations. Invalid owner
    /// identities or backward/missing release points cannot certify a call.
    pub fn peak_scratch_bytes(&self) -> Option<u64> {
        let owners: Vec<_> = self.events.iter().map(NativeResourceEvent::owner).collect();
        for (at, owner) in owners.iter().enumerate() {
            if owners[..at].contains(owner) {
                return None;
            }
        }
        let mut live = 0_u64;
        let mut peak = 0_u64;
        let mut releases = vec![0_u64; self.events.len()];
        for (at, event) in self.events.iter().enumerate() {
            live = live.checked_sub(releases[at])?;
            if let NativeResourceEvent::Scratch { bytes, release, .. } = event {
                live = live.checked_add(*bytes)?;
                peak = peak.max(live);
                if let NativeResourceRelease::Before(owner) = release {
                    let end = owners.iter().position(|candidate| candidate == owner)?;
                    if end <= at {
                        return None;
                    }
                    releases[end] = releases[end].checked_add(*bytes)?;
                }
            }
        }
        Some(peak)
    }
}

impl NativeResourceEvent {
    pub fn owner(&self) -> NativeResourceOwner {
        match self {
            Self::Entity { owner, .. } | Self::Scratch { owner, .. } => *owner,
        }
    }
}
