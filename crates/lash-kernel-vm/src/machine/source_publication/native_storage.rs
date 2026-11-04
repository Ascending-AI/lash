//! Native storage, input/result facts and ordered scratch share one preparation.
use super::*;
use crate::compile::LibRun;
use doc::Value;
use lash_kernel_state::arena::{PreparedBatch, PreparedValueProof, Registration};
use lash_kernel_state::kernel_internals::logical::Ledger;
use lash_kernel_state::kernel_internals::logical::{
    BoundaryTransaction, Dependencies, Owner, PublicationValue as PV, VirtualEntity,
};
use room::{Measure, Point};

pub(in crate::machine) struct NativeStorage<'a> {
    pub(in crate::machine) contract: doc::NativeResourceContract,
    pub(in crate::machine) function: &'a crate::compile::Lib,
    pub(in crate::machine) args: Vec<Value>,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) heap: &'a PagedHeap,
    pub(in crate::machine) charging: bool,
    pub(in crate::machine) limits: lash_kernel_value::data_limits::KernelDataLimits,
    pub(in crate::machine) room: Option<u64>,
    /// Only the checked prepared entry's actual scalar answer is supplied.
    pub(in crate::machine) known: Option<(Value, u32)>,
}
pub(in crate::machine) struct PreparedNative {
    pub(in crate::machine) value: Value,
    pub(in crate::machine) work: u64,
    pub(in crate::machine) known: bool,
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) allocated: bool,
    graph_entities: usize,
    output: Owner,
    contract: doc::NativeResourceContract,
    function: lash_kernel_value::FunctionId,
    arguments: Vec<Value>,
    given: usize,
    measurements: Vec<(crate::compile::Source, doc::Measure, u64)>,
    depth: u32,
    at: Point,
    release: Box<[room::Owner]>,
    released: u64,
}
struct DetachedNative<'a> {
    registration: Registration<'a>,
    storage: PreparedNative,
    proof: PreparedValueProof,
}

/// Detached storage remains unpublished until exact source acceptance.
/// This transaction contains no event journal.
pub(in crate::machine) struct NativeBoundary<'a> {
    pub(in crate::machine) transaction: BoundaryTransaction<'a>,
    pub(in crate::machine) storage: PreparedNative,
}

