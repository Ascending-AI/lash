//! Source collections own their normalized graph before storage is installed.
mod storage;
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, Program, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_construction(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &sites::Make,
    ) -> Result<ValuePublication, HeapFault> {
        use crate::compile::expr::Make;
        let bound = match site.make {
            Make::List | Make::Tuple => self.arena.sequence_bound(site.members),
            Make::Map | Make::Set => self.arena.table_bound(site.members),
        };
        if (site.make == Make::Tuple && site.items.is_empty())
            || self.arena.any_cold()
            || !self.resident_room(bound)
        {
            return Ok(ValuePublication::Unavailable);
        }
        self.publish_collection_construction(frame, site.pc, &site.items, site.reg, |values| {
            match site.make {
                Make::List => doc::NewObject::List(values),
                Make::Set => doc::NewObject::Set(values),
                Make::Map => doc::NewObject::Map(
                    values
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|[key, value]| (*key, *value))
                        .collect(),
                ),
                Make::Tuple => doc::NewObject::Tuple(values),
            }
        })
    }

    pub(in crate::machine::islands::execute) fn publish_record_construction(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &sites::Record,
    ) -> Result<ValuePublication, HeapFault> {
        if self.arena.any_cold() || !self.resident_room(self.arena.table_bound(site.members)) {
            return Ok(ValuePublication::Unavailable);
        }
        self.publish_collection_construction(frame, site.pc, &site.values, site.reg, |values| {
            doc::NewObject::Record(site.names.iter().copied().zip(values).collect())
        })
    }

    fn publish_collection_construction(
        &mut self,
        frame: &mut Values<'_, '_>,
        pc: u32,
        items: &[u32],
        reg: u32,
        make: impl FnOnce(Vec<Value>) -> doc::NewObject,
    ) -> Result<ValuePublication, HeapFault> {
        let Some(statement) = frame.state.statement else {
            return Ok(ValuePublication::Unavailable);
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        if !self.source_operand_transfer(frame, at) {
            return Ok(ValuePublication::Unavailable);
        }
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("construction has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("construction source owners failed verification"))?;
        let root = |selector: &room::Owner| {
            slots
                .root(selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("construction operand has no canonical owner"))
        };
        let mut inputs = Vec::new();
        let mut values = Vec::new();
        for reg in items {
            let Some((_, operand)) = roles.iter().find(|(input, _)| input.0 == *reg) else {
                return Ok(ValuePublication::Unavailable);
            };
            inputs.push(root(&room::Owner::Operand((**operand).clone()))?);
            values.push(frame.regs[*reg as usize]);
        }
        let output = root(&room::Owner::Operand(at.site.clone()))?;
        for (reg, operand) in roles.iter() {
            self.logical.replace(
                root(&room::Owner::Operand((**operand).clone()))?,
                Some(SourceValue::Value(frame.regs[reg.ix()])),
            );
        }
        self.logical.flush(self.arena, self.heap)?;
        let event = JournalEvent {
            inputs,
            ..Default::default()
        };
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("construction event disappeared"))?
            .inputs
            .clone();
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: values.iter().filter_map(Value::handle).collect(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let object = make(values);
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.collection-construction");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: frame
                    .state
                    .control
                    .iter()
                    .filter_map(|control| match control {
                        Position::Loop { id, started, .. } => {
                            Some((self.exe.stmt(*id).site.clone(), started.saturating_sub(1)))
                        }
                        _ => None,
                    })
                    .collect(),
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at,
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        drop(ledger);
        let publication_at = start.at.clone();
        let result = &mut frame.regs[reg as usize];
        let published = self.publish_source_prepared(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            || Program::new(vec![event.clone()]),
            storage::Construction {
                object,
                output,
                at: publication_at,
                release,
                ledger: store.clone(),
                heap: self.heap,
                depth_limit: self.limits.value_depth(),
            },
            |arena, _, edits, storage| {
                debug_assert!(edits.is_empty());
                storage.batch.publish(arena);
                *result = storage.value;
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::collection_constructed(frame.regs[reg as usize]);
        }
        Ok(if published {
            ValuePublication::Published(frame.regs[reg as usize])
        } else {
            ValuePublication::Declined
        })
    }
}
