//! Canonical immediate entries compute a scalar expression's values. Its
//! source certificate owns the lifetimes, peak and one final binding transfer.
use super::*;
use crate::compile::{LibId, LibRun};
use room::region::{ScalarFact, ScalarRegion};

enum Operation {
    Literal(Value),
    Binding(u32),
    Native { lib: LibId, inputs: Box<[usize]> },
}

struct Result {
    at: Point,
    pc: u32,
    reg: u32,
    operation: Operation,
    fact: Option<ScalarFact>,
    room: bool,
}

pub(super) struct Expression {
    results: Box<[Result]>,
}

pub(super) struct NativeValues {
    pub(super) at: Point,
    pub(super) function: lash_kernel_value::FunctionId,
    pub(super) arguments: Vec<Value>,
    pub(super) result: Value,
}

pub(super) struct Evaluated {
    pub(super) result: Value,
    pub(super) calls: Vec<room::CheckedRoom>,
    pub(super) formulas: Vec<NativeValues>,
}

impl Expression {
    pub(super) fn compile(
        exe: &Executable,
        stream: &Stream,
        placement: &BindingPlacement,
        expr: &expr::Expr,
    ) -> Option<Self> {
        fn add(
            results: &mut Vec<Result>,
            exe: &Executable,
            stream: &Stream,
            placement: &BindingPlacement,
            expr: &expr::Expr,
        ) -> Option<usize> {
            if let expr::Kind::Pin(inner) = &expr.kind {
                return add(results, exe, stream, placement, inner);
            }
            let at = Point {
                site: stream
                    .source_sites
                    .get(expr.pc as usize)?
                    .as_deref()?
                    .clone(),
                phase: Phase::Commit,
            };
            let operation = match &expr.kind {
                expr::Kind::Const(value) if scalar(*value) => {
                    placement.verify_literal(&at, *value).ok()?;
                    Operation::Literal(*value)
                }
                expr::Kind::Local {
                    local,
                    scoped: false,
                    ..
                } if placement.unshared(local.at) => {
                    placement.verify_read(&at, local.at).ok()?;
                    Operation::Binding(local.at)
                }
                expr::Kind::Operator {
                    lib,
                    invoke: None,
                    args,
                    ..
                }
                | expr::Kind::Native { lib, args, .. } => {
                    let function = exe.lib(*lib);
                    let LibRun::Native { body: None, .. } = function.run else {
                        return None;
                    };
                    if function.limit.is_some()
                        || function.shares_params
                        || function.arity != args.len()
                        || function.specialization.prepared.as_ref()?.0.depth() != 0
                        || function.specialization.resource
                            != room::CallSource::Native(function.specialization.function)
                    {
                        return None;
                    }
                    let inputs = args
                        .iter()
                        .map(|arg| add(results, exe, stream, placement, arg))
                        .collect::<Option<Box<[_]>>>()?;
                    Operation::Native { lib: *lib, inputs }
                }
                _ => return None,
            };
            let index = results.len();
            results.push(Result {
                at,
                pc: expr.pc,
                reg: expr.reg,
                operation,
                fact: None,
                room: false,
            });
            Some(index)
        }
        let mut results = Vec::new();
        add(&mut results, exe, stream, placement, expr)?;
        Some(Self {
            results: results.into(),
        })
    }

    pub(super) fn first(&self) -> &Point {
        &self.results[0].at
    }

    pub(super) fn points(&self) -> impl Iterator<Item = &Point> {
        self.results.iter().map(|result| &result.at)
    }

    pub(super) fn bind_facts(&mut self, exe: &Executable, region: &ScalarRegion) -> Option<()> {
        let mut facts = region.results();
        for result in &mut self.results {
            let (at, fact) = facts.next()?;
            if *at != result.at {
                return None;
            }
            result.fact = Some(fact);
            if let Operation::Native { lib, .. } = result.operation {
                let function = exe.lib(lib).specialization.function;
                let event = region
                    .interval()
                    .events()
                    .find(|event| event.point == result.at)?;
                result.room = event.peak.measures().any(|measure| {
                    matches!(measure, room::Measure::CallPeak { at, callee: room::CallSource::Native(id) }
                        if *at == result.at.site && *id == function)
                });
            }
        }
        facts.next().is_none().then_some(())
    }