/// The charge program comes from the canonical compiler at the genuine
/// unpaid suffix. Storage contributes measured arguments/results, never fees.
pub(in crate::machine) struct SourceNative<'a> {
    pub(in crate::machine) recipe: NativeStorage<'a>,
    pub(in crate::machine) charges: &'a room::charges::ChargeProgram,
    pub(in crate::machine) inputs: Vec<Owner>,
    pub(in crate::machine) operands: Vec<Value>,
}
pub(in crate::machine) struct NativePublication<'program> {
    pub(in crate::machine) storage: PreparedNative,
    pub(in crate::machine) holding: lash_kernel_state::kernel_internals::logical::Holding,
    pub(in crate::machine) units: u64,
    pub(in crate::machine) debt: room::charges::ChargeDebt<'program>,
}
enum NativeBinding {
    Ordinary(room::publication::BoundEvent),
    Original(room::publication::OriginalBound),
}
enum NativeEvent<'a> {
    Ordinary(room::publication::PreparedEvent<'a>),
    Original(room::publication::OriginalPrepared<'a>),
}
impl<'program> BoundaryPreparation for SourceNative<'program> {
    type Prepared = NativePublication<'program>;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Self::Prepared)>, HeapFault> {
        Ok(self
            .prepare_inner(arena, entry, None)?
            .map(|(event, storage)| {
                let NativeEvent::Ordinary(event) = event else {
                    unreachable!("ordinary native requested");
                };
                (event, storage)
            }))
    }
    fn prepare_remaining<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
        original: &room::charges::AcceptedOriginalPrefix,
    ) -> Result<Option<(room::publication::OriginalPrepared<'a>, Self::Prepared)>, HeapFault> {
        Ok(self
            .prepare_inner(arena, entry, Some(original))?
            .map(|(event, storage)| {
                let NativeEvent::Original(event) = event else {
                    unreachable!("remaining native requested");
                };
                (event, storage)
            }))
    }
}
impl<'program> SourceNative<'program> {
    fn prepare_inner<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
        original: Option<&room::charges::AcceptedOriginalPrefix>,
    ) -> Result<Option<(NativeEvent<'a>, NativePublication<'program>)>, HeapFault> {
        use room::publication::{ExactRoots, ExternalFacts};
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let Some((
            NativeBoundary {
                transaction,
                storage,
            },
            (bound, holding, debt),
        )) = self.recipe.prepare_boundary(
            arena,
            &self.inputs,
            |arena, storage, proof, entities| {
                storage.bind_facts(&mut prefix.facts)?;
                let formulas: Vec<_> = self.charges.formulas().collect();
                let values = if formulas.is_empty() {
                    vec![]
                } else {
                    if formulas.len() != 1
                        || formulas[0].1 != storage.function
                        || formulas[0].2 != storage.given
                    {
                        return Ok(None);
                    }
                    vec![storage.formula_values()]
                };
                let Some(debt) = self.charges.bind_measured(&values, |_, source, measure| {
                    storage.measure(source, measure)
                }) else {
                    return Ok(None);
                };
                if let Some(canonical) = debt.native_allowance(prefix.allowance.safe_point) {
                    prefix.allowance = canonical;
                    prefix.allowance.depth = prefix.allowance.depth.max(storage.depth());
                } else {
                    prefix.allowance.units = debt.units();
                }
                let scratch = scratch_events(storage.output, storage.contract())?;
                let Ok(bound) = event.bind_external(
                    frontier,
                    ledger,
                    arena,
                    heap,
                    &mut prefix.facts,
                    ExactRoots {
                        inputs: self.inputs.clone(),
                        outputs: vec![(storage.output, PV::Prepared(proof.clone()))],
                        ..Default::default()
                    },
                    prefix.next,
                    prefix.allowance,
                    resources,
                    ExternalFacts {
                        operands: &self.operands,
                        arguments: storage.arguments(),
                        member: None,
                        entities,
                        scratch: &scratch,
                        native: Some(storage.contract()),
                    },
                    &debt,
                ) else {
                    return Ok(None);
                };
                let bound = if let Some(original) = original {
                    match bound.with_original_native(frontier, ledger, original, &debt) {
                        Ok(bound) => NativeBinding::Original(bound),
                        Err(PublicationError::Miss(_)) => return Ok(None),
                        Err(PublicationError::Heap(error)) => return Err(error),
                    }
                } else {
                    NativeBinding::Ordinary(bound)
                };
                let holding = proof.holding(arena)?;
                Ok(Some((bound, holding, debt)))
            },
        )?
        else {
            return Ok(None);
        };
        let prepared = match bound {
            NativeBinding::Ordinary(bound) => bound
                .finish_external(frontier, ledger, transaction)
                .map(NativeEvent::Ordinary),
            NativeBinding::Original(bound) => bound
                .finish_external(frontier, ledger, transaction)
                .map(NativeEvent::Original),
        };
        match prepared {
            Ok(prepared) => Ok(Some((
                prepared,
                NativePublication {
                    storage,
                    holding,
                    units: debt.units(),
                    debt,
                },
            ))),
            Err(PublicationError::Miss(_)) => Ok(None),
            Err(PublicationError::Heap(error)) => Err(error),
        }
    }
}

