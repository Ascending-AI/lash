//! Fixed scalar statement kernels use the source resource compiler. Their
//! interior operand lifetimes have no runtime journal or owner-map entries.
use crate::compile::room::{
    self, ExitKind, Phase, Point, Selection,
    charges::{ChargeProgram, composition::StatementDebt},
    placement::BindingPlacement,
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

#[path = "resource/expression.rs"]
mod expression;
use expression::{Expression, NativeValues};

pub(crate) struct CertifiedStatement {
    executable: u64,
    pub(in crate::machine::islands) id: StmtId,
    pub(in crate::machine::islands) index: usize,
    pub(in crate::machine::islands) source_pc: u32,
    pub(in crate::machine::islands) target: u32,
    placement: Arc<BindingPlacement>,
    expression: Expression,
    through: Point,
    region: ScalarRegion,
    pub(in crate::machine::islands) exit: CompiledExit,
    pub(in crate::machine::islands) charges: ChargeProgram,
}

pub(in crate::machine::islands) struct EvaluatedStatement<'a> {
    pub(in crate::machine::islands) result: Value,
    pub(in crate::machine::islands) bound: room::region::BoundScalarRegion<'a>,
    pub(in crate::machine::islands) debt: StatementDebt<'a>,
    pub(in crate::machine::islands) prepared_natives: usize,
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
        let value = parsed.take(reg)?;
        if !parsed.spent() {
            return None;
        }
        let source = placement.source();
        let through = Point {
            site: statement.site.clone(),
            phase: Phase::Commit,
        };
        let (expression, region, charges, exit) = {
            let mut expression = Expression::compile(exe, stream, &placement, &value)?;
            let route: Vec<_> = expression
                .points()
                .chain(std::iter::once(&through))
                .map(|at| Selection::Event {
                    at: at.clone(),
                    exit: ExitKind::Normal,
                })
                .collect();
            let interval = source
                .compile_between(expression.first(), &through, &route, route.len())
                .ok()?;
            let region = source.compile_scalar_region(interval).ok()?;
            expression.bind_facts(exe, &region)?;
            let exit = CompiledExit::scalar(exe, &region).ok()?;
            let charges = ChargeProgram::compile(exe, placement.code(), id, statement.ops).ok()?;
            (expression, region, charges, exit)
        };
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
        let certificate = Self {
            executable: exe.local(),
            id,
            index,
            source_pc: statement.ops.start,
            target,
            placement,
            expression,
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
        self.expression.verify(stream, &self.placement)
    }

    pub(in crate::machine::islands) fn evaluate<'a>(
        &'a self,
        exe: &Executable,
        (code, slots): (compile::CodeId, &[SlotState]),
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &mut Arena,
    ) -> Result<Option<EvaluatedStatement<'a>>, HeapFault> {
        if exe.local() != self.executable {
            return Ok(None);
        }
        let Some(activation) = self
            .placement
            .capture(code, invocation, ledger, heap, slots)?
        else {
            return Ok(None);
        };
        let expression = &self.expression;
        let mut values = entry_values(&self.region, &activation)?;
        if values.len() != self.region.fact_count() {
            return Ok(None);
        }
        let Some(evaluated) = expression.evaluate(exe, slots, arena, heap, &mut values)? else {
            return Ok(None);
        };
        let Some(debt) = bind_debt(&self.charges, &evaluated.formulas) else {
            return Ok(None);
        };
        let Some(bound) = self.region.bind_activation(
            ledger,
            arena,
            heap,
            invocation,
            &activation,
            ScalarRegionValues {
                values: &values,
                calls: &evaluated.calls,
            },
        )?
        else {
            return Ok(None);
        };
        Ok(Some(EvaluatedStatement {
            result: evaluated.result,
            bound,
            debt: StatementDebt::Scalar(debt),
            prepared_natives: evaluated.formulas.len(),
        }))
    }
}

fn entry_values(
    region: &ScalarRegion,
    activation: &room::placement::DenseActivation,
) -> Result<Vec<Value>, HeapFault> {
    let mut values = vec![Value::Null; region.fact_count()];
    for (slot, fact) in region.entries() {
        let Some(value) = activation.value(slot).filter(|value| scalar(*value)) else {
            return Ok(vec![]);
        };
        values[fact.index()] = value;
    }
    Ok(values)
}
fn bind_debt<'a>(
    charges: &'a ChargeProgram,
    formulas: &[NativeValues],
) -> Option<room::charges::ChargeDebt<'a>> {
    let values = charges
        .formulas()
        .map(|(at, function, count)| {
            let native = formulas
                .iter()
                .find(|native| native.at == *at && native.function == function)?;
            (native.arguments.len() == count).then_some(room::charges::FormulaValues {
                arguments: &native.arguments,
                result: native.result,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    charges.bind(&values)
}

fn scalar(value: Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Absent | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    )
}
