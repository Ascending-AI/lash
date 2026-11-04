//! Formal/capture placement is immutable. Actual values remain in C's slots.
use super::*;

#[derive(Clone)]
pub(crate) enum Substitution {
    Role { parameter: Slot, argument: Slot },
    Absent { parameter: Slot },
}
impl Substitution {
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Role {
                    parameter: a,
                    argument: b,
                },
                Self::Role {
                    parameter: c,
                    argument: d,
                },
            ) => a == c && b == d,
            (Self::Absent { parameter: a }, Self::Absent { parameter: b }) => a == b,
            _ => false,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CaptureBinding {
    pub(crate) index: u32,
    pub(crate) slot: Slot,
}

#[derive(Clone)]
pub(crate) struct CalleePlan {
    code: CodeId,
    source: Arc<Source>,
    substitutions: Box<[Substitution]>,
    captures: Box<[CaptureBinding]>,
    enter: Point,
    leave: Point,
    frame_price: u64,
    debt: CallDebt,
    library: Option<LibId>,
}
impl CalleePlan {
    pub(super) fn compile(
        exe: &Executable,
        caller: CodeId,
        target: &Resolved,
    ) -> Result<Self, Refused> {
        // A native-backed body needs A/C's sealed kernel correspondence.
        // Re-running its native entry to check a body answer is forbidden.
        if target
            .library
            .is_some_and(|lib| !matches!(exe.lib(lib).run, LibRun::Body(_)))
        {
            return Err(Refused::Source);
        }
        let body = exe.code(target.code);
        let source = Arc::clone(&body.source_room);
        source.verify()?;
        if !body.free.is_empty()
            || (!target.dynamic && (!body.captures.is_empty() || !body.capture_names.is_empty()))
            || body.capture_names.len() != body.captures.len()
            || source.bindings.iter().any(|binding| {
                binding.borrowed
                    && !(target.dynamic
                        && body.capture_names.contains(&binding.name)
                        && binding.declared == source.site)
            })
            || body.params.len() != source.params.len()
        {
            return Err(Refused::Source);
        }
        let caller_source = &exe.code(caller).source_room;
        let roles = caller_roles(caller_source, target)?;
        let substitutions = source
            .params
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let parameter = body.params.get(index).ok_or(Refused::Source)?;
                let declared = body
                    .slots
                    .get(parameter.slot as usize)
                    .ok_or(Refused::Source)?;
                if parameter.shared
                    || declared.shared
                    || declared.name != *name
                    || declared.declared != source.site
                    || body.slots_by_at.get(parameter.at as usize) != Some(&parameter.slot)
                {
                    return Err(Refused::Source);
                }
                let parameter = slot(
                    &source,
                    &Owner::Binding {
                        declared: source.site.clone(),
                        name: name.clone(),
                    },
                )?;
                if index >= target.arguments {
                    return Ok(Substitution::Absent { parameter });
                }
                Ok(Substitution::Role {
                    parameter,
                    argument: *roles.arguments.get(index).ok_or(Refused::Source)?,
                })
            })
            .collect::<Result<Box<[_]>, Refused>>()?;
        let captures = body
            .captures
            .iter()
            .enumerate()
            .map(|(index, capture)| {
                let declared = body
                    .slots
                    .get(capture.inner as usize)
                    .ok_or(Refused::Source)?;
                if !body.capture_names.contains(&declared.name)
                    || body.slots_by_at.get(capture.inner_at as usize) != Some(&capture.inner)
                {
                    return Err(Refused::Source);
                }
                let slot = slot(
                    &source,
                    &Owner::Binding {
                        declared: declared.declared.clone(),
                        name: declared.name.clone(),
                    },
                )?;
                if !matches!(slot, Slot::Capture(_)) {
                    return Err(Refused::Source);
                }
                Ok(CaptureBinding {
                    index: u32::try_from(index).map_err(|_| Refused::Path)?,
                    slot,
                })
            })
            .collect::<Result<Box<[_]>, Refused>>()?;
        let enter = Point {
            site: source.site.clone(),
            phase: Phase::Enter,
        };
        let leave = Point {
            site: source.site.clone(),
            phase: Phase::Leave,
        };
        let entry = source.room.events.get(&enter).ok_or(Refused::Source)?;
        // Shared parameters allocate cells and therefore end a graph-free
        // lease. The admitted entry must retain exactly its real frame header.
        if entry.peak.fixed != i128::from(doc::LogicalHeldV1::FRAME)
            || entry.delta.fixed != i128::from(doc::LogicalHeldV1::FRAME)
            || entry.peak.measures().next().is_some()
            || entry.delta.measures().next().is_some()
            || source
                .room
                .events
                .get(&leave)
                .ok_or(Refused::Source)?
                .delta
                .fixed
                != -i128::from(doc::LogicalHeldV1::FRAME)
        {
            return Err(Refused::Source);
        }
        let debt = CallDebt::compile(exe, caller, target)?;
        Ok(Self {
            code: target.code,
            source,
            substitutions,
            captures,
            enter,
            leave,
            frame_price: doc::LogicalHeldV1::FRAME,
            debt,
            library: target.library,
        })
    }
    pub(super) fn verify_placement(&self, expected: &Self) -> Result<(), Refused> {
        if self.code != expected.code
            || !Arc::ptr_eq(&self.source, &expected.source)
            || self.enter != expected.enter
            || self.leave != expected.leave
            || self.frame_price != expected.frame_price
            || self.library != expected.library
            || self.captures != expected.captures
            || self.substitutions.len() != expected.substitutions.len()
            || self
                .substitutions
                .iter()
                .zip(&expected.substitutions)
                .any(|(a, b)| !a.same(b))
            || !self.debt.same(&expected.debt)
        {
            return Err(Refused::Source);
        }
        Ok(())
    }
    #[cfg(test)]
    pub(super) fn corrupt_substitution_for_law(&mut self) {
        let (first, rest) = self.substitutions.split_at_mut(1);
        let (Substitution::Role { argument: a, .. }, Substitution::Role { argument: b, .. }) =
            (&mut first[0], &mut rest[0])
        else {
            panic!("law needs two source arguments")
        };
        std::mem::swap(a, b);
    }
    pub(crate) fn code(&self) -> CodeId {
        self.code
    }
    pub(crate) fn source(&self) -> &Arc<Source> {
        &self.source
    }
    pub(crate) fn substitutions(&self) -> &[Substitution] {
        &self.substitutions
    }
    pub(crate) fn captures(&self) -> &[CaptureBinding] {
        &self.captures
    }
    pub(crate) fn enter(&self) -> &Point {
        &self.enter
    }
    pub(crate) fn leave(&self) -> &Point {
        &self.leave
    }
    pub(crate) fn frame_price(&self) -> u64 {
        self.frame_price
    }
    pub(crate) fn debt(&self) -> &CallDebt {
        &self.debt
    }
    /// Arguments are the already evaluated callee slot window.
    pub(crate) fn validate_arguments(&self, exe: &Executable, arguments: &[Value]) -> bool {
        if arguments.len() != self.substitutions.len() {
            return false;
        }
        let Some(lib) = self.library else {
            return true;
        };
        exe.lib(lib)
            .header
            .signature
            .params
            .iter()
            .zip(arguments)
            .all(|(parameter, value)| {
                parameter.optional && matches!(value, Value::Absent)
                    || compile::import_result_fits(&parameter.ty, *value)
            })
    }
    pub(crate) fn validate_result(&self, exe: &Executable, result: Value) -> bool {
        self.library.is_none_or(|lib| {
            compile::import_result_fits(&exe.lib(lib).header.signature.result, result)
        })
    }
}
