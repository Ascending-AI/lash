//! Evaluate source guards and canonical contracts over actual activation values.
use super::*;
use crate::compile::room::placement::DenseActivation;
use lash_kernel_doc::{arena::ArenaHandle, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch, PreparedListWrite},
    kernel_internals::{
        PagedHeap,
        logical::{
            Dependencies, Holding, Invocation, Ledger, Member, MemberEdit, PublicationValue,
            RootValue,
        },
    },
};

pub(crate) struct PreparedStorage {
    pub(super) tuple: Option<PreparedBatch>,
    pub(super) member: Option<PreparedListWrite>,
}
impl PreparedStorage {
    pub(crate) fn publish(self, arena: &mut Arena, mut edits: Vec<MemberEdit>) {
        if let Some(tuple) = self.tuple {
            tuple.publish(arena);
        }
        match self.member {
            Some(member) => {
                assert_eq!(edits.len(), 1, "one verified collection member");
                member.publish(arena, edits.pop().unwrap_or_else(|| unreachable!()));
            }
            None => assert!(edits.is_empty()),
        }
    }
}
struct Called {
    arguments: Vec<Value>,
    result: Value,
    room: CheckedRoom,
    resolved: Option<FunctionId>,
}
pub(super) struct Evaluated<'a> {
    proof: &'a Compiled,
    values: Vec<Value>,
    prices: Vec<u64>,
    present: Vec<bool>,
    calls: BTreeMap<usize, Called>,
    conditions: BTreeMap<Site, bool>,
    member: Option<MemberEdit>,
    member_delta: i128,
}
fn fault(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
fn native_fault(error: doc::NativeError) -> HeapFault {
    match error {
        doc::NativeError::Host(error) => error,
        error => fault(&error.to_string()),
    }
}
fn same_holding(left: &Holding, right: &Holding, heap: &PagedHeap) -> Result<bool, HeapFault> {
    if left.bytes() != right.bytes() {
        return Ok(false);
    }
    let mut expected = BTreeMap::new();
    let mut actual = BTreeMap::new();
    left.visit_edges(heap.pages(), &mut |identity, count| {
        expected.insert(identity, count);
        Ok(())
    })?;
    right.visit_edges(heap.pages(), &mut |identity, count| {
        actual.insert(identity, count);
        Ok(())
    })?;
    Ok(expected == actual)
}
fn price(
    arena: &mut Arena,
    heap: &PagedHeap,
    value: Value,
    tuple: Option<(Value, u64)>,
) -> Result<u64, HeapFault> {
    if let Some((actual, bytes)) = tuple
        && actual.same(&value)
    {
        return Ok(bytes);
    }
    Ok(Holding::from_native(arena, heap.pages(), &value)?.bytes())
}
impl Compiled {
    #[expect(
        clippy::too_many_arguments,
        reason = "actual source activation, ledger and detached tuple"
    )]
    pub(super) fn evaluate<'a>(
        &'a self,
        exe: &Executable,
        activation: &DenseActivation<'_>,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &mut Arena,
        invocation: &Invocation,
        tuple: Option<(Value, u64)>,
    ) -> Result<Option<Evaluated<'a>>, HeapFault> {
        self.evaluate_with_callback(
            exe, activation, ledger, heap, arena, invocation, tuple, None,
        )
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one source activation with an actual scoped callback"
    )]
    pub(super) fn evaluate_with_callback<'a>(
        &'a self,
        exe: &Executable,
        activation: &DenseActivation<'_>,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &mut Arena,
        invocation: &Invocation,
        tuple: Option<(Value, u64)>,
        callback: Option<&crate::compile::room::publication::sort::CallbackEvaluation<'_, '_, '_>>,
    ) -> Result<Option<Evaluated<'a>>, HeapFault> {
        for slot in &self.vacant {
            if ledger
                .holding(invocation.owner(*slot), heap.pages())?
                .is_some()
            {
                return Ok(None);
            }
            if matches!(slot, Slot::Binding(_))
                && !matches!(
                    activation.state(*slot),
                    Some(crate::machine::SlotState::Empty)
                )
            {
                return Ok(None);
            }
        }
        let mut values: Vec<Value> = Vec::with_capacity(self.values.len());
        let mut prices = Vec::with_capacity(self.values.len());
        let mut calls = BTreeMap::new();
        let mut present = Vec::with_capacity(self.values.len());
        for rule in &self.values {
            let mut observed_price = None;
            let value = match rule {
                ValueRule::Entry(slot) | ValueRule::OptionalEntry(slot) => {
                    if matches!(rule, ValueRule::OptionalEntry(_))
                        && matches!(
                            activation.state(*slot),
                            Some(crate::machine::SlotState::Empty)
                        )
                    {
                        if ledger
                            .holding(invocation.owner(*slot), heap.pages())?
                            .is_some()
                        {
                            return Ok(None);
                        }
                        values.push(Value::Absent);
                        prices.push(0);
                        present.push(false);
                        continue;
                    }
                    let Some(value) = activation.value(*slot) else {
                        return Ok(None);
                    };
                    let Some(holding) = ledger.holding(invocation.owner(*slot), heap.pages())?
                    else {
                        return Ok(None);
                    };
                    let actual = Holding::from_native(arena, heap.pages(), &value)?;
                    if !same_holding(&holding, &actual, heap)? {
                        return Ok(None);
                    }
                    observed_price = Some(actual.bytes());
                    value
                }
                ValueRule::Literal(value) => *value,
                ValueRule::Copy(input) => {
                    observed_price = Some(prices[*input]);
                    values[*input]
                }
                ValueRule::EmptyTuple => match tuple {
                    Some((value, _)) => value,
                    None => return Ok(None),
                },
                ValueRule::Item { target, index } => {
                    let Value::List(list) = values[*target] else {
                        return Ok(None);
                    };
                    let Some(index) = crate::compile::position(&values[*index]) else {
                        return Ok(None);
                    };
                    let Some(value) = arena.item(list, index).ok().flatten() else {
                        return Ok(None);
                    };
                    value
                }
                ValueRule::Callback {
                    call,
                    at,
                    arguments,
                    roles,
                } => {
                    let Some(callback) = callback else {
                        return Ok(None);
                    };
                    let arguments: Vec<_> = arguments.iter().map(|at| values[*at]).collect();
                    let roles: Vec<_> = roles
                        .iter()
                        .map(|(slot, at)| (*slot, values[*at]))
                        .collect();
                    let Some(answer) = callback.answer(call, at, arena, &roles, &arguments)? else {
                        return Ok(None);
                    };
                    let result = answer.result;
                    calls.insert(
                        values.len(),
                        Called {
                            arguments,
                            result,
                            room: answer.room,
                            resolved: Some(answer.function),
                        },
                    );
                    result
                }
                ValueRule::Native {
                    library,
                    arguments,
                    after_member,
                } => {
                    let function = exe.lib(*library);
                    let LibRun::Native { native, .. } = &function.run else {
                        return Ok(None);
                    };
                    let args: Vec<_> = arguments.iter().map(|index| values[*index]).collect();
                    if *after_member && args.iter().any(|value| value.handle().is_some()) {
                        return Ok(None);
                    }
                    if !function.header.signature.params.iter().zip(&args).all(
                        |(parameter, value)| {
                            crate::compile::import_result_fits(&parameter.ty, *value)
                        },
                    ) {
                        return Ok(None);
                    }
                    let answer = if function.is_same()
                        && args.len() == 2
                        && tuple.is_some_and(|(empty, _)| empty.same(&args[1]))
                    {
                        Some(Value::Bool(match args[0] {
                            Value::Tuple(handle) => arena.len(handle).ok() == Some(0),
                            _ => false,
                        }))
                    } else if function.length_read().is_some() {
                        match args.as_slice() {
                            [Value::List(list)] => arena
                                .len(*list)
                                .ok()
                                .and_then(|length| i64::try_from(length).ok())
                                .map(Value::Int),
                            _ => None,
                        }
                    } else if function.item_read() == Some(doc::ValueKind::List) {
                        match args.as_slice() {
                            [Value::List(list), index] => crate::compile::position(index)
                                .and_then(|index| arena.item(*list, index).ok().flatten()),
                            _ => None,
                        }
                    } else {
                        function
                            .specialization
                            .native
                            .immediate
                            .and_then(|entry| (entry.entry)(&args))
                    };
                    let Some(answer) = answer else {
                        return Ok(None);
                    };
                    if !crate::compile::import_result_fits(
                        &function.header.signature.result,
                        answer,
                    ) {
                        return Ok(None);
                    }
                    let contract = match native.resource_contract(doc::Args::new(
                        &args,
                        arena.objects(),
                        &arena.semantics(),
                    )) {
                        Ok(Some(contract))
                            if contract.exit == doc::NativeResourceExit::Returned
                                && contract.events.iter().all(|event| {
                                    matches!(event, doc::NativeResourceEvent::Scratch { .. })
                                }) =>
                        {
                            contract
                        }
                        Err(doc::NativeError::Host(error)) => return Err(error),
                        _ => return Ok(None),
                    };
                    let origin = if matches!(
                        answer,
                        Value::List(_)
                            | Value::Map(_)
                            | Value::Set(_)
                            | Value::Record(_)
                            | Value::Closure(_)
                    ) {
                        doc::NativeResourceOrigin::Alias
                    } else {
                        doc::NativeResourceOrigin::Immutable
                    };
                    if contract.output.bytes != price(arena, heap, answer, tuple)?
                        || contract.output.origin != origin
                    {
                        return Ok(None);
                    }
                    let Some(room) =
                        CheckedRoom::native(function.specialization.function, &contract)
                    else {
                        return Ok(None);
                    };
                    calls.insert(
                        values.len(),
                        Called {
                            arguments: args,
                            result: answer,
                            room,
                            resolved: Some(function.specialization.function),
                        },
                    );
                    answer
                }
            };
            prices.push(match observed_price {
                Some(bytes) => bytes,
                None => price(arena, heap, value, tuple)?,
            });
            present.push(true);
            values.push(value);
        }
        let mut conditions = BTreeMap::new();
        let mut member = None;
        let mut member_delta = 0;
        for event in &self.events {
            if let Some(at) = event.condition {
                let Value::Bool(taken) = values[at] else {
                    return Ok(None);
                };
                conditions.insert(event.at.site.clone(), taken);
            }
            if let Some([target, index, rhs]) = event.member {
                let Value::List(handle) = values[target] else {
                    return Ok(None);
                };
                let Some(index) = crate::compile::position(&values[index]) else {
                    return Ok(None);
                };
                let Ok(length) = arena.len(handle) else {
                    return Ok(None);
                };
                if index > length {
                    return Ok(None);
                }
                let old = if index == length {
                    0
                } else {
                    let Some(value) = arena.item(handle, index).ok().flatten() else {
                        return Ok(None);
                    };
                    price(arena, heap, value, tuple)?
                };
                member_delta = i128::from(prices[rhs]) - i128::from(old);
                member = Some(MemberEdit {
                    handle,
                    member: Member::List(index),
                    next: Some(values[rhs]),
                });
            }
        }
        Ok(Some(Evaluated {
            proof: self,
            values,
            prices,
            present,
            calls,
            conditions,
            member,
            member_delta,
        }))
    }
}
struct EventFacts<'a> {
    rule: &'a EventRule,
    evaluation: &'a Evaluated<'a>,
}
impl Facts for EventFacts<'_> {
    fn dependencies(&self) -> Option<&[ArenaHandle]> {
        Some(&[])
    }
    fn measure(&self, measure: &Measure) -> Option<i128> {
        if matches!(measure, Measure::MemberDelta(_)) && self.rule.member.is_some() {
            return Some(self.evaluation.member_delta);
        }
        let (_, indices) = self
            .rule
            .measures
            .iter()
            .find(|(actual, _)| actual == measure)?;
        indices.iter().try_fold(0_i128, |sum, index| {
            sum.checked_add(i128::from(self.evaluation.prices[*index]))
        })
    }
    fn call(&self, at: &Site, source: CallSource) -> Option<&CheckedRoom> {
        let (expected, index) = self.rule.call?;
        (self.rule.at.site == *at && expected == source)
            .then_some(&self.evaluation.calls.get(&index)?.room)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, at: &Site) -> Option<FunctionId> {
        let (_, index) = self.rule.call?;
        (*at == self.rule.at.site).then_some(self.evaluation.calls.get(&index)?.resolved?)
    }
}
impl Execution for &Evaluated<'_> {
    fn event(&mut self, point: &Point) -> Option<Box<dyn Facts + '_>> {
        let rule = self.proof.events.iter().find(|event| event.at == *point)?;
        Some(Box::new(EventFacts {
            rule,
            evaluation: self,
        }))
    }
    fn choose(&mut self, at: &Site) -> Option<bool> {
        self.conditions.get(at).copied()
    }
}

