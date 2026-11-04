//! Project the finite kernel's answers into the compiled source iteration.
use super::*;
use crate::compile::{
    CodeId, LibId, StmtId,
    room::{
        self, Phase, Point,
        placement::{BindingPlacement, DenseActivation},
        region::{
            ScalarRegionValues,
            calls::{
                VerifiedCall,
                scope::{
                    CallScope, ScopedCall,
                    iteration::{IterationEffects, IterationValues, VerifiedIteration},
                },
            },
        },
    },
};
use crate::machine::islands::compiled::recurrence::Recurrence;
use lash_kernel_doc::{TaskId, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{self, Invocation, Ledger, Slot},
    },
};
use std::{collections::BTreeMap, sync::Arc};
mod facts;

#[derive(Clone, Copy)]
enum ValueAt {
    Entry(u32),
    Literal(Value),
    Test,
    Before,
    Scale,
    Reduced,
    Next,
    After,
}
struct Native {
    at: Point,
    lib: LibId,
    arguments: Box<[ValueAt]>,
    result: ValueAt,
}
struct Call {
    call: Arc<VerifiedCall>,
    arguments: Box<[ValueAt]>,
    root: bool,
    result: ValueAt,
    library: Option<LibId>,
}
type CallRoles = Box<[(Slot, ValueAt)]>;