impl NativeStorage<'_> {
    /// Bind source facts before the transaction takes registration's arena
    /// borrow. Returning None abandons storage and all provisional identities.
    pub(in crate::machine) fn prepare_boundary<'a, C>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
        bind: impl FnOnce(
            &Arena,
            &PreparedNative,
            &PreparedValueProof,
            &[VirtualEntity],
        ) -> Result<Option<C>, HeapFault>,
    ) -> Result<Option<(NativeBoundary<'a>, C)>, HeapFault> {
        let ledger = std::rc::Rc::clone(&self.ledger);
        let heap = self.heap;
        let mut handles: Vec<_> = self.args.iter().filter_map(Value::handle).collect();
        handles.extend(self.known.and_then(|(value, _)| value.handle()));
        let Some(dependencies) = Dependencies::capture(&ledger.borrow(), arena, &handles) else {
            return Ok(None);
        };
        let Some(DetachedNative {
            mut registration,
            storage,
            proof,
        }) = self.prepare_storage(arena, inputs)?
        else {
            return Ok(None);
        };
        if !storage
            .batch
            .matches_proof(proof.graph(), registration.arena())
        {
            return Ok(None);
        }
        let Some(bound) = bind(
            registration.arena(),
            &storage,
            &proof,
            proof.graph().entities(),
        )?
        else {
            return Ok(None);
        };
        let Some(transaction) = ledger.borrow().prepare_native_boundary(
            registration,
            heap,
            inputs,
            storage.output,
            &proof,
            &storage.contract,
            dependencies,
        )?
        else {
            return Ok(None);
        };
        Ok(Some((
            NativeBoundary {
                transaction,
                storage,
            },
            bound,
        )))
    }

    fn prepare_storage<'a>(
        self,
        arena: &'a mut Arena,
        inputs: &[Owner],
    ) -> Result<Option<DetachedNative<'a>>, HeapFault> {
        let LibRun::Native { native, .. } = &self.function.run else {
            return Ok(None);
        };
        if !supports(&self.contract)
            || self.args.len() != self.function.arity
            || !self
                .function
                .header
                .signature
                .params
                .iter()
                .zip(&self.args)
                .all(|(param, value)| {
                    (param.optional && matches!(value, Value::Absent))
                        || resident_argument_fits(arena, &param.ty, *value)
                })
            || self.function.limit.is_some()
            || self.function.shares_params
            || self.function.specialization.resource
                != room::CallSource::Native(self.function.specialization.function)
        {
            return Ok(None);
        }
        // The implementation authenticates the changing input facts. An absent
        // declaration requires its checked prepared scalar answer below.
        let explicit_contract = match native.resource_contract(doc::Args::new(
            &self.args,
            arena.objects(),
            &arena.semantics(),
        )) {
            Ok(Some(contract)) if contract == self.contract => true,
            Ok(None) => false,
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(None),
        };
        let known = match self.known {
            // Explicit scratch phases still require ordinary native execution.
            Some(_) if !self.contract.events.is_empty() => None,
            Some((value, depth)) => {
                if depth != 0
                    || !crate::compile::scalar(&value)
                    || !crate::compile::import_result_fits(
                        &self.function.header.signature.result,
                        value,
                    )
                {
                    return Ok(None);
                }
                match self.function.check_known_scalar(arena, &self.args, value) {
                    Ok(value) => value,
                    Err(()) => return Ok(None),
                }
            }
            None => None,
        };
        if !explicit_contract {
            let Some(value) = known else {
                return Ok(None);
            };
            let canonical = doc::NativeResourceContract::returned(
                doc::Args::new(&self.args, arena.objects(), &arena.semantics()),
                &value,
            )
            .map_err(native_fault)?;
            if canonical != self.contract {
                return Ok(None);
            }
        }
        let (value, work, depth, new) = if let Some(value) = known {
            (value, 0, 0, vec![])
        } else {
            let semantics = arena.semantics().with_limits(self.limits);
            let mut nursery = match self.room {
                Some(room) => doc::Nursery::new(arena.objects(), doc::heap::LogicalBytes(room)),
                None => doc::Nursery::unaccounted(arena.objects()),
            };
            let mut counter = doc::WorkCounter::new(None);
            let answer = native.call(doc::NativeCall {
                args: doc::Args::new(&self.args, arena.objects(), &semantics),
                new: &mut nursery,
                counter: &mut counter,
            });
            let value = match answer {
                Ok(value) => value,
                Err(error) => {
                    nursery.discard();
                    return match error {
                        doc::NativeError::Host(error) => Err(error),
                        doc::NativeError::Malformed(reason) => Err(malformed(reason)),
                        _ => Ok(None),
                    };
                }
            };
            // Heap reads can have a checked direct answer without an immediate
            // prepared entry. Authenticate it with the installed native before
            // retaining any nursery output or accepting the source boundary.
            // Keep the nursery's slot owner until every result guard passes.
            // A speculative type miss must leave the precise retry no gaps.
            if !crate::compile::import_result_fits(&self.function.header.signature.result, value)
                || self
                    .known
                    .is_some_and(|(expected, _)| !value.same(&expected))
            {
                nursery.discard();
                return Ok(None);
            }
            if self.room.is_some_and(|room| {
                arena
                    .install_bound(nursery.built())
                    .checked_add(nursery.held().0)
                    .is_none_or(|bytes| bytes > room)
            }) {
                nursery.discard();
                return Ok(None);
            }
            (value, counter.spent(), 0, nursery.into_new())
        };
        let allocated = !new.is_empty();
        let mut installation = arena.prepare_install(new).map_err(native_fault)?;
        // Retain the canonical changing formula inputs while the detached
        // graph is readable. A region must bind these facts to its compiled
        // plan; it cannot accept the legacy allowance's fee as authority.
        let mut measurements = Vec::new();
        for &(source, measure) in self
            .function
            .charge
            .measurements()
            .iter()
            .filter(|_| self.charging)
        {
            let operand = match source {
                crate::compile::Source::Arg(index) => self.args.get(index),
                crate::compile::Source::Result => Some(&value),
                crate::compile::Source::Nothing => None,
            };
            let measured = if let Some(value) = operand {
                let measured = match measure {
                    doc::Measure::Size => crate::data::view_size(&installation, value),
                    doc::Measure::DeepSize => crate::data::view_deep_size(&installation, value),
                    doc::Measure::NestedSize => crate::data::view_nested_size(&installation, value),
                    doc::Measure::Magnitude => {
                        Ok(crate::data::view_magnitude(&installation, value))
                    }
                };
                match measured {
                    Ok(measured) => measured,
                    Err(doc::NativeError::Host(error)) => return Err(error),
                    Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
                    Err(_) => return Ok(None),
                }
            } else {
                0
            };
            measurements.push((source, measure, measured));
        }
        if !installation.values_fit_depth(&[value], self.limits.value_depth()) {
            return Ok(None);
        }
        let graph = installation.prove_values(
            self.heap.pages(),
            &[value],
            self.limits.value_depth(),
            false,
        )?;
        let proof = graph.value(0);
        let graph_entities = graph.entities().len();
        let (mut registration, batch) = installation.into_parts();
        let released = self.ledger.borrow().release_cost_preserving_graph(
            registration.arena(),
            self.heap,
            inputs,
            &proof,
        )?;
        let owners = graph
            .entities()
            .iter()
            .zip(self.contract.events.iter().filter_map(|event| match event {
                doc::NativeResourceEvent::Entity { owner, .. } => Some(*owner),
                _ => None,
            }))
            .map(|(entity, owner)| (entity.entity, owner))
            .collect();
        if proof.native_holding(&owners) != self.contract.output {
            return Ok(None);
        }
        Ok(Some(DetachedNative {
            registration,
            storage: PreparedNative {
                value,
                batch,
                allocated,
                graph_entities,
                output: self.output,
                contract: self.contract,
                function: self.function.specialization.function,
                arguments: self.args,
                given: inputs.len(),
                measurements,
                work,
                known: self.known.is_some(),
                depth,
                at: self.at,
                release: self.release,
                released,
            },
            proof,
        }))
    }
}

