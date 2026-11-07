//! Scalar source proofs can join independently prepared canonical storage.
use super::super::{
    charges::{ChargeProgram, FormulaValues},
    publication::{
        Frontier, Position, Successor,
        boundary::{BoundPublication, BoundaryEvidence, BoundaryNeeds, CompiledPublication},
    },
};
use super::*;

impl ScalarRegion {
    pub(crate) fn publication_plan(
        &self,
        exe: &crate::compile::Executable,
        code: crate::compile::CodeId,
        charges: Vec<Arc<ChargeProgram>>,
    ) -> Result<Arc<CompiledPublication>, Refused> {
        self.verify()?;
        if !Arc::ptr_eq(&self.source, &exe.code(code).source_room)
            || self.interval.exit() != ExitKind::Normal
        {
            return Err(Refused::Source);
        }
        let first = &self.interval.events().next().ok_or(Refused::Source)?.point;
        let through = &self.interval.events().last().ok_or(Refused::Source)?.point;
        if charges.first().map(|program| program.first()) != Some(first)
            || charges.last().map(|program| program.through()) != Some(through)
            || charges.windows(2).any(|pair| {
                self.source
                    .exact_successor(pair[0].through(), Successor::Normal)
                    .as_ref()
                    != Ok(pair[1].first())
            })
        {
            return Err(Refused::Source);
        }
        let mut depth = 0;
        for call in &self.calls {
            let CallSource::Native(id) = call.callee else {
                return Err(Refused::Source);
            };
            let lib = exe.library_lib(&id).ok_or(Refused::Source)?;
            depth = depth.max(u64::from(exe.lib(lib).operation()));
        }
        CompiledPublication::compile(
            exe,
            code,
            self.interval
                .events()
                .next()
                .ok_or(Refused::Source)?
                .point
                .clone(),
            self.interval
                .events()
                .last()
                .ok_or(Refused::Source)?
                .point
                .clone(),
            Successor::Normal,
            charges,
            BoundaryNeeds {
                depth,
                ..Default::default()
            },
        )
    }
}
impl BoundScalarRegion<'_> {
    #[expect(
        clippy::too_many_arguments,
        reason = "one checked scalar proof and its source boundary"
    )]
    pub(crate) fn bind_publication(
        self,
        plan: &Arc<CompiledPublication>,
        frontier: &Frontier,
        ledger: &Ledger,
        arena: &Arena,
        heap: &PagedHeap,
        formulas: &[Vec<FormulaValues<'_>>],
        proposed: Position,
    ) -> Result<Option<BoundPublication>, doc::heap::HeapFault> {
        let first = &self
            .region
            .interval
            .events()
            .next()
            .ok_or_else(|| {
                doc::heap::HeapFault::Malformed("scalar publication has no entry".into())
            })?
            .point;
        let through = &self
            .region
            .interval
            .events()
            .last()
            .ok_or_else(|| {
                doc::heap::HeapFault::Malformed("scalar publication has no exit".into())
            })?
            .point;
        if !ledger.is_current(&self.stamp)
            || !plan.authenticates(&self.region.source, first, through)
            || plan.charges().len() != formulas.len()
        {
            return Ok(None);
        }
        let Some(debts) = plan
            .charges()
            .iter()
            .zip(formulas)
            .map(|(program, values)| program.bind(values))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let inputs = self
            .region
            .entries
            .iter()
            .map(|entry| self.invocation.owner(entry.slot))
            .collect();
        let outputs = self
            .region
            .survivors
            .iter()
            .map(|(slot, live)| {
                let value = live.entry.map_or(
                    logical::PublicationValue::Native(self.values[live.fact.0]),
                    |entry| logical::PublicationValue::Existing(self.invocation.owner(entry)),
                );
                (self.invocation.owner(*slot), value)
            })
            .collect();
        let handles: Vec<_> = self.values.iter().filter_map(Value::handle).collect();
        let Some(dependencies) = logical::Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        plan.bind(
            frontier,
            ledger,
            arena,
            heap,
            BoundaryEvidence {
                invocation: self.invocation,
                room: CheckedRoom {
                    owner: self.region.source.owner,
                    native: false,
                    whole: false,
                    native_contract: None,
                    balance: Balance {
                        peak_extra: self.peak,
                        retained_delta: self.delta,
                    },
                    exit: ExitKind::Normal,
                },
                dependencies,
                inputs,
                outputs,
                members: Vec::new(),
            },
            debts,
            proposed,
        )
    }
}
