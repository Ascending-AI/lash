//! Logical obligations of one body-less native invocation.
//!
//! Owners are local to the invocation. A source description qualifies them with
//! its exact FunctionId and canonical call coordinate. These declarations do not
//! reserve physical nursery room or change guest admission.

use crate::LogicalHeldV1;

/// A native's allocation or scratch occurrence in canonical event order.
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
    Record { field_name_bytes: u64 },
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
/// its immutable descendants. Records retain field-name payload bytes but
/// have no occurrence slot for a field name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeResourceEvent {
    Entity {
        owner: NativeResourceOwner,
        kind: NativeResourceEntity,
        /// None transfers the entity to the exit graph; otherwise it is temporary.
        release: Option<NativeResourceRelease>,
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
    /// Describe an existing input-derived value without allocating an entity.
    pub fn returned(
        args: crate::Args<'_>,
        value: &crate::Value,
    ) -> Result<Self, crate::NativeError> {
        let origin = match value {
            crate::Value::List(_)
            | crate::Value::Map(_)
            | crate::Value::Set(_)
            | crate::Value::Record(_)
            | crate::Value::Closure(_) => NativeResourceOrigin::Alias,
            _ => NativeResourceOrigin::Immutable,
        };
        Ok(Self::value(
            NativeResourceExit::Returned,
            LogicalHeldV1::native_local(args, value)?,
            origin,
        ))
    }

    /// A refusal becomes an immutable error with null data. Kind/message are
    /// payload, rather than separate text occurrences (ErrorRef's rule).
    pub fn refused(kind: &str, message: &str) -> Option<Self> {
        let bytes = (kind.len() as u64)
            .checked_add(message.len() as u64)?
            .checked_add(LogicalHeldV1::REFERENCE.checked_mul(2)?)?;
        Some(Self::value(
            NativeResourceExit::Raised,
            bytes,
            NativeResourceOrigin::Immutable,
        ))
    }

    /// Convert an input-only query's answer to its exit obligation. This must
    /// not be used on the result of executing a native or building its nursery.
    pub fn reading(
        args: crate::Args<'_>,
        answer: Result<crate::Value, crate::NativeError>,
    ) -> Result<Self, crate::NativeError> {
        match answer {
            Ok(value) => Self::returned(args, &value),
            Err(crate::NativeError::Refused { kind, message }) => Self::refused(kind, &message)
                .ok_or(crate::NativeError::Malformed(
                    "native logical cost overflow",
                )),
            Err(error) => Err(error),
        }
    }

    /// A fresh list of input-derived members; existing mutable children remain
    /// aliases. No Rust vector capacity or physical copy enters this contract.
    pub fn list(
        args: crate::Args<'_>,
        members: &[crate::Value],
    ) -> Result<Self, crate::NativeError> {
        let owner = NativeResourceOwner(0);
        Ok(Self {
            exit: NativeResourceExit::Returned,
            output: NativeResourceHolding {
                bytes: LogicalHeldV1::REFERENCE,
                origin: NativeResourceOrigin::Fresh(owner),
            },
            events: vec![NativeResourceEvent::Entity {
                owner,
                kind: NativeResourceEntity::List,
                release: None,
                members: members
                    .iter()
                    .map(|member| Self::returned(args, member).map(|contract| contract.output))
                    .collect::<Result<_, _>>()?,
            }],
        })
    }

    /// A result with no new mutable entities or logical scratch obligation.
    pub fn value(exit: NativeResourceExit, bytes: u64, origin: NativeResourceOrigin) -> Self {
        Self {
            exit,
            events: Vec::new(),
            output: NativeResourceHolding { bytes, origin },
        }
    }

    /// The output occurrence and retained fresh entities, once. Invalid owners,
    /// release schedules, fresh references or arithmetic cannot certify a call.
    pub fn retained_bytes(&self) -> Option<u64> {
        self.balances().map(|(retained, _, _)| retained)
    }

    /// A sufficient envelope including temporary entities and logical scratch.
    /// The output occurrence is conservatively live throughout the invocation.
    /// A failed envelope check resumes exact events; it cannot justify refusal.
    pub fn peak_extra_bytes(&self) -> Option<u64> {
        self.balances().map(|(_, peak, _)| peak)
    }

    /// The peak of simultaneous scratch leases, excluding entity obligations.
    pub fn peak_scratch_bytes(&self) -> Option<u64> {
        self.balances().map(|(_, _, scratch)| scratch)
    }

    fn balances(&self) -> Option<(u64, u64, u64)> {
        if self.output.bytes < LogicalHeldV1::REFERENCE {
            return None;
        }
        let owners: Vec<_> = self.events.iter().map(NativeResourceEvent::owner).collect();
        let mut ends = Vec::with_capacity(owners.len());
        for (at, event) in self.events.iter().enumerate() {
            if owners[..at].contains(&owners[at]) {
                return None;
            }
            let release = match event {
                NativeResourceEvent::Entity { release, .. } => *release,
                NativeResourceEvent::Scratch { release, .. } => Some(*release),
            };
            let end = match release {
                None => self.events.len() + 1,
                Some(NativeResourceRelease::Exit) => self.events.len(),
                Some(NativeResourceRelease::Before(owner)) => {
                    owners.iter().position(|candidate| *candidate == owner)?
                }
            };
            if end <= at {
                return None;
            }
            ends.push(end);
        }
        let target = |holding: &NativeResourceHolding| -> Option<Option<usize>> {
            match holding.origin {
                NativeResourceOrigin::Fresh(owner) => {
                    let at = owners.iter().position(|candidate| *candidate == owner)?;
                    matches!(self.events[at], NativeResourceEvent::Entity { .. })
                        .then_some(Some(at))
                }
                _ => Some(None),
            }
        };
        if let Some(at) = target(&self.output)?
            && ends[at] <= self.events.len()
        {
            return None;
        }
        for (at, event) in self.events.iter().enumerate() {
            if let NativeResourceEvent::Entity { members, .. } = event {
                for member in members {
                    if let Some(child) = target(member)?
                        && (child >= at || ends[child] < ends[at])
                    {
                        return None;
                    }
                }
            }
        }
        let mut release_entities = vec![0_u64; self.events.len() + 1];
        let mut release_scratch = vec![0_u64; self.events.len() + 1];
        let (mut entities, mut scratch) = (0_u64, 0_u64);
        let (mut peak, mut scratch_peak) = (self.output.bytes, 0_u64);
        for (at, event) in self.events.iter().enumerate() {
            entities = entities.checked_sub(release_entities[at])?;
            scratch = scratch.checked_sub(release_scratch[at])?;
            let bytes = event.bytes()?;
            let (live, releases) = match event {
                NativeResourceEvent::Entity { .. } => (&mut entities, &mut release_entities),
                NativeResourceEvent::Scratch { .. } => (&mut scratch, &mut release_scratch),
            };
            *live = live.checked_add(bytes)?;
            if ends[at] <= self.events.len() {
                releases[ends[at]] = releases[ends[at]].checked_add(bytes)?;
            }
            peak = peak.max(
                entities
                    .checked_add(scratch)?
                    .checked_add(self.output.bytes)?,
            );
            scratch_peak = scratch_peak.max(scratch);
        }
        entities = entities.checked_sub(release_entities[self.events.len()])?;
        scratch = scratch.checked_sub(release_scratch[self.events.len()])?;
        if scratch != 0 {
            return None;
        }
        Some((entities.checked_add(self.output.bytes)?, peak, scratch_peak))
    }
}

impl NativeResourceEvent {
    fn bytes(&self) -> Option<u64> {
        match self {
            Self::Scratch { bytes, .. } => Some(*bytes),
            Self::Entity { kind, members, .. } => {
                let names = match kind {
                    NativeResourceEntity::Record { field_name_bytes } => *field_name_bytes,
                    _ => 0,
                };
                members.iter().try_fold(
                    LogicalHeldV1::OBJECT.checked_add(names)?,
                    |bytes, member| bytes.checked_add(member.bytes),
                )
            }
        }
    }

    pub fn owner(&self) -> NativeResourceOwner {
        match self {
            Self::Entity { owner, .. } | Self::Scratch { owner, .. } => *owner,
        }
    }
}
