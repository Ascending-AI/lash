//! Source fact placement for the finite recurrence. This is a projection of
//! the checked graph, not an evaluator: arithmetic stays in the typed kernel.
use super::*;
use crate::compile::{
    CodeId, LibId, LibRun,
    room::{
        self, ExitKind, Phase, Point, Selection,
        charges::{ChargeProgram, FormulaValues, composition::StatementDebt},
        placement::{BindingPlacement, DenseActivation},
    },
};
use crate::machine::islands::compiled::recurrence::Recurrence;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{Invocation, Ledger},
    },
};
use std::{collections::BTreeMap, sync::Arc};

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
struct ResultFact {
    at: Point,
    value: ValueAt,
    fact: usize,
}
struct Native {
    at: Point,
    lib: LibId,
    arguments: Box<[ValueAt]>,
    result: ValueAt,
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
    results: Box<[ResultFact]>,
    natives: Box<[Native]>,
    charges: Box<[ChargeProgram]>,
    pub(in crate::machine::islands) first: Point,
    pub(in crate::machine::islands) through: Point,
    pub(in crate::machine::islands) depth: u64,
    pub(in crate::machine::islands) recovery: [u8; 32],
    entries: Box<[(u32, usize)]>,
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
    pub(in crate::machine::islands) calls: Vec<room::CheckedRoom>,
    pub(in crate::machine::islands) debts: Vec<StatementDebt<'a>>,
}

