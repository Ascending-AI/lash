//! Canonical source continuation. Physical placement and authority are rebuilt.
use super::*;
use crate::compile::room::{Phase, Point};
use lash_kernel_state::{kernel_internals::logical::Slot, paged::ChargeDebtRecord};

#[derive(Clone, Debug)]
pub(in crate::machine) struct SourceContinuation {
    pub(in crate::machine) at: Point,
    pub(in crate::machine) unpaid: Point,
    pub(in crate::machine) progress: u32,
    pub(in crate::machine) occurrence: Option<u64>,
    pub(in crate::machine) roles: Vec<(Slot, Value)>,
    pub(in crate::machine) pending: Option<Value>,
    pub(in crate::machine) departure: Option<Point>,
    /// Each exit a running cleanup suspended, outermost first: its actual
    /// origin and its quoted completion debt.
    pub(in crate::machine) suspended: Vec<(Point, u64)>,
    pub(in crate::machine) ordinal: u64,
    pub(in crate::machine) debt: ChargeDebtRecord,
    pub(in crate::machine) root: Site,
    pub(in crate::machine) refusal: Option<u32>,
}

pub(in crate::machine) fn phase(tag: u32) -> Result<Phase, HeapFault> {
    Ok(match tag {
        0 => Phase::Enter,
        1 => Phase::Open,
        2 => Phase::Next,
        3 => Phase::Check,
        4 => Phase::Item,
        5 => Phase::Test,
        6 => Phase::Catch,
        7 => Phase::Suspend,
        8 => Phase::Restore,
        9 => Phase::Supersede,
        10 => Phase::Commit,
        11 => Phase::Fallthrough,
        12 => Phase::Leave,
        13 => Phase::Policy,
        _ => return Err(malformed("unknown canonical source phase")),
    })
}

impl SourceContinuation {
    pub(in crate::machine) fn each_value(&self, visit: &mut dyn FnMut(&Value)) {
        self.roles.iter().for_each(|(_, value)| visit(value));
        if let Some(value) = &self.pending {
            visit(value);
        }
    }

    fn suffix(&self, point: &Point) -> Result<Vec<u32>, HeapFault> {
        if point.site.unit != self.root.unit || !point.site.path.starts_with(&self.root.path) {
            return Err(malformed("source continuation left its defining body"));
        }
        Ok(point.site.path[self.root.path.len()..].to_vec())
    }

    pub(super) fn project(
        &self,
        invocation: u64,
        owners: &crate::compile::room::OwnerSlots,
        arena: &mut Arena,
        access: &Access<'_>,
    ) -> Result<ContinuationRecord, HeapFault> {
        let roles = self
            .roles
            .iter()
            .map(|(slot, value)| {
                // A checkpoint names each role by its full discriminator, so a
                // carry can map its sites and derive the new identity.
                let role = owners
                    .role(*slot)
                    .ok_or_else(|| malformed("a source continuation role has no source role"))?;
                Ok(RoleRecord {
                    coordinate: RoleCoordinate::Source {
                        kind: slot.kind(),
                        role,
                    },
                    value: project(arena, access, value)?,
                })
            })
            .collect::<Result<_, HeapFault>>()?;
        let mut record = ContinuationRecord::new(
            self.suffix(&self.at)?,
            match self.progress {
                3 => ContinuationPhase::Call {
                    phase: self.at.phase as u32,
                },
                4 => ContinuationPhase::Completion {
                    phase: self.at.phase as u32,
                },
                progress => ContinuationPhase::Source {
                    phase: self.at.phase as u32,
                    progress,
                },
            },
            roles,
            access,
        )?
        .with_invocation(invocation)
        .with_occurrence(self.occurrence);
        record.unpaid = Some((self.suffix(&self.unpaid)?, self.unpaid.phase as u32));
        record.pending = self
            .pending
            .as_ref()
            .map(|value| project(arena, access, value))
            .transpose()?;
        record.departure = self
            .departure
            .as_ref()
            .map(|point| Ok::<_, HeapFault>((self.suffix(point)?, point.phase as u32)))
            .transpose()?;
        record.suspended = self
            .suspended
            .iter()
            .map(|(origin, completion)| {
                Ok::<_, HeapFault>(((self.suffix(origin)?, origin.phase as u32), *completion))
            })
            .collect::<Result<_, _>>()?;
        record.ordinal = self.ordinal;
        record.debt = self.debt;
        record.refusal = self.refusal;
        Ok(record)
    }

    pub(super) fn restore(
        record: &ContinuationRecord,
        invocation: u64,
        owners: &crate::compile::room::OwnerSlots,
        root: &Site,
        arena: &mut Arena,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        if invocation == 0 || record.invocation != invocation {
            return Err(malformed(
                "source continuation invocation differs from its frame",
            ));
        }
        let (tag, progress) = match record.phase {
            ContinuationPhase::Source { phase, progress } => (phase, progress),
            ContinuationPhase::Call { phase } => (phase, 3),
            ContinuationPhase::Completion { phase } => (phase, 4),
            _ => return Err(malformed("source continuation has another action phase")),
        };
        if record.refusal.is_some_and(|bound| bound > 2) {
            return Err(malformed("source continuation has an unknown refusal"));
        }
        let point = |path: &[u32], tag| -> Result<Point, HeapFault> {
            let mut site = root.clone();
            site.path.extend_from_slice(path);
            Ok(Point {
                site,
                phase: phase(tag)?,
            })
        };
        let (path, tag_unpaid) = record
            .unpaid
            .as_ref()
            .ok_or_else(|| malformed("source continuation has no unpaid phase"))?;
        let mut roles = Vec::new();
        for role in record.roles(access)? {
            let Some(slot) = role.coordinate.slot() else {
                return Err(malformed(
                    "source continuation holds an action expression role",
                ));
            };
            // The role must name a slot of this exact body (Q4).
            let RoleCoordinate::Source { role: source, .. } = &role.coordinate else {
                unreachable!();
            };
            if owners.role(slot).as_ref() != Some(source) {
                return Err(malformed(
                    "source continuation role is not a role of its body",
                ));
            }
            if roles.iter().any(|(actual, _)| *actual == slot)
                || matches!(slot, Slot::Binding(_) | Slot::Capture(_))
            {
                return Err(malformed("source continuation duplicates a frame role"));
            }
            roles.push((slot, admit(arena, access, &role.value)?));
        }
        Ok(Self {
            at: point(&record.site, tag)?,
            unpaid: point(path, *tag_unpaid)?,
            progress,
            occurrence: record.occurrence,
            roles,
            pending: record
                .pending
                .as_ref()
                .map(|value| admit(arena, access, value))
                .transpose()?,
            departure: record
                .departure
                .as_ref()
                .map(|(path, tag)| point(path, *tag))
                .transpose()?,
            suspended: record
                .suspended
                .iter()
                .map(|((path, tag), completion)| Ok((point(path, *tag)?, *completion)))
                .collect::<Result<_, HeapFault>>()?,
            ordinal: record.ordinal,
            debt: record.debt,
            root: root.clone(),
            refusal: record.refusal,
        })
    }
}