impl PreparedNative {
    pub(in crate::machine) fn arguments(&self) -> &[Value] {
        &self.arguments
    }
    pub(in crate::machine) fn depth(&self) -> u64 {
        u64::from(self.depth)
    }

    pub(in crate::machine) fn bind_facts(
        &self,
        facts: &mut LeafExecution,
    ) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&self.at)
            .ok_or_else(|| malformed("native value lost its release event"))?;
        facts.values.insert(
            Measure::Release(self.release.clone()),
            i128::from(self.released),
        );
        facts.values.insert(
            Measure::NativeScratch {
                at: self.at.site.clone(),
                function: self.function,
            },
            i128::from(
                self.contract
                    .peak_scratch_bytes()
                    .ok_or_else(|| malformed("native scratch schedule changed"))?,
            ),
        );
        facts.call = Some((
            self.at.site.clone(),
            room::CallSource::Native(self.function),
            room::CheckedRoom::native(self.function, &self.contract)
                .ok_or_else(|| malformed("native result contract changed"))?,
        ));
        Ok(())
    }
    pub(in crate::machine) fn formula_values(&self) -> room::charges::FormulaValues<'_> {
        room::charges::FormulaValues {
            arguments: &self.arguments[..self.given],
            result: self.value,
        }
    }
    pub(in crate::machine) fn measure(
        &self,
        source: crate::compile::Source,
        measure: doc::Measure,
    ) -> Option<u64> {
        self.measurements
            .iter()
            .find_map(|(s, m, value)| (*s == source && *m == measure).then_some(*value))
    }
    pub(in crate::machine) fn contract(&self) -> &doc::NativeResourceContract {
        &self.contract
    }

    pub(in crate::machine) fn known(&self) -> bool {
        self.known
    }
    pub(in crate::machine) fn allocated_immutable(&self) -> bool {
        self.allocated && self.graph_entities == 0
    }
    pub(in crate::machine) fn value(&self) -> Value {
        self.value
    }
    pub(in crate::machine) fn work(&self) -> u64 {
        self.work
    }
    pub(in crate::machine) fn publish(self, arena: &mut Arena) {
        #[cfg(any(test, feature = "testing"))]
        if self.graph_entities > 1 {
            crate::machine::islands::witness::native_graph_published();
        }
        self.batch.publish(arena);
    }
}

