//! Resident read natives publish their actual input and result occurrences.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, Program, PublicationValue as PV, RootValue,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_native_read(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        if !matches!(
            site.direct(self.exe),
            Some(Direct::Length(_, _) | Direct::Item(_) | Direct::Member(_))
        ) || !self.resident_room(0)
            || self.arena.any_cold()
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(ValuePublication::Unavailable);
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(site.pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        if !source.local_binding_transfer(at) {
            return Ok(ValuePublication::Unavailable);
        }
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(op) = stream.ops.get(site.pc as usize) else {
            return Ok(ValuePublication::Unavailable);
        };
        let registers = op.reads();
        if registers.len() != args.len() {
            return Ok(ValuePublication::Unavailable);
        }
        let function = self.exe.lib(site.lib);
        let LibRun::Native { native, .. } = &function.run else {
            return Ok(ValuePublication::Unavailable);
        };
        let room::CallSource::Native(function_id) = function.specialization.resource else {
            return Ok(ValuePublication::Unavailable);
        };
        let contract = match native.resource_contract(doc::Args::new(
            args,
            self.arena.objects(),
            &self.arena.semantics(),
        )) {
            Ok(Some(contract))
                if contract.events.is_empty()
                    && contract.exit == doc::NativeResourceExit::Returned =>
            {
                contract
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(ValuePublication::Unavailable),
        };
        let Some(called) = room::CheckedRoom::native(function.specialization.function, &contract)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let answer = match (site.direct(self.exe), args.first()) {
            (Some(Direct::Length(kind, units)), Some(value)) if value.kind() == kind => value
                .handle()
                .and_then(|handle| self.arena.len(handle).ok())
                .and_then(|len| i64::try_from(len).ok())
                .map(|len| (Value::Int(len), if self.charging { units } else { 0 })),
            (Some(Direct::Item(_)), _) => self.item_answer(site, args),
            (Some(Direct::Member(_)), _) => self.membership(site, args),
            _ => None,
        };
        let Some((value, units)) = answer else {
            return Ok(ValuePublication::Unavailable);
        };
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(input_sites) = registers
            .iter()
            .map(|register| {
                roles
                    .iter()
                    .find(|(reg, _)| reg == register)
                    .map(|(_, at)| (**at).clone())
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("native read has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("native read owners failed verification"))?;
        let root = |at| {
            slots
                .root(
                    &room::Owner::Operand(at),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("native read operand has no source owner"))
        };
        let mut inputs = Vec::new();
        for (at, value) in input_sites.into_iter().zip(args) {
            let owner = root(at)?;
            self.logical
                .replace(owner, Some(SourceValue::Value(*value)));
            inputs.push(owner);
        }
        let output = root(at.site.clone())?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        let released = ledger.release_cost_preserving(
            self.arena,
            self.heap,
            &inputs,
            &[(output, RootValue::Value(&value))],
        )?;
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("native read event disappeared"))?;
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: args
                        .iter()
                        .chain(std::iter::once(&value))
                        .filter_map(Value::handle)
                        .collect(),
                    values: [
                        (Measure::Release(event.inputs.clone()), i128::from(released)),
                        (
                            Measure::NativeScratch {
                                at: at.site.clone(),
                                function: function_id,
                            },
                            0,
                        ),
                    ]
                    .into(),
                    call: Some((at.site.clone(), function.specialization.resource, called)),
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.native-read");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
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
            units: if self.accounting { units } else { 0 },
            close: self.accounting.then_some(site.close).flatten(),
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        let make = || {
            Program::new(vec![JournalEvent {
                inputs: inputs.clone(),
                outputs: vec![(output, PV::Native(value))],
                ..Default::default()
            }])
        };
        drop(ledger);
        let published = self.publish_source(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            make,
            |_, edits| debug_assert!(edits.is_empty()),
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::native_read_published();
        }
        Ok(if published {
            ValuePublication::Published(value)
        } else {
            ValuePublication::Declined
        })
    }
}
