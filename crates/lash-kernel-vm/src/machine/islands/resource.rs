//! Fixed scalar statement kernels use the source resource compiler. Their
//! interior operand lifetimes have no runtime journal or owner-map entries.
use crate::compile::room::{
    self, ExitKind, Phase, Point, Selection,
    charges::ChargeProgram,
    placement::{BindingPlacement, DenseActivation},
    publication::CompiledExit,
    region::{ScalarRegion, ScalarRegionValues},
};
use crate::compile::{self, Executable, StmtId, Stream, expr};
use crate::machine::SlotState;
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_exec::Op;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{Invocation, Ledger},
    },
};
use std::sync::Arc;

enum Input {
    Literal(Value),
    Binding(u32),
}

pub(in crate::machine::islands) struct CertifiedStatement {
    pub(in crate::machine::islands) id: StmtId,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) source_pc: u32,
    pub(in crate::machine::islands) target: u32,
    placement: Arc<BindingPlacement>,
    input: Input,
    read: Point,
    through: Point,
    region: ScalarRegion,
    pub(in crate::machine::islands) exit: CompiledExit,
    pub(in crate::machine::islands) charges: ChargeProgram,
}

pub(in crate::machine::islands) struct EvaluatedStatement<'a> {
    pub(in crate::machine::islands) activation: DenseActivation,
    pub(in crate::machine::islands) values: Vec<Value>,
    pub(in crate::machine::islands) result: Value,
    pub(in crate::machine::islands) certificate: &'a CertifiedStatement,
}

impl CertifiedStatement {
    pub(in crate::machine::islands) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        id: StmtId,
        index: usize,
    ) -> Option<Self> {
        let (stream, statement, _) = exe.statement(id);
        if exe.stmt_at(placement.code(), &statement.site) != Some(id) {
            return None;
        }
        let last_pc = statement.ops.end().checked_sub(1)?;
        let last = *stream.ops.get(last_pc as usize)?;
        let (target, reg) = match last {
            Op::Bind { local, src } | Op::WriteLocal { local, src }
                if placement.unshared(local.at) =>
            {
                (local.at, src)
            }
            _ => return None,
        };
        let mut parsed = expr::statement(stream, statement.ops, false)?;
        let mut value = parsed.take(reg)?;
        if !parsed.spent() {
            return None;
        }
        while let expr::Kind::Pin(inner) = value.kind {
            value = *inner;
        }
        let read_site = stream
            .source_sites
            .get(value.pc as usize)?
            .as_deref()?
            .clone();
        let read = Point {
            site: read_site,
            phase: Phase::Commit,
        };
        let input = match value.kind {
            expr::Kind::Const(value) if scalar(value) => Input::Literal(value),
            expr::Kind::Local {
                local,
                scoped: false,
                ..
            } if placement.unshared(local.at) => {
                placement.verify_read(&read, local.at).ok()?;
                Input::Binding(local.at)
            }
            _ => return None,
        };
        let source = placement.source();
        let through = Point {
            site: statement.site.clone(),
            phase: Phase::Commit,
        };
        let route = [&read, &through].map(|at| Selection::Event {
            at: at.clone(),
            exit: ExitKind::Normal,
        });
        let interval = source
            .compile_between(&read, &through, &route, route.len())
            .ok()?;
        let region = source.compile_scalar_region(interval).ok()?;
        let output = region
            .interval()
            .events()
            .last()?
            .output
            .clone()
            .unwrap_or_else(|| {
                let mut node = lash_kernel_doc::Node::Block(&source.body);
                for at in &through.site.path[source.site.path.len()..] {
                    node = node.children()[*at as usize];
                }
                let lash_kernel_doc::Node::Stmt(lash_kernel_doc::Stmt::Assign {
                    place: lash_kernel_doc::Place::Variable(name),
                    ..
                }) = node
                else {
                    unreachable!("a checked scalar write has a local target")
                };
                room::Owner::Visible {
                    at: through.site.clone(),
                    name: name.clone(),
                }
            });
        let lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } = source
            .owner_slots()
            .ok()?
            .root(&output, lash_kernel_doc::TaskId::MAIN, 0)?
        else {
            return None;
        };
        if placement.physical(slot) != Some(target) {
            return None;
        }
        let exit = CompiledExit::scalar(&region).ok()?;
        let charges = ChargeProgram::compile(exe, placement.code(), id, statement.ops).ok()?;
        let certificate = Self {
            id,
            index,
            source_pc: statement.ops.start,
            target,
            placement,
            input,
            read,
            through,
            region,
            exit,
            charges,
        };
        certificate.verify(stream).ok()?;
        Some(certificate)
    }

    pub(in crate::machine::islands) fn verify(&self, stream: &Stream) -> Result<(), room::Refused> {
        self.region.verify()?;
        let last = stream
            .ops
            .iter()
            .enumerate()
            .find(|(pc, _)| {
                stream.source_sites.get(*pc).and_then(Option::as_deref) == Some(&self.through.site)
                    && matches!(stream.ops[*pc], Op::Bind { .. } | Op::WriteLocal { .. })
            })
            .map(|(_, op)| *op)
            .ok_or(room::Refused::Source)?;
        if !matches!(last, Op::Bind { local, .. } | Op::WriteLocal { local, .. } if self.placement.unshared(local.at) && local.at == self.target)
        {
            return Err(room::Refused::Source);
        }
        match &self.input {
            Input::Literal(actual) => self.placement.verify_literal(&self.read, *actual),
            Input::Binding(at) => self.placement.verify_read(&self.read, *at),
        }
    }

    pub(in crate::machine::islands) fn evaluate<'a>(
        &'a self,
        code: compile::CodeId,
        slots: &[SlotState],
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
    ) -> Result<Option<EvaluatedStatement<'a>>, HeapFault> {
        let Some(activation) = self
            .placement
            .capture(code, invocation, ledger, heap, slots)?
        else {
            return Ok(None);
        };
        let result = match self.input {
            Input::Literal(value) => value,
            Input::Binding(at) => match slots.get(at as usize) {
                Some(SlotState::Value(value)) if scalar(*value) => *value,
                _ => return Ok(None),
            },
        };
        let mut values = vec![Value::Null; self.region.fact_count()];
        for (slot, fact) in self.region.entries() {
            let Some(value) = activation.value(slot).filter(|value| scalar(*value)) else {
                return Ok(None);
            };
            values[fact.index()] = value;
        }
        for (_, fact) in self.region.results() {
            values[fact.index()] = result;
        }
        Ok(Some(EvaluatedStatement {
            activation,
            values,
            result,
            certificate: self,
        }))
    }
}

impl EvaluatedStatement<'_> {
    pub(in crate::machine::islands) fn bind<'a>(
        &'a self,
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<room::region::BoundScalarRegion<'a>>, HeapFault> {
        self.certificate.region.bind_activation(
            ledger,
            arena,
            heap,
            invocation,
            &self.activation,
            ScalarRegionValues {
                values: &self.values,
                calls: &[],
            },
        )
    }
}

fn scalar(value: Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Absent | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    )
}