/// A kind check suffices only for unconstrained containers. A resident list
/// with a constrained item contract checks every actual resident member.
/// Enum, union, tuple and set parameters retain their canonical type checks;
/// cold or unsupported shapes use the ordinary signature handler.
pub(in crate::machine) fn resident_argument_fits(
    arena: &Arena,
    ty: &lash_kernel_value::Type,
    value: Value,
) -> bool {
    if let Some(check) = crate::compile::import_result_check(ty) {
        return check(value);
    }
    use lash_kernel_value::Type;
    match (ty, value) {
        (Type::Enum(members), value @ (Value::Text(_) | Value::LongText(_))) => arena
            .semantics()
            .text(&value)
            .is_ok_and(|text| members.iter().any(|member| member == text)),
        (Type::Union(members), value) => members
            .iter()
            .any(|ty| resident_argument_fits(arena, ty, value)),
        (Type::Tuple(items), Value::Tuple(handle)) => arena.sequence(handle).is_ok_and(|values| {
            values.len() == items.len()
                && items
                    .iter()
                    .zip(values)
                    .all(|(ty, value)| resident_argument_fits(arena, ty, *value))
        }),
        (Type::List(item), Value::List(handle)) => arena.sequence(handle).is_ok_and(|values| {
            values
                .iter()
                .all(|value| resident_argument_fits(arena, item, *value))
        }),
        (Type::Set(item), Value::Set(handle)) => {
            arena.semantics().entries(handle).is_ok_and(|values| {
                values
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .all(|pair| resident_argument_fits(arena, item, pair[0]))
            })
        }
        _ => false,
    }
}

pub(in crate::machine) fn supports(contract: &doc::NativeResourceContract) -> bool {
    contract.exit == doc::NativeResourceExit::Returned
        && contract.retained_bytes().is_some()
        && contract.events.iter().all(|event| {
            matches!(
                event,
                doc::NativeResourceEvent::Entity {
                    kind: doc::NativeResourceEntity::List
                        | doc::NativeResourceEntity::Map
                        | doc::NativeResourceEntity::Set
                        | doc::NativeResourceEntity::Record { .. },
                    ..
                } | doc::NativeResourceEvent::Scratch { .. }
            )
        })
}

/// Retain the canonical lease boundaries, including a release before an entity
/// allocation. Equal peaks do not imply interchangeable phase schedules.
pub(in crate::machine) fn scratch_events(
    output: Owner,
    contract: &doc::NativeResourceContract,
) -> Result<Vec<lash_kernel_state::kernel_internals::logical::ScratchEvent>, HeapFault> {
    use doc::{NativeResourceEvent as Event, NativeResourceRelease as Release};
    use lash_kernel_state::kernel_internals::logical::{ScratchEvent, Slot};
    let Owner::Value {
        task,
        invocation,
        slot: Slot::Operand(_),
    } = output
    else {
        return Err(malformed("native output has no operand owner"));
    };
    if contract.peak_scratch_bytes().is_none() {
        return Err(malformed("native scratch contract is invalid"));
    }
    let owner = |index: doc::NativeResourceOwner| Owner::Value {
        task,
        invocation,
        slot: Slot::Scratch(index.0),
    };
    let mut active = Vec::new();
    let mut events = Vec::new();
    for event in &contract.events {
        active.retain(|(index, release)| {
            if *release == Release::Before(event.owner()) {
                events.push(ScratchEvent::End {
                    owner: owner(*index),
                });
                false
            } else {
                true
            }
        });
        if let Event::Scratch {
            owner: index,
            bytes,
            release,
        } = event
        {
            events.push(ScratchEvent::Begin {
                owner: owner(*index),
                bytes: *bytes,
            });
            active.push((*index, *release));
        }
    }
    events.extend(active.into_iter().map(|(index, _)| ScratchEvent::End {
        owner: owner(index),
    }));
    Ok(events)
}