impl CollectionProgram {
    #[cfg(test)]
    pub(crate) fn alias_matches_for_law(
        arena: &mut Arena,
        heap: &PagedHeap,
        first: Value,
        second: Value,
    ) -> Result<bool, HeapFault> {
        assert!(!first.same(&second));
        let first = Holding::from_native(arena, heap.pages(), &first)?;
        let second = Holding::from_native(arena, heap.pages(), &second)?;
        assert_eq!(
            (first.bytes(), second.bytes()),
            (Cost::REFERENCE, Cost::REFERENCE)
        );
        same_holding(&first, &second, heap)
    }
    pub(crate) fn publication_positions(
        &self,
        occurrence: publication::Occurrence,
    ) -> impl Iterator<Item = publication::Position> + '_ {
        self.publications
            .iter()
            .map(move |(_, plan)| plan.exit(occurrence.clone()))
    }
    #[cfg(test)]
    pub(crate) fn publication_start(
        &self,
        exit: &publication::Position,
    ) -> Option<publication::Position> {
        self.publications
            .iter()
            .find(|(_, plan)| plan.exit(exit.occurrence.clone()) == *exit)
            .map(|(_, plan)| plan.start(exit.occurrence.clone()))
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "actual activation, source outcome and shared resource window"
    )]
    pub(crate) fn prepare_publication<'a, 'r>(
        &self,
        exe: &Executable,
        frontier: &publication::Frontier,
        activation: &DenseActivation<'_>,
        invocation: &Invocation,
        ledger: &Ledger,
        heap: &PagedHeap,
        arena: &'a mut Arena,
        result: CollectionResult<'_>,
        resources: crate::admission::SourceResources<'r>,
        memory_bound: u64,
    ) -> Result<
        Option<(
            crate::admission::SourceAccepted<'r, publication::boundary::PreparedPublication<'a>>,
            PreparedStorage,
        )>,
        HeapFault,
    > {
        use publication::boundary::BoundaryEvidence;
        if !activation.current(&self.source, ledger, invocation)
            || !self.matches(result.exit, result.branches)
        {
            return Ok(None);
        }
        let Some(proof) = &self.proof else {
            return Ok(None);
        };
        let Some((_, plan)) = self
            .publications
            .iter()
            .find(|(_, plan)| plan.exit(result.exit.occurrence.clone()) == *result.exit)
        else {
            return Ok(None);
        };
        let (tuple, tuple_measures, mut registration, batch) = if proof
            .values
            .iter()
            .any(|value| matches!(value, ValueRule::EmptyTuple))
        {
            let (value, mut storage) = arena.prepare_empty_tuple().map_err(native_fault)?;
            let page =
                crate::machine::project::project_prepared(&mut storage, heap.pages(), &value)?;
            let bytes = Holding::from_page(heap.pages(), &page)?.bytes();
            let measurements = [
                crate::data::view_size(&storage, &value).map_err(native_fault)?,
                crate::data::view_deep_size(&storage, &value).map_err(native_fault)?,
                crate::data::view_nested_size(&storage, &value).map_err(native_fault)?,
                crate::data::view_magnitude(&storage, &value),
            ];
            let (registration, batch) = storage.into_parts();
            (
                Some((value, bytes)),
                Some((value, measurements)),
                registration,
                Some(batch),
            )
        } else {
            (
                None,
                None,
                arena.registration().map_err(native_fault)?,
                None,
            )
        };
        let arena = registration.arena();
        let Some(evaluation) =
            proof.evaluate(exe, activation, ledger, heap, arena, invocation, tuple)?
        else {
            return Ok(None);
        };
        if result
            .branches
            .iter()
            .any(|(at, taken)| evaluation.conditions.get(at) != Some(taken))
        {
            return Ok(None);
        }
        for (slot, value) in result.writes {
            let Some((_, index)) = proof.survivors.iter().find(|(actual, _)| actual == slot) else {
                return Ok(None);
            };
            if !evaluation.values[*index].same(value) {
                return Ok(None);
            }
        }
        if proof
            .written
            .iter()
            .filter(|slot| proof.survivors.iter().any(|(actual, _)| actual == *slot))
            .any(|slot| {
                result
                    .writes
                    .iter()
                    .filter(|(actual, _)| actual == slot)
                    .count()
                    != 1
            })
            || result
                .clears
                .iter()
                .any(|slot| proof.survivors.iter().any(|(actual, _)| actual == slot))
        {
            return Ok(None);
        }
        let expected_member = evaluation.member.as_ref();
        if !match (result.member, expected_member) {
            (None, None) => true,
            (Some(actual), Some(expected)) => {
                actual.handle == expected.handle
                    && matches!((&actual.member, &expected.member), (Member::List(a), Member::List(b)) if a == b)
                    && actual
                        .next
                        .zip(expected.next)
                        .is_some_and(|(a, b)| a.same(&b))
            }
            _ => false,
        } {
            return Ok(None);
        }
        let room = self.evaluate_checked(
            result.exit,
            result.branches,
            &mut &evaluation,
            |event, _, _| {
                let rule = proof
                    .events
                    .iter()
                    .find(|rule| rule.at == event.point)
                    .ok_or(Refused::Source)?;
                Ok(rule.condition.map(|index| match evaluation.values[index] {
                    Value::Bool(taken) => taken,
                    _ => unreachable!(),
                }))
            },
        );
        let Ok(room) = room else {
            return Ok(None);
        };
        let mut formula_storage = Vec::new();
        for span in &self.charges {
            let values = span
                .program
                .formulas()
                .map(|(at, function, given)| {
                    let rule = proof.events.iter().find(|event| event.at == *at)?;
                    let (CallSource::Native(actual), index) = rule.call? else {
                        return None;
                    };
                    let call = evaluation.calls.get(&index)?;
                    (actual == function && call.arguments.len() == given).then_some(call)
                })
                .collect::<Option<Vec<_>>>();
            let Some(values) = values else {
                return Ok(None);
            };
            formula_storage.push(values);
        }
        let formulas: Vec<Vec<charges::FormulaValues<'_>>> = formula_storage
            .iter()
            .map(|calls| {
                calls
                    .iter()
                    .map(|call| charges::FormulaValues {
                        arguments: &call.arguments,
                        result: call.result,
                    })
                    .collect()
            })
            .collect();
        let Some(debts) = self
            .charges
            .iter()
            .zip(&formulas)
            .enumerate()
            .map(|(span_index, (span, values))| {
                span.program
                    .bind_measured(values, |index, source, measure| {
                        let call = formula_storage[span_index][index];
                        let value = match source {
                            crate::compile::Source::Arg(at) => call.arguments.get(at),
                            crate::compile::Source::Result => Some(&call.result),
                            crate::compile::Source::Nothing => None,
                        };
                        value.map_or(Some(0), |value| {
                            if let Some((tuple, measurements)) = tuple_measures
                                && tuple.same(value)
                            {
                                return Some(
                                    measurements[match measure {
                                        doc::Measure::Size => 0,
                                        doc::Measure::DeepSize => 1,
                                        doc::Measure::NestedSize => 2,
                                        doc::Measure::Magnitude => 3,
                                    }],
                                );
                            }
                            match measure {
                                doc::Measure::Size => crate::data::size(arena, value).ok(),
                                doc::Measure::DeepSize => crate::data::deep_size(arena, value).ok(),
                                doc::Measure::NestedSize => {
                                    crate::data::nested_size(arena, value).ok()
                                }
                                doc::Measure::Magnitude => {
                                    Some(crate::data::magnitude(arena, value))
                                }
                            }
                        })
                    })
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let inputs: Vec<_> = proof
            .entries
            .iter()
            .filter(|(_, index)| evaluation.present[*index])
            .map(|(slot, _)| invocation.owner(*slot))
            .collect();
        let outputs: Vec<_> = proof
            .survivors
            .iter()
            .filter(|(_, index)| evaluation.present[*index])
            .map(|(slot, index)| {
                (
                    invocation.owner(*slot),
                    proof.origin(*index).map_or(
                        PublicationValue::Native(evaluation.values[*index]),
                        |entry| PublicationValue::Existing(invocation.owner(entry)),
                    ),
                )
            })
            .collect();
        let handles: Vec<_> = evaluation
            .values
            .iter()
            .filter_map(Value::handle)
            .filter(|handle| arena.content_version(*handle).is_some())
            .collect();
        let Some(dependencies) = Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        let members: Vec<_> = evaluation.member.clone().into_iter().collect();
        let bound = plan.bind(
            frontier,
            ledger,
            arena,
            heap,
            BoundaryEvidence {
                invocation: invocation.clone(),
                room,
                dependencies,
                inputs: inputs.clone(),
                outputs: outputs.clone(),
                members: members.clone(),
            },
            debts,
            result.exit.clone(),
        )?;
        let Some(bound) = bound else {
            return Ok(None);
        };
        let member = match expected_member {
            Some(edit) => match arena.prepare_list_write(
                edit.handle,
                match edit.member {
                    Member::List(index) => index,
                    _ => unreachable!(),
                },
                edit.next.unwrap_or(Value::Absent),
            )? {
                Some(storage) => Some(storage),
                None => return Ok(None),
            },
            None => None,
        };
        let roots: Vec<_> = outputs
            .iter()
            .map(|(owner, value)| {
                (
                    *owner,
                    match value {
                        PublicationValue::Native(value) => RootValue::Value(value),
                        PublicationValue::Existing(from) => RootValue::Existing(*from),
                        _ => unreachable!(),
                    },
                )
            })
            .collect();
        let transaction = match ledger.prepare_source_members_boundary(
            registration,
            heap,
            &members,
            &inputs,
            &roots,
        ) {
            Ok(transaction) => transaction,
            Err(logical::MemberFault::Heap(error)) => return Err(error),
            Err(logical::MemberFault::Native(doc::NativeError::Host(error))) => return Err(error),
            Err(_) => return Ok(None),
        };
        // No fresh mutable entity is in this source grammar. Equal entity
        // counts prove no interior graph can have died and later reappeared;
        // direct occurrences therefore have their exact canonical release price.
        if transaction.balance()?.entities != ledger.balance()?.entities {
            return Ok(None);
        }
        let Some(accepted) =
            bound.prepare_external(frontier, ledger, heap, transaction, resources, memory_bound)?
        else {
            return Ok(None);
        };
        Ok(Some((
            accepted,
            PreparedStorage {
                tuple: batch,
                member,
            },
        )))
    }
}

/// The same value-flow program serves an ordinary path or an authenticated
/// composed caller route; no guest evaluator or VM-authored room is added.
struct SourceCaller {
    source: Arc<Source>,
    route: Arc<interval::ComposedInterval>,
    proof: Compiled,
}
impl crate::compile::room::publication::sort::CallbackProgram {
    pub(in crate::compile::room) fn compile_caller_values(
        &self,
        exe: &Executable,
        route: &Arc<interval::ComposedInterval>,
    ) -> Result<Arc<dyn crate::compile::room::publication::sort::CallerProof>, Refused> {
        if !route.matches(exe, self.caller_code(), std::slice::from_ref(self.call())) {
            return Err(Refused::Source);
        }
        let source = Arc::clone(&exe.code(self.caller_code()).source_room);
        let proof = Compiled::compile_events(exe, &source, route.events(), Some(self))?;
        Ok(Arc::new(SourceCaller {
            source,
            route: Arc::clone(route),
            proof,
        }))
    }
}
impl crate::compile::room::publication::sort::CallerProof for SourceCaller {
    fn depth(&self) -> u64 {
        self.proof.depth
    }
    fn evaluate(
        &self,
        context: crate::compile::room::publication::sort::CallerContext<'_, '_, '_, '_, '_>,
    ) -> Result<Option<crate::compile::room::publication::sort::CallerFacts>, HeapFault> {
        use crate::compile::room::publication::sort::{CallerContext, CallerFacts};
        let CallerContext {
            exe,
            activation,
            ledger,
            heap,
            arena,
            invocation,
            callback,
        } = context;
        if !activation.current(&self.source, ledger, invocation) {
            return Ok(None);
        }
        let Some(evaluation) = self.proof.evaluate_with_callback(
            exe,
            activation,
            ledger,
            heap,
            arena,
            invocation,
            None,
            Some(callback),
        )?
        else {
            return Ok(None);
        };
        for selection in self.route.selections() {
            if let Selection::Branch { at, taken } = selection
                && evaluation.conditions.get(at) != Some(taken)
            {
                return Ok(None);
            }
        }
        let mut peak = 0_i128;
        let mut balance = 0_i128;
        for event in self.route.events() {
            let Some(rule) = self.proof.events.iter().find(|rule| rule.at == event.point) else {
                return Ok(None);
            };
            // Call entry and return are one authenticated transfer. Root
            // actions first create their source atom roles; the scoped body
            // owns parameter consumption and the returned result. Replaying
            // the ordinary Commit delta would consume those atoms twice.
            if let Some(completion) = self
                .route
                .calls()
                .iter()
                .find(|call| call.entry().0 == &event.point)
            {
                let Some((_, at)) = rule.call else {
                    return Ok(None);
                };
                let Some(called) = evaluation.calls.get(&at) else {
                    return Ok(None);
                };
                let call = completion.call();
                if call.root_arguments().is_some() {
                    let atoms = call.consumed_roles().try_fold(0_i128, |sum, slot| {
                        let ValueRule::Callback { roles, .. } = &self.proof.values[at] else {
                            return None;
                        };
                        let (_, index) = roles.iter().find(|(actual, _)| *actual == slot)?;
                        sum.checked_add(i128::from(evaluation.prices[*index]))
                    });
                    let Some(atoms) = atoms else {
                        return Ok(None);
                    };
                    balance = balance
                        .checked_add(atoms)
                        .ok_or_else(|| fault("callback atom room overflow"))?;
                    peak = peak.max(balance);
                }
                peak = peak.max(
                    balance
                        .checked_add(i128::from(called.room.balance.peak_extra))
                        .ok_or_else(|| fault("callback peak overflow"))?,
                );
                balance = balance
                    .checked_add(called.room.balance.retained_delta)
                    .ok_or_else(|| fault("callback retained room overflow"))?;
                let ValueRule::Callback {
                    arguments, roles, ..
                } = &self.proof.values[at]
                else {
                    return Ok(None);
                };
                if call.root_arguments().is_none() {
                    for slot in call.consumed_roles().skip(arguments.len()) {
                        let Some((_, index)) = roles.iter().find(|(actual, _)| *actual == slot)
                        else {
                            return Ok(None);
                        };
                        balance -= i128::from(evaluation.prices[*index]);
                    }
                }
                if arguments.len() != called.arguments.len() {
                    return Ok(None);
                }
                peak = peak.max(balance);
                continue;
            }
            if self
                .route
                .calls()
                .iter()
                .any(|call| call.resume() == &event.point)
            {
                continue;
            }
            let facts = EventFacts {
                rule,
                evaluation: &evaluation,
            };
            let Some(extra) = event.peak.evaluate(&facts).filter(|extra| *extra >= 0) else {
                return Ok(None);
            };
            let Some(delta) = event.delta.evaluate(&facts) else {
                return Ok(None);
            };
            let Some(interior) = balance.checked_add(extra) else {
                return Ok(None);
            };
            peak = peak.max(interior);
            let Some(next) = balance.checked_add(delta) else {
                return Ok(None);
            };
            balance = next;
            peak = peak.max(balance);
        }
        let Ok(peak_extra) = u64::try_from(peak.max(0)) else {
            return Ok(None);
        };
        let inputs = self
            .proof
            .entries
            .iter()
            .filter(|(_, at)| evaluation.present[*at])
            .map(|(slot, _)| invocation.owner(*slot))
            .collect();
        let outputs = self
            .proof
            .survivors
            .iter()
            .filter(|(_, at)| evaluation.present[*at])
            .map(|(slot, at)| {
                (
                    invocation.owner(*slot),
                    self.proof
                        .origin(*at)
                        .map_or(PublicationValue::Native(evaluation.values[*at]), |slot| {
                            PublicationValue::Existing(invocation.owner(slot))
                        }),
                )
            })
            .collect();
        let values = self
            .proof
            .survivors
            .iter()
            .filter(|(_, at)| evaluation.present[*at])
            .map(|(slot, at)| (*slot, evaluation.values[*at]))
            .collect();
        let handles: Vec<_> = evaluation
            .values
            .iter()
            .filter_map(Value::handle)
            .filter(|h| arena.content_version(*h).is_some())
            .collect();
        let Some(dependencies) = Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        Ok(Some(CallerFacts {
            room: CheckedRoom {
                owner: self.source.owner,
                native: false,
                whole: false,
                native_contract: None,
                balance: Balance {
                    peak_extra,
                    retained_delta: balance,
                },
                exit: ExitKind::Normal,
            },
            dependencies,
            inputs,
            outputs,
            members: evaluation.member.clone().into_iter().collect(),
            values,
            clears: self
                .proof
                .entries
                .iter()
                .map(|(slot, _)| *slot)
                .filter(|slot| {
                    !self
                        .proof
                        .survivors
                        .iter()
                        .any(|(actual, _)| actual == slot)
                })
                .collect(),
            formulas: self
                .proof
                .events
                .iter()
                .filter_map(|rule| {
                    let (source, at) = rule.call?;
                    let call = evaluation.calls.get(&at)?;
                    let function = match source {
                        CallSource::Body(function) | CallSource::Native(function) => function,
                        CallSource::Dynamic => call.resolved?,
                    };
                    Some((
                        rule.at.clone(),
                        (function, call.arguments.clone(), call.result),
                    ))
                })
                .collect(),
        }))
    }
}
