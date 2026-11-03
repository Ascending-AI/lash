//! Actual source arguments bind one pure callee without a physical frame.
use super::*;
impl VerifiedCall {
    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated call entry supplies values and logical context"
    )]
    pub(super) fn bind_arguments<'a>(
        &'a self,
        exe: &Executable,
        ledger: &Ledger,
        arena: &mut Arena,
        _heap: &PagedHeap,
        caller_invocation: &logical::Invocation,
        arguments: Vec<Value>,
        caller_values: Vec<(Slot, Value)>,
    ) -> Result<Option<BoundCall<'a>>, doc::heap::HeapFault> {
        let Some(EvaluatedArguments {
            result,
            peak,
            delta,
            values,
            caller_formulas,
            body_formulas,
            depth,
            path,
            ..
        }) = self.evaluate_arguments(exe, arena, arguments, &[])?
        else {
            return Ok(None);
        };
        if self.paths[path].region.scalar().is_none() {
            return Ok(None);
        }
        let logical::Owner::Frame { task, .. } = caller_invocation.frame() else {
            return Ok(None);
        };
        let Some(reservation) = ledger.reserve_invocations(task, 1) else {
            return Ok(None);
        };
        Ok(Some(BoundCall {
            call: self,
            stamp: ledger.stamp(),
            caller_invocation: caller_invocation.clone(),
            reservation,
            result,
            peak,
            delta,
            caller_values,
            values,
            path,
            caller_formulas,
            body_formulas,
            inputs: vec![],
            live: vec![(
                slot(&self.caller_source, &Owner::Operand(self.at.site.clone()))
                    .map_err(|_| doc::heap::HeapFault::Malformed("call result role".into()))?,
                result,
                None,
            )],
            depth,
        }))
    }

    pub(super) fn evaluate_arguments(
        &self,
        exe: &Executable,
        arena: &Arena,
        arguments: Vec<Value>,
        captures: &[(Slot, doc::arena::ArenaHandle, Value)],
    ) -> Result<Option<EvaluatedArguments>, doc::heap::HeapFault> {
        if arguments.len() != self.argument_slots.len() {
            return Ok(None);
        }
        for (index, path) in self.paths.iter().enumerate() {
            if let Some(evaluated) =
                self.evaluate_path(exe, arena, &arguments, captures, path, index)?
            {
                return Ok(Some(evaluated));
            }
        }
        Ok(None)
    }

    fn evaluate_path(
        &self,
        exe: &Executable,
        arena: &Arena,
        arguments: &[Value],
        captures: &[(Slot, doc::arena::ArenaHandle, Value)],
        path: &super::paths::BodyPath,
        index: usize,
    ) -> Result<Option<EvaluatedArguments>, doc::heap::HeapFault> {
        let Some(region) = path.region.scalar() else {
            return self.evaluate_nested_path(exe, arena, arguments, path, index);
        };
        let mut values = vec![Value::Null; region.fact_count()];
        let mut prices = vec![0_i128; region.prices];
        let price = |value: &Value, arena: &Arena| -> Result<i128, doc::heap::HeapFault> {
            Ok(i128::from(arena.logical_local(value)?))
        };
        for capture in &region.captures {
            let Some((_, _, value)) = captures.iter().find(|(slot, _, _)| *slot == capture.slot)
            else {
                return Ok(None);
            };
            values[capture.live.fact.index()] = *value;
            prices[capture.live.price] = price(value, arena)?;
        }
        let mut given = 0_i128;
        let mut padded = 0_i128;
        for (index, parameter) in self.parameters.iter().enumerate() {
            let value = arguments.get(index).copied().unwrap_or(Value::Absent);
            if let Some(lib) = self.library {
                let param = &exe.lib(lib).header.signature.params[index];
                if !(param.optional && matches!(value, Value::Absent)
                    || compile::import_result_fits(&param.ty, value))
                {
                    return Ok(None);
                }
            }
            let Some(entry) = region.entries.iter().find(|entry| entry.slot == *parameter) else {
                return Ok(None);
            };
            values[entry.live.fact.index()] = value;
            let amount = price(&value, arena)?;
            prices[entry.live.price] = amount;
            if index < arguments.len() {
                given += amount;
            } else {
                padded += amount;
            }
        }
        let mut computed = BTreeMap::new();
        let mut depth = 1_u64;
        for result in &region.results {
            let value = match result.origin {
                Origin::Literal(value) => value,
                Origin::Read(input) => values[input.fact.index()],
                Origin::Computed => {
                    let Some(native) = path
                        .natives
                        .iter()
                        .find(|native| native.result == result.live.fact)
                    else {
                        return Ok(None);
                    };
                    let Some((input, answer)) =
                        super::paths::evaluate_native(native, exe, arena, &values)?
                    else {
                        return Ok(None);
                    };
                    let function = exe.lib(native.lib);
                    depth = depth.max(1 + u64::from(function.operation()));
                    computed.insert(native.at.clone(), (input, answer));
                    answer
                }
            };
            if !matches!(
                value,
                Value::Int(_)
                    | Value::Float(_)
                    | Value::Bool(_)
                    | Value::Null
                    | Value::Absent
                    | Value::Text(_)
            ) {
                return Ok(None);
            }
            values[result.live.fact.index()] = value;
            prices[result.live.price] = price(&value, arena)?;
            if region.guards.iter().any(|(fact, taken)| {
                *fact == result.live.fact
                    && !matches!(value, Value::Bool(actual) if actual == *taken)
            }) {
                return Ok(None);
            }
        }
        for call in &region.calls {
            // The installed input contracts above prove no scratch or graph
            // allocation for the actual canonical prepared answers.
            prices[call.price] = prices[call.result.price];
        }
        let Some(peak) = region
            .peaks
            .iter()
            .try_fold(padded, |peak, polynomial| {
                Some(peak.max(padded.checked_add(polynomial.evaluate(&prices)?)?))
            })
            .and_then(|peak| u64::try_from(peak.max(0)).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = region
            .delta
            .evaluate(&prices)
            .and_then(|delta| delta.checked_add(padded))
        else {
            return Ok(None);
        };
        let Some((_, result)) = region
            .survivors
            .first()
            .filter(|_| region.survivors.len() == 1)
        else {
            return Ok(None);
        };
        let result = values[result.fact.index()];
        if delta != price(&result, arena)? - given {
            return Ok(None);
        }
        if !self.validate_result(exe, arena, arguments, result)? {
            return Ok(None);
        }
        let caller_formulas = path
            .charges
            .caller()
            .formulas()
            .map(|(at, _, _)| {
                if *at == self.at {
                    Some((arguments.to_vec(), result))
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>();
        let Some(caller_formulas) = caller_formulas else {
            return Ok(None);
        };
        let body_formulas = path
            .charges
            .body()
            .iter()
            .map(|program| {
                program
                    .formulas()
                    .map(|(at, _, _)| computed.get(at).cloned())
                    .collect::<Option<Vec<_>>>()
            })
            .collect::<Option<Vec<_>>>();
        let Some(body_formulas) = body_formulas else {
            return Ok(None);
        };
        Ok(Some(EvaluatedArguments {
            result,
            peak,
            delta,
            recovery: region
                .recovery
                .iter()
                .map(|map| {
                    (
                        map.after.clone(),
                        map.roles
                            .iter()
                            .map(|(slot, fact)| (*slot, values[fact.index()]))
                            .collect(),
                    )
                })
                .collect(),
            values,
            caller_formulas,
            body_formulas,
            depth,
            arguments: arguments.to_vec(),
            nested: Vec::new(),
            path: index,
        }))
    }
    pub(super) fn validate_result(
        &self,
        exe: &Executable,
        arena: &Arena,
        arguments: &[Value],
        result: Value,
    ) -> Result<bool, doc::heap::HeapFault> {
        let price = |value: &Value, arena: &Arena| -> Result<i128, doc::heap::HeapFault> {
            Ok(i128::from(arena.logical_local(value)?))
        };
        if let Some(lib) = self.library
            && !compile::import_result_fits(&exe.lib(lib).header.signature.result, result)
        {
            return Ok(false);
        }
        if let Some(lib) = self.library {
            let function = exe.lib(lib);
            if let LibRun::Native {
                native,
                body: Some(_),
            } = &function.run
            {
                let expected = doc::NativeResourceContract::value(
                    doc::NativeResourceExit::Returned,
                    u64::try_from(price(&result, arena)?).map_err(|_| {
                        doc::heap::HeapFault::Malformed("scalar call holding".into())
                    })?,
                    doc::NativeResourceOrigin::Immutable,
                );
                let actual = function
                    .specialization
                    .prepared
                    .as_ref()
                    .and_then(|(entry, _)| entry.call(arguments));
                if actual.is_none_or(|answer| !answer.same(&result)) {
                    return Ok(false);
                }
                match native.resource_contract(doc::Args::new(
                    arguments,
                    arena.objects(),
                    &arena.semantics(),
                )) {
                    Ok(Some(contract)) if contract == expected => {}
                    Err(doc::NativeError::Host(error)) => return Err(error),
                    Err(doc::NativeError::Malformed(reason)) => {
                        return Err(doc::heap::HeapFault::Malformed(reason.into()));
                    }
                    _ => return Ok(false),
                }
            }
        }
        Ok(true)
    }
}

pub(super) struct EvaluatedArguments {
    pub(super) result: Value,
    pub(super) arguments: Vec<Value>,
    pub(super) recovery: Vec<(Point, Vec<(Slot, Value)>)>,
    pub(super) nested: Vec<super::nested::Evaluated>,
    pub(super) peak: u64,
    pub(super) delta: i128,
    pub(super) values: Vec<Value>,
    pub(super) path: usize,
    pub(super) caller_formulas: Vec<(Vec<Value>, Value)>,
    pub(super) body_formulas: Vec<Vec<(Vec<Value>, Value)>>,
    pub(super) depth: u64,
}
