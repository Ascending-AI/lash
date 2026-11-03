//! Bind actual source values and a canonical body shadow, never a consumer fee
//! or body delta. Existing graph bodies stay retained in the interior envelope;
//! the final ledger transaction computes their exact releases once.
use super::*;
use lash_kernel_doc::heap::Identity;
use lash_kernel_doc::{
    NativeError, NativeResourceContract, NativeResourceExit, NativeResourceOrigin,
};

struct NativeValues {
    at: Point,
    function: lash_kernel_value::FunctionId,
    arguments: Vec<Value>,
    result: Value,
}
fn read<T>(value: Result<T, NativeError>) -> Result<Option<T>, HeapFault> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(NativeError::Host(error)) => Err(error),
        Err(NativeError::Malformed(reason)) => Err(HeapFault::Malformed(reason.into())),
        Err(_) => Ok(None),
    }
}
fn address(
    address: &Address,
    activation: &DenseActivation<'_>,
    values: &[Value],
) -> Option<(ArenaHandle, logical::Member)> {
    Some(match address {
        Address::Cell(slot) => match activation.state(*slot)? {
            SlotState::Cell(handle) => (handle, logical::Member::Variable),
            _ => return None,
        },
        Address::Field { target, name } => {
            let Value::Record(handle) = *values.get(*target)? else {
                return None;
            };
            (handle, logical::Member::Field(Arc::clone(name)))
        }
        Address::Index { target, index } => {
            let key = *values.get(*index)?;
            match *values.get(*target)? {
                Value::List(handle) => (
                    handle,
                    logical::Member::List(crate::compile::position(&key)?),
                ),
                Value::Map(handle) => (handle, logical::Member::Table(key)),
                Value::Record(handle) if matches!(key, Value::Text(_) | Value::LongText(_)) => {
                    (handle, logical::Member::Table(key))
                }
                _ => return None,
            }
        }
    })
}
fn member(
    arena: &Arena,
    bodies: &[PreparedMemberBody],
    handle: ArenaHandle,
    member: &logical::Member,
) -> Result<Option<Value>, HeapFault> {
    if !arena.facts(handle).is_ok_and(|facts| !facts.cold) {
        return Ok(None);
    }
    if let Some(body) = bodies.iter().find(|body| body.handle() == handle) {
        return body.read(arena, member);
    }
    Ok(match member {
        logical::Member::Variable => read(arena.cell(handle))?,
        logical::Member::List(index) => read(arena.item(handle, *index))?.flatten(),
        logical::Member::Table(key) => read(arena.table_get(handle, key))?.flatten(),
        logical::Member::Field(name) => {
            read(arena.table_find(handle, &lash_kernel_state::arena::Key::of_text(name)))?
                .flatten()
                .map(|(_, value)| value)
        }
    })
}
/// A computed graph reference may only reach entities retained at entry.
/// No allocation or hidden native graph is elided by the interior envelope.
fn price(
    ledger: &Ledger,
    arena: &mut Arena,
    heap: &PagedHeap,
    value: Value,
    handles: &mut Vec<ArenaHandle>,
) -> Result<Option<u64>, HeapFault> {
    if value
        .handle()
        .is_some_and(|handle| !arena.facts(handle).is_ok_and(|facts| !facts.cold))
    {
        return Ok(None);
    }
    handles.extend(value.handle());
    let holding = Holding::from_native(arena, heap.pages(), &value)?;
    let mut present = true;
    holding.visit_edges(heap.pages(), &mut |target, _| {
        if let Identity::Object(entity) = target {
            present &= ledger.entity_holding(entity, heap.pages())?.is_some();
        }
        Ok(())
    })?;
    Ok(present.then_some(holding.bytes()))
}
impl MemberRegion {
    #[expect(
        clippy::too_many_arguments,
        reason = "actual source activation, graph and selected legal prefix"
    )]
    pub(crate) fn bind_prefix<'p>(
        &'p self,
        exe: &Executable,
        activation: &DenseActivation<'_>,
        invocation: &Invocation,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        count: usize,
    ) -> Result<Option<BoundMembers<'p>>, HeapFault> {
        if exe.local() != self.executable
            || count == 0
            || count > self.statements.len()
            || !activation.current(&self.source, ledger, invocation)
        {
            return Ok(None);
        }
        for slot in &self.vacant {
            if ledger
                .holding(invocation.owner(*slot), heap.pages())?
                .is_some()
            {
                return Ok(None);
            }
        }
        let end = self.statements[count - 1].nodes.end;
        let mut values = Vec::with_capacity(end);
        let mut bodies: Vec<PreparedMemberBody> = Vec::new();
        let mut handles = Vec::new();
        let mut natives = Vec::new();
        let mut facts = Vec::with_capacity(3 * end);
        for node in &self.nodes[..end] {
            let mut member_delta = None;
            let mut known_price = None;
            let value = match &node.operation {
                Operation::Literal(value) => Some(*value),
                Operation::Binding { slot, shared } => match (shared, activation.state(*slot)) {
                    (false, Some(SlotState::Value(value))) => Some(value),
                    (true, Some(SlotState::Cell(handle))) => {
                        handles.push(handle);
                        member(arena, &bodies, handle, &logical::Member::Variable)?
                    }
                    _ => return Ok(None),
                },
                Operation::Read(place) => {
                    let Some((handle, place)) = address(place, activation, &values) else {
                        return Ok(None);
                    };
                    handles.push(handle);
                    member(arena, &bodies, handle, &place)?
                }
                Operation::Native { lib, inputs } => {
                    let function = exe.lib(*lib);
                    let LibRun::Native { native, body: None } = &function.run else {
                        return Ok(None);
                    };
                    let arguments: Vec<_> = inputs.iter().map(|index| values[*index]).collect();
                    if arguments.iter().any(|value| !crate::compile::scalar(value)) {
                        return Ok(None);
                    }
                    let mut padded = arguments.clone();
                    padded.resize(function.arity, Value::Absent);
                    if !function.header.signature.params.iter().zip(&padded).all(
                        |(param, value)| {
                            (param.optional && matches!(value, Value::Absent))
                                || crate::compile::import_result_fits(&param.ty, *value)
                        },
                    ) {
                        return Ok(None);
                    }
                    let Some(contract) = read(native.resource_contract(doc::Args::new(
                        &padded,
                        arena.objects(),
                        &arena.semantics(),
                    )))?
                    .flatten() else {
                        return Ok(None);
                    };
                    let Some((entry, _)) = function.specialization.prepared.as_ref() else {
                        return Ok(None);
                    };
                    let Some(value) = entry.call(&padded).filter(crate::compile::scalar) else {
                        return Ok(None);
                    };
                    if !crate::compile::import_result_fits(&function.header.signature.result, value)
                    {
                        return Ok(None);
                    }
                    let Some(bytes) = price(ledger, arena, heap, value, &mut handles)? else {
                        return Ok(None);
                    };
                    if contract
                        != NativeResourceContract::value(
                            NativeResourceExit::Returned,
                            bytes,
                            NativeResourceOrigin::Immutable,
                        )
                    {
                        return Ok(None);
                    }
                    known_price = Some(bytes);
                    natives.push(NativeValues {
                        at: node.event.point.clone(),
                        function: function.specialization.function,
                        arguments,
                        result: value,
                    });
                    Some(value)
                }
                Operation::Write {
                    address: place,
                    value,
                } => {
                    let value_index = *value;
                    let value = values[value_index];
                    let Some((handle, place)) = address(place, activation, &values) else {
                        return Ok(None);
                    };
                    handles.push(handle);
                    let index = match bodies.iter().position(|body| body.handle() == handle) {
                        Some(index) => index,
                        None => {
                            let Some(body) = arena.prepare_member_body(handle)? else {
                                return Ok(None);
                            };
                            bodies.push(body);
                            bodies.len() - 1
                        }
                    };
                    let old = bodies[index].read(arena, &place)?;
                    let old_bytes = match old {
                        Some(old) => {
                            let Some(bytes) = price(ledger, arena, heap, old, &mut handles)? else {
                                return Ok(None);
                            };
                            bytes
                        }
                        None if matches!(place, logical::Member::List(index) if arena.len(handle).ok() == Some(index)) => {
                            0
                        }
                        None => return Ok(None),
                    };
                    let Ok(bytes) = u64::try_from(facts[3 * value_index]) else {
                        return Ok(None);
                    };
                    if !bodies[index].stage(arena, &place, value)? {
                        return Ok(None);
                    }
                    member_delta = Some(i128::from(bytes) - i128::from(old_bytes));
                    None
                }
            };
            if node.output.is_some() && value.is_none() {
                return Ok(None);
            }
            let bytes = match value {
                Some(_) if known_price.is_some() => known_price.unwrap_or(0),
                Some(value) => {
                    let Some(bytes) = price(ledger, arena, heap, value, &mut handles)? else {
                        return Ok(None);
                    };
                    bytes
                }
                None => 0,
            };
            let delta = member_delta.unwrap_or(0);
            facts.extend([i128::from(bytes), delta, delta.max(0)]);
            values.push(value.unwrap_or(Value::Absent));
        }
        let targets: Vec<_> = bodies.iter().map(PreparedMemberBody::handle).collect();
        let Some(observers) =
            self.observers
                .bind(activation, invocation, ledger, arena, heap, &[], &targets)?
        else {
            return Ok(None);
        };
        if targets
            .iter()
            .any(|target| !observers.permits(&self.source, invocation, ledger, arena, *target))
        {
            return Ok(None);
        }
        let Some(dependencies) = Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        let mut sealed = Vec::with_capacity(bodies.len());
        for body in bodies {
            let Some(body) = body.seal(arena)? else {
                return Ok(None);
            };
            sealed.push(body);
        }
        let mut debts = Vec::with_capacity(count);
        for stmt in &self.statements[..count] {
            let mut formula_values = Vec::new();
            for (at, function, arity) in stmt.charges.formulas() {
                let Some(actual) = natives.iter().find(|native| &native.at == at) else {
                    return Ok(None);
                };
                if actual.function != function || actual.arguments.len() != arity {
                    return Ok(None);
                }
                formula_values.push(FormulaValues {
                    arguments: &actual.arguments,
                    result: actual.result,
                });
            }
            let Some(debt) = stmt.charges.bind(&formula_values) else {
                return Ok(None);
            };
            debts.push(debt);
        }
        let Some(peak) = self.peaks[..2 * end]
            .iter()
            .try_fold(0_i128, |peak, polynomial| {
                Some(peak.max(polynomial.evaluate(&facts)?))
            })
            .and_then(|peak| u64::try_from(peak).ok())
        else {
            return Ok(None);
        };
        let depth = self.statements[..count]
            .iter()
            .map(|stmt| stmt.depth)
            .max()
            .unwrap_or(0);
        Ok(Some(BoundMembers {
            region: self,
            invocation: invocation.clone(),
            count,
            bodies: sealed,
            observers,
            dependencies,
            debts,
            peak,
            depth,
        }))
    }
}
pub(super) fn member_fault(error: logical::MemberFault) -> HeapFault {
    match error {
        logical::MemberFault::Heap(error) => error,
        logical::MemberFault::Native(NativeError::Host(error)) => error,
        logical::MemberFault::Native(error) => fault(&error.to_string()),
    }
}