#[cfg(test)]
thread_local! { static NATIVE_PUBLICATIONS: Cell<[u64; 5]> = const { Cell::new([0; 5]) }; }
#[cfg(test)]
pub(in crate::machine) fn published(
    packet: bool,
    contract: &doc::NativeResourceContract,
    optional: bool,
) {
    NATIVE_PUBLICATIONS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(packet)] += 1;
        next[2] += u64::from(
            contract
                .events
                .iter()
                .any(|event| matches!(event, doc::NativeResourceEvent::Entity { .. })),
        );
        next[3] += u64::from(
            contract
                .events
                .iter()
                .any(|event| matches!(event, doc::NativeResourceEvent::Scratch { .. })),
        );
        next[4] += u64::from(optional);
        counts.set(next);
    });
}
#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn take_native_publications(&self) -> [u64; 5] {
        NATIVE_PUBLICATIONS.with(|counts| counts.replace([0; 5]))
    }
}

#[cfg(test)]
mod scalar_validation_tests {
    use super::*;
    use crate::laws::embedder::{Embedder, Setup};

    #[test]
    fn known_answers_use_their_kernel_and_reject_disagreement() {
        struct Kernel;
        impl doc::NativeFunction for Kernel {
            fn call(&self, _: doc::NativeCall<'_, '_>) -> Result<Value, doc::NativeError> {
                panic!("kernel validation must not execute the native");
            }
            fn prepare(&self) -> Option<doc::PreparedNative> {
                Some(doc::PreparedNative {
                    pair: Some(doc::scalar::Pair {
                        ints: Some(doc::scalar::ScalarOp::Add),
                        floats: Some(doc::scalar::ScalarOp::Add),
                    }),
                    entry: |_| panic!("an available kernel must not replay the prepared entry"),
                    charged_entry: None,
                    operation: "law.kernel_validation",
                })
            }
            fn specialization(&self) -> doc::NativeSpecialization {
                doc::NativeSpecialization {
                    immediate: self.prepare(),
                    text_relation: Some(doc::scalar::ScalarOp::Lt),
                    ..Default::default()
                }
            }
        }
        fn probes(
            registry: &mut doc::FunctionRegistry,
            _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
        ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
            let definition = doc::parse_definition("function law.kernel_validation(a: Any, b: Any) -> Any\nkernel 1\ncharge 5\nnative\n").unwrap();
            vec![(
                "law.kernel_validation",
                registry
                    .register(definition, Some(std::sync::Arc::new(Kernel)))
                    .unwrap(),
            )]
        }
        let run = Embedder::with(
            "main { return law.kernel_validation(3, 4) }",
            Setup {
                probes: Some(probes),
                ..Default::default()
            },
        );
        let function = run.machine.exe.lib(
            run.machine
                .exe
                .library_lib(&run.library.ids["law.kernel_validation"])
                .unwrap(),
        );
        let arena = Arena::new();
        assert!(matches!(
            function.validate_known_scalar(&arena, &[Value::Int(3), Value::Int(4)], Value::Int(7)),
            Some(Value::Int(7))
        ));
        assert!(
            function
                .validate_known_scalar(&arena, &[Value::Int(3), Value::Int(4)], Value::Int(8))
                .is_none()
        );
        assert!(
            function
                .validate_known_scalar(
                    &arena,
                    &[Value::Int(i64::MAX), Value::Int(1)],
                    Value::Int(0)
                )
                .is_none()
        );
        let float = |value| Value::Float(doc::Float::new(value));
        assert!(
            function
                .validate_known_scalar(&arena, &[float(1.0), float(2.0)], float(3.0))
                .is_some()
        );
        assert!(
            function
                .validate_known_scalar(&arena, &[float(1.0), float(2.0)], float(4.0))
                .is_none()
        );
        let text = |value: &str| Value::Text(doc::arena::ShortText::new(value).unwrap());
        let args = [text("😀"), text("\u{e000}")];
        assert!(matches!(
            function.validate_known_scalar(&arena, &args, Value::Bool(true)),
            Some(Value::Bool(true))
        ));
        assert!(
            function
                .validate_known_scalar(&arena, &args, Value::Bool(false))
                .is_none()
        );
    }
}
