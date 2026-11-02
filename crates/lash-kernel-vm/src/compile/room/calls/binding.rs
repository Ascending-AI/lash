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
        }) = self.evaluate_arguments(exe, arena, arguments)?
        else {
            return Ok(None);
        };
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
    ) -> Result<Option<EvaluatedArguments>, doc::heap::HeapFault> {
        let mut values = vec![Value::Null; self.region.fact_count()];
        let mut prices = vec![0_i128; self.region.prices];
        let price = |value: &Value, arena: &Arena| -> Result<i128, doc::heap::HeapFault> {
            Ok(i128::from(arena.logical_local(value)?))
        };
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
            let Some(entry) = self
                .region
                .entries
                .iter()
                .find(|entry| entry.slot == *parameter)
            else {
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
        for result in &self.region.results {
            let value = match result.origin {
                Origin::Literal(value) => value,
                Origin::Read(input) => values[input.fact.index()],
                Origin::Computed => {
                    let Some(native) = self
                        .natives
                        .iter()
                        .find(|native| native.result == result.live.fact)
                    else {
                        return Ok(None);
                    };
                    let input: Vec<_> = native
                        .inputs
                        .iter()
                        .map(|fact| values[fact.index()])
                        .collect();
                    let answer = match input.as_slice() {
                        [Value::Int(a), Value::Int(b)] => native
                            .pair
                            .ints
                            .and_then(|op| doc::scalar::ints(op, *a, *b)),
                        [Value::Float(a), Value::Float(b)] => native
                            .pair
                            .floats
                            .and_then(|op| doc::scalar::floats(op, *a, *b)),
                        _ => None,
                    };
                    let Some(answer) = answer else {
                        return Ok(None);
                    };
                    let function = exe.lib(native.lib);
                    let LibRun::Native {
                        native: implementation,
                        ..
                    } = &function.run
                    else {
                        return Ok(None);
                    };
                    if function
                        .specialization
                        .prepared
                        .as_ref()
                        .and_then(|(entry, _)| entry.call(&input))
                        .is_none_or(|actual| !actual.same(&answer))
                    {
                        return Ok(None);
                    }
                    let contract = match implementation.resource_contract(doc::Args::new(
                        &input,
                        arena.objects(),
                        &arena.semantics(),
                    )) {
                        Ok(Some(contract)) => contract,
                        Err(doc::NativeError::Host(error)) => return Err(error),
                        Err(doc::NativeError::Malformed(reason)) => {
                            return Err(doc::heap::HeapFault::Malformed(reason.into()));
                        }
                        _ => return Ok(None),
                    };
                    let expected = doc::NativeResourceContract::value(
                        doc::NativeResourceExit::Returned,
                        u64::try_from(price(&answer, arena)?).map_err(|_| {
                            doc::heap::HeapFault::Malformed("scalar native holding".into())
                        })?,
                        doc::NativeResourceOrigin::Immutable,
                    );
                    if contract != expected {
                        return Ok(None);
                    }
                    depth = depth.max(1 + u64::from(function.operation()));
                    computed.insert(native.at.clone(), (input, answer));
                    answer
                }
            };
            if !matches!(
                value,
                Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
            ) {
                return Ok(None);
            }
            values[result.live.fact.index()] = value;
            prices[result.live.price] = price(&value, arena)?;
        }
        for call in &self.region.calls {
            // The installed input contracts above prove no scratch or graph
            // allocation for the actual canonical prepared answers.
            prices[call.price] = prices[call.result.price];
        }
        let Some(peak) = self
            .region
            .peaks
            .iter()
            .try_fold(padded, |peak, polynomial| {
                Some(peak.max(padded.checked_add(polynomial.evaluate(&prices)?)?))
            })
            .and_then(|peak| u64::try_from(peak.max(0)).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = self
            .region
            .delta
            .evaluate(&prices)
            .and_then(|delta| delta.checked_add(padded))
        else {
            return Ok(None);
        };
        let Some((_, result)) = self
            .region
            .survivors
            .first()
            .filter(|_| self.region.survivors.len() == 1)
        else {
            return Ok(None);
        };
        let result = values[result.fact.index()];
        if delta != price(&result, arena)? - given {
            return Ok(None);
        }
        if let Some(lib) = self.library
            && !compile::import_result_fits(&exe.lib(lib).header.signature.result, result)
        {
            return Ok(None);
        }
        let caller_formulas = self
            .charges
            .caller()
            .formulas()
            .map(|(at, _, _)| {
                if *at == self.at {
                    Some((arguments.clone(), result))
                } else {
                    None
                }
            })
            .collect::<Option<Vec<_>>>();
        let Some(caller_formulas) = caller_formulas else {
            return Ok(None);
        };
        let body_formulas = self
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
            values,
            caller_formulas,
            body_formulas,
            depth,
        }))
    }
}

pub(super) struct EvaluatedArguments {
    pub(super) result: Value,
    pub(super) peak: u64,
    pub(super) delta: i128,
    pub(super) values: Vec<Value>,
    pub(super) caller_formulas: Vec<(Vec<Value>, Value)>,
    pub(super) body_formulas: Vec<Vec<(Vec<Value>, Value)>>,
    pub(super) depth: u64,
}