struct Projection<'a> {
    exe: &'a Executable,
    placement: &'a BindingPlacement,
    environment: BTreeMap<u32, ValueAt>,
    results: Vec<ResultFact>,
    natives: Vec<Native>,
    selected: Vec<Selection>,
    depth: u64,
}
impl Projection<'_> {
    fn event(&mut self, at: Point) {
        self.selected.push(Selection::Event {
            at,
            exit: ExitKind::Normal,
        });
    }
    fn expression(
        &mut self,
        stream: &crate::compile::Stream,
        expr: &Expr,
        output: ValueAt,
    ) -> Option<ValueAt> {
        let expr = unpin(expr);
        let at = Point {
            site: stream
                .source_sites
                .get(expr.pc as usize)?
                .as_deref()?
                .clone(),
            phase: Phase::Commit,
        };
        let value = match &expr.kind {
            Expression::Const(value @ (Value::Int(_) | Value::Float(_))) => {
                self.placement.verify_literal(&at, *value).ok()?;
                ValueAt::Literal(*value)
            }
            Expression::Local {
                local,
                scoped: false,
                ..
            } if self.placement.unshared(local.at) => {
                self.placement.verify_read(&at, local.at).ok()?;
                self.environment
                    .get(&local.at)
                    .copied()
                    .unwrap_or(ValueAt::Entry(local.at))
            }
            Expression::Operator {
                lib,
                invoke: None,
                args,
                ..
            } => {
                let function = self.exe.lib(*lib);
                if !matches!(function.run, LibRun::Native { body: None, .. })
                    || function.limit.is_some()
                    || function.shares_params
                    || function.arity != args.len()
                    || function.specialization.prepared.as_ref()?.0.depth() != 0
                    || function.specialization.resource
                        != room::CallSource::Native(function.specialization.function)
                {
                    return None;
                }
                let arguments = args
                    .iter()
                    .map(|arg| self.expression(stream, arg, ValueAt::Scale))
                    .collect::<Option<Box<[_]>>>()?;
                self.depth = self.depth.max(u64::from(function.operation()));
                self.natives.push(Native {
                    at: at.clone(),
                    lib: *lib,
                    arguments,
                    result: output,
                });
                output
            }
            _ => return None,
        };
        self.event(at.clone());
        self.results.push(ResultFact {
            at,
            value,
            fact: usize::MAX,
        });
        Some(value)
    }
}
impl Proof {
    pub(super) fn compile(
        exe: &Executable,
        placement: Arc<BindingPlacement>,
        statement: &Statement,
        targets: Targets,
    ) -> Option<Self> {
        let StatementKind::While { cond, body, .. } = &statement.kind else {
            return None;
        };
        let (stream, stmt, _) = exe.statement(statement.id);
        let source = placement.source();
        let mut projection = Projection {
            exe,
            placement: &placement,
            environment: BTreeMap::new(),
            results: Vec::new(),
            natives: Vec::new(),
            selected: Vec::new(),
            depth: 0,
        };
        projection.expression(stream, cond, ValueAt::Test)?;
        projection.event(Point {
            site: stmt.site.clone(),
            phase: Phase::Test,
        });
        projection.selected.push(Selection::Branch {
            at: stmt.site.clone(),
            taken: true,
        });
        let body_site = stmt.site.child(1);
        projection.event(Point {
            site: body_site.clone(),
            phase: Phase::Enter,
        });
        let crate::compile::Form::While { test, .. } = stmt.form else {
            return None;
        };
        let mut charges =
            vec![ChargeProgram::compile(exe, placement.code(), statement.id, test).ok()?];
        for statement in &body.statements {
            let (at, _, expr) = write(statement)?;
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
            let (stream, stmt, _) = exe.statement(statement.id);
            let commit = Point {
                site: stmt.site.clone(),
                phase: Phase::Commit,
            };
            let event = source.compile_event(&commit, ExitKind::Normal).ok()?;
            let mut owner = event.events().next()?.output.clone();
            if owner.is_none() {
                let mut node = lash_kernel_doc::Node::Block(&source.body);
                for index in &stmt.site.path[source.site.path.len()..] {
                    node = *node.children().get(*index as usize)?;
                }
                let lash_kernel_doc::Node::Stmt(lash_kernel_doc::Stmt::Assign {
                    place: lash_kernel_doc::Place::Variable(name),
                    ..
                }) = node
                else {
                    return None;
                };
                owner = Some(room::Owner::Visible {
                    at: stmt.site.clone(),
                    name: name.clone(),
                });
            }
            let lash_kernel_state::kernel_internals::logical::Owner::Value { slot, .. } = source
                .owner_slots()
                .ok()?
                .root(&owner?, lash_kernel_doc::TaskId::MAIN, 0)?
            else {
                return None;
            };
            if placement.physical(slot) != Some(at) || !placement.unshared(at) {
                return None;
            }
            let value = projection.expression(stream, expr, output)?;
            projection.environment.insert(at, value);
            projection.event(Point {
                site: stmt.site.clone(),
                phase: Phase::Commit,
            });
            charges
                .push(ChargeProgram::compile(exe, placement.code(), statement.id, stmt.ops).ok()?);
        }
        let first = projection.results.first()?.at.clone();
        let through = Point {
            site: body_site,
            phase: Phase::Leave,
        };
        projection.event(through.clone());
        let interval = source
            .compile_between(
                &first,
                &through,
                &projection.selected,
                projection.selected.len(),
            )
            .ok()?;
        let recurrence = Recurrence::compile(source, interval, stmt.site.clone()).ok()?;
        let mut canonical = recurrence.region().results();
        for result in &mut projection.results {
            let (point, fact) = canonical.next()?;
            if *point != result.at {
                return None;
            }
            result.fact = fact.index();
        }
        if canonical.next().is_some() {
            return None;
        }
        drop(canonical);
        let entries = recurrence
            .region()
            .entries()
            .map(|(slot, fact)| Some((placement.physical(slot)?, fact.index())))
            .collect::<Option<Box<[_]>>>()?;
        let Projection {
            results,
            natives,
            depth,
            ..
        } = projection;
        use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.repeated-region");
        first.site.encode(&mut encoding);
        through.site.encode(&mut encoding);
        let recovery = logical_digest(&encoding.into_bytes());
        Some(Self {
            recovery,
            placement,
            recurrence,
            results: results.into(),
            natives: natives.into(),
            charges: charges.into(),
            first,
            through,
            depth,
            entries,
        })
    }
    pub(in crate::machine::islands) fn verify(
        &self,
        exe: &Executable,
    ) -> Result<(), room::Refused> {
        self.recurrence.region().verify()?;
        for charge in &self.charges {
            charge.verify(exe, self.placement.code())?;
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
    pub(in crate::machine::islands) fn entry_values(
        &self,
        activation: &DenseActivation<'_>,
    ) -> Option<Vec<Value>> {
        self.recurrence.entry_values(activation)
    }
    fn value(&self, at: ValueAt, values: &[Value], actual: &Values) -> Option<Value> {
        match at {
            ValueAt::Entry(at) => self
                .entries
                .iter()
                .find(|(physical, _)| *physical == at)
                .and_then(|(_, fact)| values.get(*fact))
                .copied(),
            ValueAt::Literal(value) => Some(value),
            ValueAt::Test => Some(Value::Bool(actual.test)),
            ValueAt::Before => actual.before,
            ValueAt::Scale => actual.scale,
            ValueAt::Reduced => actual.reduced,
            ValueAt::Next => actual.next,
            ValueAt::After => actual.after,
        }
    }
    pub(in crate::machine::islands) fn evaluate<'a>(
        &'a self,
        exe: &Executable,
        arena: &Arena,
        actual: &Values,
        values: &mut [Value],
    ) -> Result<Option<Evaluated<'a>>, HeapFault> {
        for result in &self.results {
            let Some(value) = self.value(result.value, values, actual) else {
                return Ok(None);
            };
            values[result.fact] = value;
        }
        let mut facts = Vec::with_capacity(self.natives.len());
        for native in &self.natives {
            let Some(arguments) = native
                .arguments
                .iter()
                .map(|input| self.value(*input, values, actual))
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(None);
            };
            let Some(result) = self.value(native.result, values, actual) else {
                return Ok(None);
            };
            let function = exe.lib(native.lib);
            if !function
                .header
                .signature
                .params
                .iter()
                .zip(&arguments)
                .all(|(param, value)| {
                    (param.optional && matches!(value, Value::Absent))
                        || crate::compile::import_result_fits(&param.ty, *value)
                })
                || !crate::compile::import_result_fits(&function.header.signature.result, result)
                || function
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(entry, _)| entry.call(&arguments))
                    .is_none_or(|answer| !answer.same(&result))
            {
                return Ok(None);
            }
            let LibRun::Native {
                native: implementation,
                ..
            } = &function.run
            else {
                return Ok(None);
            };
            let contract = match implementation.resource_contract(lash_kernel_doc::Args::new(
                &arguments,
                arena.objects(),
                &arena.semantics(),
            )) {
                Ok(Some(contract)) => contract,
                // Preparation is speculative. The source path owns any error
                // and its paid prefix, after admitted iterations publish at
                // their exact backedge.
                _ => return Ok(None),
            };
            if contract
                != lash_kernel_doc::NativeResourceContract::value(
                    lash_kernel_doc::NativeResourceExit::Returned,
                    arena.logical_local(&result)?,
                    lash_kernel_doc::NativeResourceOrigin::Immutable,
                )
            {
                return Ok(None);
            }
            let Some(room) = room::CheckedRoom::native(function.specialization.function, &contract)
            else {
                return Ok(None);
            };
            facts.push((native, arguments, result, Some(room)));
        }
        let mut calls = Vec::new();
        for (point, callee) in self.recurrence.calls() {
            let Some((_, _, _, room)) = facts.iter_mut().find(|(native, ..)| {
                native.at == *point
                    && callee
                        == room::CallSource::Native(exe.lib(native.lib).specialization.function)
            }) else {
                return Ok(None);
            };
            let Some(room) = room.take() else {
                return Ok(None);
            };
            calls.push(room);
        }
        let mut debts = Vec::with_capacity(self.charges.len());
        for program in &self.charges {
            let formulas = program
                .formulas()
                .map(|(point, function, count)| {
                    let (_, arguments, result, _) =
                        facts.iter().find(|(native, arguments, ..)| {
                            native.at == *point
                                && exe.lib(native.lib).specialization.function == function
                                && arguments.len() == count
                        })?;
                    Some(FormulaValues {
                        arguments,
                        result: *result,
                    })
                })
                .collect::<Option<Vec<_>>>();
            let Some(debt) = formulas.and_then(|values| program.bind(&values)) else {
                return Ok(None);
            };
            debts.push(StatementDebt::Scalar(debt));
        }
        Ok(Some(Evaluated { calls, debts }))
    }
}