struct ScalarFacts {
    values: Box<[ValueAt]>,
    natives: Box<[usize]>,
}
enum Step {
    Scalar(usize),
    Call(usize),
}
pub(super) struct Targets {
    pub(super) counter: u32,
    pub(super) before: Option<u32>,
    pub(super) reduced: Option<u32>,
    pub(super) after: Option<u32>,
}
pub(in crate::machine::islands) struct Proof {
    placement: Arc<BindingPlacement>,
    pub(in crate::machine::islands) recurrence: Recurrence,
    scalars: Box<[ScalarFacts]>,
    calls: Box<[Call]>,
    roles: Box<[CallRoles]>,
    steps: Box<[Step]>,
    natives: Box<[Native]>,
    pub(in crate::machine::islands) first: Point,
    pub(in crate::machine::islands) through: Point,
    pub(in crate::machine::islands) recovery: [u8; 32],
}
#[derive(Default)]
pub(in crate::machine::islands) struct Values {
    pub(in crate::machine::islands) test: bool,
    pub(in crate::machine::islands) before: Option<Value>,
    pub(in crate::machine::islands) scale: Option<Value>,
    pub(in crate::machine::islands) reduced: Option<Value>,
    pub(in crate::machine::islands) next: Option<Value>,
    pub(in crate::machine::islands) after: Option<Value>,
}
pub(in crate::machine::islands) struct Evaluated<'a> {
    pub(in crate::machine::islands) effects: IterationEffects<'a>,
    pub(in crate::machine::islands) candidates: Vec<ScopedCall<'a>>,
}
struct Projection<'a> {
    exe: &'a Executable,
    placement: &'a BindingPlacement,
    environment: BTreeMap<u32, ValueAt>,
    results: BTreeMap<Point, ValueAt>,
    natives: Vec<Native>,
    calls: Vec<Call>,
}
impl Projection<'_> {
    fn expression(
        &mut self,
        id: StmtId,
        expr: &Expr,
        output: ValueAt,
        root: bool,
    ) -> Option<ValueAt> {
        let expr = unpin(expr);
        let (stream, stmt, _) = self.exe.statement(id);
        let at = Point {
            site: if root {
                stmt.site.clone()
            } else {
                stream
                    .source_sites
                    .get(expr.pc as usize)?
                    .as_deref()?
                    .clone()
            },
            phase: Phase::Commit,
        };
        let value = match &expr.kind {
            Expression::Const(value @ (Value::Int(_) | Value::Float(_))) => {
                if !root {
                    self.placement.verify_literal(&at, *value).ok()?;
                }
                ValueAt::Literal(*value)
            }
            Expression::Local {
                local,
                scoped: false,
                ..
            } if self.placement.unshared(local.at) => {
                if !root {
                    self.placement.verify_read(&at, local.at).ok()?;
                }
                self.environment
                    .get(&local.at)
                    .copied()
                    .unwrap_or(ValueAt::Entry(local.at))
            }
            Expression::Operator { lib, args, .. }
                if matches!(self.exe.lib(*lib).run, LibRun::Native { body: None, .. }) =>
            {
                if root {
                    return None;
                }
                let arguments = args
                    .iter()
                    .map(|arg| self.expression(id, arg, ValueAt::Scale, false))
                    .collect::<Option<Box<[_]>>>()?;
                self.natives.push(Native {
                    at: at.clone(),
                    lib: *lib,
                    arguments,
                    result: output,
                });
                output
            }
            Expression::Operator { args, .. } | Expression::Call { args, .. } => {
                let arguments = if root {
                    args.iter()
                        .map(|arg| match &unpin(arg).kind {
                            Expression::Const(value @ (Value::Int(_) | Value::Float(_))) => {
                                Some(ValueAt::Literal(*value))
                            }
                            Expression::Local {
                                local,
                                scoped: false,
                                ..
                            } if self.placement.unshared(local.at) => Some(
                                self.environment
                                    .get(&local.at)
                                    .copied()
                                    .unwrap_or(ValueAt::Entry(local.at)),
                            ),
                            _ => None,
                        })
                        .collect::<Option<Box<[_]>>>()?
                } else {
                    args.iter()
                        .map(|arg| self.expression(id, arg, ValueAt::Scale, false))
                        .collect::<Option<Box<[_]>>>()?
                };
                let pc = if root { stmt.ops.start } else { expr.pc };
                let first = (stmt.ops.start..pc)
                    .rev()
                    .find(|pc| {
                        let op = stream.ops[*pc as usize];
                        !op.kind().in_region() && !matches!(op, lash_kernel_exec::Op::Region { .. })
                    })
                    .map_or(stmt.ops.start, |pc| pc + 1);
                let call = Arc::new(
                    VerifiedCall::compile_from(self.exe, self.placement.code(), id, pc, first)
                        .ok()?,
                );
                let library = match call.source().owner {
                    lash_kernel_doc::constant_pool::PoolOwner::Function(function) => {
                        self.exe.library_lib(&function)
                    }
                    _ => None,
                };
                self.calls.push(Call {
                    call,
                    arguments,
                    root,
                    result: output,
                    library,
                });
                output
            }
            _ => return None,
        };
        self.results.insert(at, value);
        Some(value)
    }
}
fn owner_slot(source: &room::Source, owner: &room::Owner) -> Option<Slot> {
    match source.owner_slots().ok()?.root(owner, TaskId::MAIN, 0)? {
        logical::Owner::Value { slot, .. } => Some(slot),
        _ => None,
    }
}
impl Proof {
    pub(super) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statement: &syntax::LoopSyntax,
        targets: Targets,
    ) -> Option<Self> {
        let cond = &statement.cond;
        let body = &statement.body;
        let mut projection = Projection {
            exe,
            placement: &placement,
            environment: BTreeMap::new(),
            results: BTreeMap::new(),
            natives: Vec::new(),
            calls: Vec::new(),
        };
        projection.expression(statement.id, cond, ValueAt::Test, false)?;
        for statement in body {
            let (at, _, expr) = write(statement);
            facts::target(&placement, exe, statement.id, at)?;
            let output = if at == targets.counter {
                ValueAt::Next
            } else if Some(at) == targets.before {
                ValueAt::Before
            } else if Some(at) == targets.reduced {
                ValueAt::Reduced
            } else if Some(at) == targets.after {
                ValueAt::After
            } else {
                return None;
            };
            let value = projection.expression(statement.id, expr, output, statement.root)?;
            projection.environment.insert(at, value);
        }
        let descriptors: Vec<_> = projection
            .calls
            .iter()
            .map(|call| Arc::clone(&call.call))
            .collect();
        let attempt =
            VerifiedIteration::compile_loop(exe, placement.code(), statement.id, &descriptors);
        let iteration = attempt.ok()?;
        let source = iteration.source();
        let first = iteration.interval().events().next()?.point.clone();
        let through = iteration.interval().events().last()?.point.clone();
        let mut scalars = Vec::new();
        let mut roles = Vec::new();
        let mut steps = Vec::new();
        let mut live: BTreeMap<_, _> = iteration
            .entries()
            .iter()
            .map(|slot| Some((*slot, ValueAt::Entry(placement.physical(*slot)?))))
            .collect::<Option<_>>()?;
        let mut regions = iteration.scalars().enumerate().peekable();
        let mut calls = projection.calls.iter().enumerate().peekable();
        for event in iteration.interval().events() {
            if let Some((index, call)) = calls.peek()
                && call.call.at() == &event.point
            {
                let mut required = BTreeMap::new();
                for (arg, slot) in call.arguments.iter().zip(call.call.scope_roles()) {
                    if call.root {
                        required.insert(slot, *arg);
                    }
                }
                for slot in call.call.scope_roles() {
                    if let std::collections::btree_map::Entry::Vacant(entry) = required.entry(slot)
                    {
                        entry.insert(*live.get(&slot)?);
                    }
                }
                // Root atom values are authenticated by the call descriptor;
                // nested argument roles already belong to the scalar prefix.
                let argument_slots: Vec<_> =
                    call.call.scope_roles().take(call.arguments.len()).collect();
                for slot in argument_slots {
                    live.remove(&slot);
                }
                live.insert(
                    owner_slot(source, &room::Owner::Operand(call.call.at().site.clone()))?,
                    call.result,
                );
                roles.push(required.into_iter().collect::<Vec<_>>().into_boxed_slice());
                steps.push(Step::Call(*index));
                calls.next();
            } else if let Some((index, region)) = regions.peek()
                && region.interval().events().next()?.point == event.point
            {
                let mut values = vec![ValueAt::Literal(Value::Null); region.fact_count()];
                for (slot, fact) in region.entries() {
                    values[fact.index()] = *live.get(&slot)?;
                }
                for (at, fact) in region.results() {
                    values[fact.index()] = *projection.results.get(at)?;
                }
                let natives = region
                    .interval()
                    .events()
                    .flat_map(|event| {
                        event.peak.measures().filter_map(|measure| match measure {
                            room::Measure::CallPeak {
                                callee: room::CallSource::Native(_),
                                ..
                            } => projection
                                .natives
                                .iter()
                                .position(|native| native.at == event.point),
                            _ => None,
                        })
                    })
                    .collect::<Vec<_>>();
                for (slot, _) in region.entries() {
                    live.remove(&slot);
                }
                let last = region.interval().events().last()?;
                for (slot, fact) in region.recovery_after(&last.point)? {
                    live.insert(*slot, values[fact.index()]);
                }
                scalars.push(ScalarFacts {
                    values: values.into(),
                    natives: natives.into(),
                });
                steps.push(Step::Scalar(*index));
                regions.next();
            }
        }
        if regions.next().is_some() || calls.next().is_some() {
            return None;
        }
        drop(regions);
        drop(calls);
        use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.repeated-region");
        first.site.encode(&mut encoding);
        through.site.encode(&mut encoding);
        let recovery = logical_digest(&encoding.into_bytes());
        let Projection { calls, natives, .. } = projection;
        Some(Self {
            placement,
            recurrence: Recurrence { iteration },
            scalars: scalars.into(),
            calls: calls.into(),
            natives: natives.into(),
            roles: roles.into(),
            steps: steps.into(),
            first,
            through,
            recovery,
        })
    }
    pub(in crate::machine::islands) fn verify(
        &self,
        exe: &Executable,
    ) -> Result<(), room::Refused> {
        for region in self.recurrence.iteration.scalars() {
            region.verify()?;
        }
        for call in &self.calls {
            call.call.verify(exe)?;
        }
        Ok(())
    }
    pub(in crate::machine::islands) fn capture<'a>(
        &self,
        code: CodeId,
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        slots: &'a [crate::machine::SlotState],
    ) -> Result<Option<DenseActivation<'a>>, HeapFault> {
        self.placement
            .capture(code, invocation, ledger, heap, slots)
    }
    pub(in crate::machine::islands) fn scope<'ledger>(
        &self,
        exe: &Executable,
        entry: Option<room::publication::boundary::CallEntry<'_>>,
        ledger: &'ledger Ledger,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        capacity: u64,
    ) -> Option<Option<CallScope<'ledger>>> {
        let Some(call) = self.calls.first() else {
            return Some(None);
        };
        let logical::Owner::Frame { task, .. } = invocation.frame() else {
            return None;
        };
        let width = self.calls.iter().try_fold(0u64, |width, call| {
            width.checked_add(call.call.reservation_width()?)
        })?;
        let capacity = capacity
            .saturating_mul(width)
            .min(u64::MAX - ledger.prospective_invocation(task).id());
        let range = ledger.reserve_invocations(task, capacity)?;
        Some(Some(call.call.bind_scope(
            exe,
            entry?,
            call.call.callee(),
            ledger,
            invocation,
            activation,
            range,
        )?))
    }
    fn value(&self, at: ValueAt, entry: &[(Slot, Value)], actual: &Values) -> Option<Value> {
        match at {
            ValueAt::Entry(at) => entry.iter().find_map(|(slot, value)| {
                (self.placement.physical(*slot) == Some(at)).then_some(*value)
            }),
            ValueAt::Literal(v) => Some(v),
            ValueAt::Test => Some(Value::Bool(actual.test)),
            ValueAt::Before => actual.before,
            ValueAt::Scale => actual.scale,
            ValueAt::Reduced => actual.reduced,
            ValueAt::Next => actual.next,
            ValueAt::After => actual.after,
        }
    }
}