    pub(super) fn verify(
        &self,
        stream: &Stream,
        placement: &BindingPlacement,
    ) -> std::result::Result<(), room::Refused> {
        for (index, result) in self.results.iter().enumerate() {
            if stream
                .source_sites
                .get(result.pc as usize)
                .and_then(Option::as_deref)
                != Some(&result.at.site)
            {
                return Err(room::Refused::Source);
            }
            match &result.operation {
                Operation::Literal(value) => placement.verify_literal(&result.at, *value)?,
                Operation::Binding(at) => placement.verify_read(&result.at, *at)?,
                Operation::Native { lib, inputs } => {
                    if !matches!(stream.ops.get(result.pc as usize),
                        Some(Op::Operator { lib: actual, args } | Op::Call { lib: actual, args })
                        if actual.0 == lib.0 && args.len as usize == inputs.len())
                    {
                        return Err(room::Refused::Source);
                    }
                    let registers = stream.ops[result.pc as usize].reads();
                    if inputs.iter().zip(registers).any(|(input, register)| {
                        *input >= index || self.results[*input].reg != register.0
                    }) {
                        return Err(room::Refused::Source);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn evaluate(
        &self,
        exe: &Executable,
        slots: &[SlotState],
        arena: &mut Arena,
        heap: &PagedHeap,
        facts: &mut [Value],
    ) -> std::result::Result<Option<Evaluated>, HeapFault> {
        let mut values = Vec::with_capacity(self.results.len());
        let mut calls = Vec::new();
        let mut formulas = Vec::new();
        for result in &self.results {
            let value = match &result.operation {
                Operation::Literal(value) => *value,
                Operation::Binding(at) => match slots.get(*at as usize) {
                    Some(SlotState::Value(value)) if scalar(*value) => *value,
                    _ => return Ok(None),
                },
                Operation::Native { lib, inputs } => {
                    let function = exe.lib(*lib);
                    let LibRun::Native { native, .. } = &function.run else {
                        return Ok(None);
                    };
                    let arguments: Vec<_> = inputs.iter().map(|at| values[*at]).collect();
                    if !function.header.signature.params.iter().zip(&arguments).all(
                        |(param, value)| {
                            (param.optional && matches!(value, Value::Absent))
                                || compile::import_result_fits(&param.ty, *value)
                        },
                    ) {
                        return Ok(None);
                    }
                    let Some((entry, _)) = function.specialization.prepared.as_ref() else {
                        return Ok(None);
                    };
                    let Some(value) = entry.call(&arguments).filter(|value| scalar(*value)) else {
                        return Ok(None);
                    };
                    if !compile::import_result_fits(&function.header.signature.result, value) {
                        return Ok(None);
                    }
                    // A pair or prepared entry says nothing about scratch. Only
                    // the installed implementation's input contract proves it.
                    let contract = match native.resource_contract(lash_kernel_doc::Args::new(
                        &arguments,
                        arena.objects(),
                        &arena.semantics(),
                    )) {
                        Ok(Some(contract)) => contract,
                        Err(lash_kernel_doc::NativeError::Host(error)) => return Err(error),
                        Err(lash_kernel_doc::NativeError::Malformed(reason)) => {
                            return Err(HeapFault::Malformed(reason.into()));
                        }
                        _ => return Ok(None),
                    };
                    let bytes = lash_kernel_state::kernel_internals::logical::Holding::from_native(
                        arena,
                        heap.pages(),
                        &value,
                    )?
                    .bytes();
                    if contract
                        != lash_kernel_doc::NativeResourceContract::value(
                            lash_kernel_doc::NativeResourceExit::Returned,
                            bytes,
                            lash_kernel_doc::NativeResourceOrigin::Immutable,
                        )
                    {
                        return Ok(None);
                    }
                    if result.room {
                        let Some(room) =
                            room::CheckedRoom::native(function.specialization.function, &contract)
                        else {
                            return Ok(None);
                        };
                        calls.push(room);
                    }
                    formulas.push(NativeValues {
                        at: result.at.clone(),
                        function: function.specialization.function,
                        arguments,
                        result: value,
                    });
                    value
                }
            };
            let Some(fact) = result.fact else {
                return Ok(None);
            };
            facts[fact.index()] = value;
            values.push(value);
        }
        Ok(values.last().copied().map(|result| Evaluated {
            result,
            calls,
            formulas,
        }))
    }
}
