//! The canonical RHS transfer and defining Leave shared by return executors.
use super::*;
use crate::compile::room::{
    Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};
use lash_kernel_state::kernel_internals::logical::{
    FrameEvent, JournalEvent, Ledger, Owner, PublicationValue as PV, RootValue,
};
use lash_kernel_value::{
    sites::Site,
    tokens::{TokenCodec, TokenWriter, logical_digest},
};

pub(in crate::machine) struct ReturnInput<'a> {
    pub(in crate::machine) source: &'a Arc<room::Source>,
    pub(in crate::machine) invocation: &'a Invocation,
    pub(in crate::machine) task: doc::TaskId,
    pub(in crate::machine) at: &'a Site,
    /// A return carries its RHS operand; a break or continue carries none.
    pub(in crate::machine) carried: Option<Carried<'a>>,
    pub(in crate::machine) safe_point: u64,
    pub(in crate::machine) loops: Vec<(Site, u64)>,
}
pub(in crate::machine) struct Carried<'a> {
    pub(in crate::machine) rhs: &'a Site,
    pub(in crate::machine) input: Owner,
    pub(in crate::machine) value: doc::Value,
}
pub(in crate::machine) struct ReturnRecipe {
    pub(in crate::machine) prefix: Prefix,
    pub(in crate::machine) program: Program,
}
impl ReturnInput<'_> {
    pub(in crate::machine) fn prepare(
        self,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<ReturnRecipe>, HeapFault> {
        let source = self.source;
        let at = Point {
            site: self.at.clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_departure_suffix(&at) else {
            return Ok(None);
        };
        if !interval
            .return_loops()
            .ok_or_else(|| malformed("return has no source control proof"))?
            .iter()
            .eq(self.loops.iter().map(|(site, _)| site))
        {
            return Err(malformed(
                "return loop owners differ from its authored controls",
            ));
        }
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("return source owners failed verification"))?;
        let root = |selector| {
            slots
                .root(&selector, self.task, self.invocation.id())
                .ok_or_else(|| malformed("return has no canonical source owner"))
        };
        let departure_selector = room::Owner::ActiveDeparture(source.site.clone());
        let departure = root(departure_selector.clone())?;
        let incoming = Owner::Incoming(self.task);
        if ledger.holding(departure, heap.pages())?.is_some()
            || ledger.holding(incoming, heap.pages())?.is_some()
        {
            return Ok(None);
        }
        let mut retired = std::collections::BTreeSet::new();
        let (departing, commit) = match &self.carried {
            Some(carried) => {
                let input_selector = room::Owner::Operand(carried.rhs.clone());
                let input = root(input_selector.clone())?;
                if input != carried.input {
                    return Err(malformed("return has another source operand"));
                }
                let Some(holding) = ledger.holding(input, heap.pages())? else {
                    return Ok(None);
                };
                let released = ledger.release_cost_preserving(
                    arena,
                    heap,
                    &[input],
                    &[(departure, RootValue::Existing(input))],
                )?;
                retired.insert(input);
                (
                    JournalEvent {
                        inputs: vec![input],
                        outputs: vec![(departure, PV::Existing(input))],
                        ..Default::default()
                    },
                    LeafFacts {
                        dependencies: carried.value.handle().into_iter().collect(),
                        values: [
                            (
                                Measure::Local(departure_selector),
                                i128::from(holding.bytes()),
                            ),
                            (
                                Measure::Release(vec![input_selector].into()),
                                i128::from(released),
                            ),
                        ]
                        .into(),
                        ..Default::default()
                    },
                )
            }
            // The value-less record holds nothing.
            None => (
                JournalEvent {
                    outputs: vec![(departure, PV::Departure)],
                    ..Default::default()
                },
                LeafFacts::default(),
            ),
        };
        let mut events = vec![departing];
        let mut facts = LeafExecution([(at.clone(), commit)].into());
        let mut leave = None;
        for event in interval.events().skip(1) {
            if leave.is_some() {
                return Err(malformed("return suffix has another source departure"));
            }
            // The innermost cleanup holds the departure while it runs.
            if event.point.phase == SourcePhase::Suspend {
                let pending = root(room::Owner::PendingDeparture(event.point.site.clone()))?;
                if ledger.holding(pending, heap.pages())?.is_some() {
                    return Ok(None);
                }
                facts.0.insert(event.point.clone(), LeafFacts::default());
                leave = Some(event.point.clone());
                events.push(JournalEvent {
                    inputs: vec![departure],
                    outputs: vec![(pending, PV::Existing(departure))],
                    ..Default::default()
                });
                continue;
            }
            if event.point.phase != SourcePhase::Leave {
                return Err(malformed("return suffix has another source departure"));
            }
            let groups = vec![
                room::Owner::Scope(event.point.site.clone()),
                room::Owner::Operands(event.point.site.clone()),
            ];
            let mut closing = std::collections::BTreeSet::new();
            for group in &groups {
                for owner in slots
                    .group(group, self.task, self.invocation.id())
                    .ok_or_else(|| malformed("return scope has no canonical owner group"))?
                {
                    if !retired.contains(&owner) && ledger.holding(owner, heap.pages())?.is_some() {
                        closing.insert(owner);
                    }
                }
            }
            let closing = closing.into_iter().collect::<Vec<_>>();
            let preview = Program::new(events.clone());
            let Some(preview) = preview.evaluate(ledger, arena) else {
                return Ok(None);
            };
            let preview = preview
                .prepare(
                    ledger,
                    arena.registration().map_err(native_fault)?,
                    heap,
                    Program::new(events.clone()),
                )
                .map_err(member_fault)?;
            let released = preview.release_cost(&closing, heap.pages())?;
            drop(preview);
            facts.0.insert(
                event.point.clone(),
                LeafFacts {
                    values: [(Measure::Release(groups.into()), i128::from(released))].into(),
                    ..Default::default()
                },
            );
            if event.point.site == source.site {
                leave = Some(event.point.clone());
                events.push(JournalEvent {
                    frame: Some(FrameEvent::Leave {
                        task: self.task,
                        invocation: self.invocation.id(),
                    }),
                    outputs: vec![(incoming, PV::Existing(departure))],
                    ..Default::default()
                });
            } else {
                retired.extend(closing.iter().copied());
                events.push(JournalEvent {
                    inputs: closing,
                    ..Default::default()
                });
            }
        }
        let leave = leave.ok_or_else(|| malformed("return suffix has no defining Leave"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        match &self.carried {
            Some(carried) => {
                encoding.text("lash.source.return-leave");
                at.site.encode(&mut encoding);
                carried.input.encode(&mut encoding);
            }
            None => {
                encoding.text("lash.source.departure-leave");
                at.site.encode(&mut encoding);
                departure.encode(&mut encoding);
            }
        }
        departure.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: self.invocation.id(),
                loops: self.loops,
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at: leave,
            unpaid: interval.resume().cloned(),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point: self.safe_point,
        };
        Ok(Some(ReturnRecipe {
            prefix: Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            program: Program::new(events),
        }))
    }
}
fn member_fault(error: lash_kernel_state::kernel_internals::logical::MemberFault) -> HeapFault {
    match error {
        lash_kernel_state::kernel_internals::logical::MemberFault::Heap(error) => error,
        lash_kernel_state::kernel_internals::logical::MemberFault::Native(error) => {
            native_fault(error)
        }
    }
}
