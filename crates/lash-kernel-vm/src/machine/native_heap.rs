//! Native calls publish execution records only when their heap needs them.
//! Reads and immutable payload construction do not collect. Reservations
//! and allocations retry their refused heap operation after publication;
//! the heap itself refines with that operation's proposed values in scope.
use super::tasks::Tasks;
use lash_kernel_doc::heap::{
    BytesRef, Chunk, ErrorRef, FieldName, HeapFault, IntRef, NormalKey, TextRef, TupleRef,
};
use lash_kernel_doc::{Element, EntityId, Integer, NativeError, NativeHeap, Object, Value};
use lash_kernel_state::paged::tree::Tree;
use lash_kernel_state::paged::{HeapWriteError, ListJoinRecord, PagedHeap};
use std::ops::{ControlFlow, Range};
use std::sync::Arc;

/// The machine's authoritative roots while its heap is borrowed by a native.
pub(super) struct Roots<'a> {
    pub(super) tasks: &'a mut Tasks,
    pub(super) joins: &'a Tree<ListJoinRecord>,
    pub(super) registers: &'a super::registers::Registers,
    pub(super) pinned: &'a [Value],
    pub(super) pinned_bytes: u64,
    pub(super) fresh: &'a [EntityId],
    pub(super) refinements: &'a mut u64,
}

impl Roots<'_> {
    fn reconcile(&mut self, heap: &PagedHeap) -> Result<(), HeapFault> {
        let refinements = heap.certificate().refinements;
        if refinements != *self.refinements {
            self.tasks.after_refinement(heap.task_root().clone())?;
            *self.refinements = refinements;
        }
        Ok(())
    }

    pub(super) fn publish(
        &mut self,
        heap: &mut PagedHeap,
        coordinates: bool,
    ) -> Result<(), HeapWriteError> {
        #[cfg(any(test, feature = "perf-witness"))]
        crate::perf_witness::publish();
        self.reconcile(heap)?;
        heap.update_execution_roots(
            self.joins.clone(),
            &self.registers.projection(),
            self.pinned,
            self.pinned_bytes,
            self.fresh,
            || {
                if coordinates {
                    self.tasks.roots()
                } else {
                    self.tasks.holdings()
                }
            },
        )?;
        self.reconcile(heap)?;
        Ok(())
    }
}

pub(super) struct CallHeap<'a> {
    pub(super) heap: &'a mut PagedHeap,
    pub(super) roots: Roots<'a>,
}

fn native_error(error: HeapWriteError) -> NativeError {
    match error {
        HeapWriteError::Memory => NativeError::Memory,
        HeapWriteError::Fault(error) => NativeError::Heap(error),
        HeapWriteError::Raised { kind, message } => {
            NativeError::Raised(Arc::new(ErrorRef::new(kind, &message)))
        }
        HeapWriteError::NeedsPrecision { bytes } => NativeError::Heap(HeapFault::Malformed(
            format!("a native refinement still needs precision for {bytes} bytes"),
        )),
    }
}

impl CallHeap<'_> {
    /// As in the machine's write wrapper, a refusal is before any edit.
    /// Retry that heap operation alone, never the native that called it.
    fn write<T>(
        &mut self,
        mut write: impl FnMut(&mut PagedHeap) -> Result<T, NativeError>,
    ) -> Result<T, NativeError> {
        self.heap.take_refusal();
        match write(self.heap) {
            Err(error) => match self.heap.take_refusal() {
                Some(_) => {
                    self.roots.publish(self.heap, false).map_err(native_error)?;
                    // The retry refines with its proposed graph in scope.
                    // Refining first could sweep a child the native built
                    // earlier and holds only in that proposed object.
                    write(self.heap)
                }
                None => Err(error),
            },
            result => result,
        }
    }
}

macro_rules! reads {
    ($(fn $name:ident(&self $(, $arg:ident: $ty:ty)*) -> $out:ty;)*) => {
        $(fn $name(&self $(, $arg: $ty)*) -> $out {
            NativeHeap::$name(&*self.heap $(, $arg)*)
        })*
    };
}

impl NativeHeap for CallHeap<'_> {
    reads! {
        fn normal_key(&self, value: &Value) -> Result<Option<NormalKey>, HeapFault>;
        fn equal(&self, a: &Value, b: &Value) -> Result<bool, HeapFault>;
        fn data_depth_limit(&self) -> usize;
        fn value_depth_limit(&self) -> usize;
        fn len(&self, object: EntityId) -> Result<u64, HeapFault>;
        fn list_get(&self, list: EntityId, index: u64) -> Result<Option<Value>, HeapFault>;
        fn map_get(&self, map: EntityId, key: &Value) -> Result<Option<Value>, HeapFault>;
        fn set_contains(&self, set: EntityId, member: &Value) -> Result<bool, HeapFault>;
        fn record_get(&self, record: EntityId, field: FieldName<'_>) -> Result<Option<Value>, HeapFault>;
        fn visit(&self, object: EntityId, visitor: &mut dyn FnMut(Element<'_>) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn error_data(&self, error: &ErrorRef) -> Result<Value, HeapFault>;
        fn tuple_get(&self, tuple: &TupleRef, index: u64) -> Result<Option<Value>, HeapFault>;
        fn tuple_visit(&self, tuple: &TupleRef, visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn payload_visit(&self, value: &Value, range: Range<u64>, visitor: &mut dyn FnMut(Chunk<'_>) -> ControlFlow<()>) -> Result<(), HeapFault>;
        fn text_offset(&self, text: &TextRef, index: u64) -> Result<u64, HeapFault>;
    }

    fn materialize_text(&mut self, text: &TextRef) -> Result<Arc<str>, NativeError> {
        self.write(|heap| heap.materialize_text(text))
    }
    fn materialize_bytes(&mut self, bytes: &BytesRef) -> Result<Arc<[u8]>, NativeError> {
        self.write(|heap| heap.materialize_bytes(bytes))
    }
    fn materialize_int(&mut self, int: &IntRef) -> Result<Integer, NativeError> {
        self.write(|heap| heap.materialize_int(int))
    }
    fn text(&mut self, text: &str) -> Result<TextRef, NativeError> {
        self.write(|heap| heap.text(text))
    }
    fn bytes(&mut self, bytes: &[u8]) -> Result<BytesRef, NativeError> {
        self.write(|heap| heap.bytes(bytes))
    }
    fn int(&mut self, int: Integer) -> Result<IntRef, NativeError> {
        self.write(|heap| heap.int(int.clone()))
    }
    fn tuple(&mut self, members: Vec<Value>) -> Result<Arc<TupleRef>, NativeError> {
        self.write(|heap| heap.tuple(members.clone()))
    }
    fn concat(&mut self, parts: &[Value]) -> Result<Value, NativeError> {
        self.write(|heap| heap.concat(parts))
    }
    fn slice(&mut self, value: &Value, range: Range<u64>) -> Result<Value, NativeError> {
        self.write(|heap| heap.slice(value, range.clone()))
    }
    fn allocate(&mut self, object: Object) -> Result<EntityId, NativeError> {
        self.write(|heap| heap.allocate(object.clone()))
    }
    fn reserve(&mut self, values: u64, bytes: u64) -> Result<(), NativeError> {
        self.write(|heap| heap.reserve(values, bytes))
    }
}

#[cfg(test)]
mod laws {
    use super::*;
    use crate::Bounds;
    use crate::laws::embedder::{Embedder, Setup};

    /// FIG-6614: scalar statements reuse initialized storage; publication
    /// still prices the complete logical window, including taken Nulls.
    #[test]
    fn register_reuse_preserves_pressure_projection() {
        use crate::Machine;
        use crate::machine::{Boundary, KernelMachine};
        use lash_kernel_state::paged::directory::value_cost;

        fn assert_projection(machine: &mut KernelMachine, expected: &[Value]) {
            let actual = machine.regs.projection();
            assert_eq!(actual.len(), expected.len());
            for (actual, expected) in actual.iter().zip(expected) {
                assert!(machine.heap.equal(actual, expected).unwrap());
                assert_eq!(value_cost(actual), value_cost(expected));
            }
            machine.publish_holdings().unwrap();
            machine.heap.collect().unwrap();
            let reused = machine.heap.certificate();
            let tasks = machine.tasks.holdings().unwrap();
            machine
                .heap
                .set_execution_roots(
                    tasks,
                    machine.joins.clone(),
                    expected,
                    &machine.pins.values,
                    machine.pins.bytes,
                    &machine.fresh,
                )
                .unwrap();
            machine.heap.collect().unwrap();
            let fresh = machine.heap.certificate();
            assert_eq!(
                reused.upper, fresh.upper,
                "publication changed logical memory"
            );
            assert_eq!(
                reused.entities, fresh.entities,
                "publication changed reachable roots"
            );
        }

        let run = |n, collect| {
            let mut embedder = Embedder::new(&format!(
                "main {{ let i = 0 while num.lt(i, {n}) {{ set i = num.add(i, 1) }} return i }}"
            ));
            if collect {
                embedder.machine.boundary = Some(Box::new(|_| Boundary::Collect));
            }
            assert_eq!(
                crate::laws::embedder::result(embedder.run_to_end(&[])),
                crate::laws::embedder::int(n)
            );
            (
                embedder.machine.regs.initializations,
                embedder.machine.meters().charged,
            )
        };
        let warm = run(3, false).0;
        let repeated = run(100, false);
        assert!(warm > 0, "the witness must initialize a real window");
        assert_eq!(
            repeated.0, warm,
            "warm statements refilled the physical window"
        );
        assert_eq!(
            run(100, true),
            repeated,
            "collection changed charge or initialization"
        );

        let mut embedder = Embedder::new("main { return null }");
        let machine = &mut embedder.machine;
        machine.regs.window(130);
        for at in 0..130 {
            machine.put(at, Value::Int((at as u64).into()));
        }
        machine.regs.clear();
        machine.regs.window(70);
        let mut expected = vec![Value::Null; 70];
        assert_projection(machine, &expected);

        let list = Value::List(
            machine
                .heap
                .allocate(Object::List(vec![Value::Null; 7]))
                .unwrap(),
        );
        machine.put(65, list.clone());
        expected[65] = list;
        assert_projection(machine, &expected);
        let taken = machine.take(65);
        machine.pin_value(&taken).unwrap();
        expected[65] = Value::Null;
        assert_projection(machine, &expected);

        let owned = Value::text("released at the statement boundary");
        let Value::Text(text) = &owned else {
            unreachable!()
        };
        let lash_kernel_doc::heap::Payload::Resident(text) = &text.body else {
            unreachable!()
        };
        let weak = Arc::downgrade(text);
        machine.put(67, owned.clone());
        expected[67] = owned;
        assert_projection(machine, &expected);

        // Native arguments must remain contiguous, including taken Nulls
        // and missing parameters filled with Absent. A writer beyond the
        // declared window grows its logical extent just as the old Vec did.
        for at in [64, 66] {
            machine.put(at, Value::Absent);
            expected[at] = Value::Absent;
        }
        for (actual, expected) in machine.regs[64..68].iter().zip(&expected[64..68]) {
            assert!(machine.heap.equal(actual, expected).unwrap());
        }
        machine.put(131, Value::Bool(true));
        expected.resize(132, Value::Null);
        expected[131] = Value::Bool(true);
        assert_projection(machine, &expected);
        machine.regs.clear();
        machine.pins.clear();
        machine.heap.release_statement_roots().unwrap();
        drop(expected);
        assert!(
            weak.upgrade().is_none(),
            "scratch retained an owned value past its boundary"
        );
        assert_projection(machine, &[]);
    }

    /// K-BND-003: the operation which requested precision must bring its
    /// proposed edges to the trace. A child held only by a native's local
    /// value survives that allocation; unrelated garbage does not.
    #[test]
    fn allocation_pressure_keeps_the_proposed_objects_native_only_child() {
        let mut embedder = Embedder::with(
            "main { return null }",
            Setup {
                bounds: Bounds {
                    memory: 4096,
                    ..crate::laws::embedder::ROOMY
                },
                ..Setup::default()
            },
        );
        let machine = &mut embedder.machine;
        machine.heap.execution_roots_moved();
        let mut heap = CallHeap {
            heap: &mut machine.heap,
            roots: Roots {
                tasks: &mut machine.tasks,
                joins: &machine.joins,
                registers: &machine.regs,
                pinned: &machine.pins.values,
                pinned_bytes: machine.pins.bytes,
                fresh: &machine.fresh,
                refinements: &mut machine.census_refinements,
            },
        };
        heap.allocate(Object::List(vec![Value::Null; 150])).unwrap();
        let child = heap.allocate(Object::List(vec![Value::Null; 50])).unwrap();
        let before = heap.heap.certificate().refinements;
        let parent = heap
            .allocate(Object::List(vec![Value::List(child); 50]))
            .unwrap();
        assert!(heap.heap.certificate().refinements > before);
        assert_eq!(heap.len(child).unwrap(), 50);
        assert!(matches!(heap.list_get(parent, 0).unwrap(), Some(Value::List(id)) if id == child));
    }
}

#[cfg(test)]
mod scalar_laws {
    use super::*;
    use crate::laws::embedder::Embedder;
    use crate::{End, Machine, Step};
    use lash_kernel_doc::heap::LogicalBytes;
    use lash_kernel_state::paged::{
        AccountingCertificate, AllocationCensus, CensusFault, CensusGraph, PendingReservation,
        RefinedDecision, ReservationDecision, TransientRoots, census_impl::Census,
    };
    use std::cell::Cell;
    use std::rc::Rc;

    struct Counted {
        census: Census,
        certificates: Rc<Cell<u64>>,
    }

    impl AllocationCensus for Counted {
        fn restore(
            certificate: AccountingCertificate,
            bound: LogicalBytes,
        ) -> Result<Self, CensusFault> {
            Ok(Self {
                census: Census::restore(certificate, bound)?,
                certificates: Rc::default(),
            })
        }
        fn certificate(&self) -> AccountingCertificate {
            self.certificates.set(self.certificates.get() + 1);
            self.census.certificate()
        }
        fn added(&mut self, cost: LogicalBytes, entities: u64) -> Result<(), CensusFault> {
            self.census.added(cost, entities)
        }
        fn changed(
            &mut self,
            before: LogicalBytes,
            after: LogicalBytes,
        ) -> Result<(), CensusFault> {
            self.census.changed(before, after)
        }
        fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision {
            self.census.reserve(bytes)
        }
        fn refine(
            &mut self,
            graph: &mut dyn CensusGraph,
            transient: &TransientRoots<'_>,
            pending: PendingReservation,
        ) -> Result<RefinedDecision, CensusFault> {
            self.census.refine(graph, transient, pending)
        }
    }

    /// FIG-6587 / K-BND-003: prepared scalar answers retain their holding
    /// cost without a tracing root or another copy of the census certificate.
    #[test]
    fn prepared_scalar_answers_need_no_root_insertions_or_hold_certificate_copies() {
        let mut embedder = Embedder::new(
            "main { let i = 0 while num.lt(i, 12) { set i = num.add(i, 1) } return i }",
        );
        let end = Machine::run(&mut embedder.machine, &mut embedder.world, u64::MAX).unwrap();
        assert!(
            matches!(end, Step::Ended(End::Finished(ref result)) if result.result == crate::laws::embedder::int(12)),
            "{end:?}"
        );
        let insertions = embedder.machine.pins.scalar_root_insertions;

        let machine = &mut embedder.machine;
        let before = machine.heap.certificate();
        let counted = Counted::restore(before, LogicalBytes(u64::MAX)).unwrap();
        let certificates = counted.certificates.clone();
        machine.heap.set_census(Box::new(counted)).unwrap();
        certificates.set(0);
        machine.pin_value(&Value::Bool(true)).unwrap();
        assert_eq!(machine.heap.certificate().upper.0, before.upper.0 + 16);
        machine.heap.release_statement_roots().unwrap();
        assert_eq!(machine.heap.certificate(), before);
        println!(
            "scalar root insertions={insertions}, hold/release certificate copies={}",
            certificates.get()
        );
        assert_eq!((insertions, certificates.get()), (0, 0));
    }
}

#[cfg(test)]
mod precision_laws {
    use crate::laws::embedder::{Embedder, ROOMY, Setup, int};
    use crate::{Bounds, End, KernelMachine, Layout, RunError, Step};
    use lash_kernel_doc::Datum;
    use lash_kernel_doc::heap::LogicalBytes;
    use lash_kernel_state::paged::{
        AccountingCertificate, AllocationCensus, CensusFault, CensusGraph, PendingReservation,
        RefinedDecision, ReservationDecision, TransientRoots, census_impl::Census,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Default, PartialEq)]
    struct Precision {
        reservations: Vec<(u64, AccountingCertificate, bool)>,
        refinements: Vec<Refinement>,
    }

    #[derive(Debug, PartialEq)]
    struct Refinement {
        pending: u64,
        transient_cost: u64,
        roots: LogicalBytes,
        before: AccountingCertificate,
        after: AccountingCertificate,
        granted: bool,
    }

    struct Recording {
        census: Census,
        precision: Rc<RefCell<Precision>>,
    }

    impl AllocationCensus for Recording {
        fn restore(
            certificate: AccountingCertificate,
            bound: LogicalBytes,
        ) -> Result<Self, CensusFault> {
            Ok(Self {
                census: Census::restore(certificate, bound)?,
                precision: Rc::default(),
            })
        }
        fn certificate(&self) -> AccountingCertificate {
            self.census.certificate()
        }
        fn added(&mut self, cost: LogicalBytes, entities: u64) -> Result<(), CensusFault> {
            self.census.added(cost, entities)
        }
        fn changed(
            &mut self,
            before: LogicalBytes,
            after: LogicalBytes,
        ) -> Result<(), CensusFault> {
            self.census.changed(before, after)
        }
        fn reserve(&mut self, bytes: LogicalBytes) -> ReservationDecision {
            let decision = self.census.reserve(bytes);
            self.precision.borrow_mut().reservations.push((
                bytes.0,
                self.census.certificate(),
                matches!(decision, ReservationDecision::Granted(_)),
            ));
            decision
        }
        fn refine(
            &mut self,
            graph: &mut dyn CensusGraph,
            transient: &TransientRoots<'_>,
            pending: PendingReservation,
        ) -> Result<RefinedDecision, CensusFault> {
            let transient_cost = transient
                .registers
                .iter()
                .chain(transient.pinned)
                .chain(transient.native_args)
                .map(|value| graph.value_cost(value).unwrap().0)
                .try_fold(transient.reserved.0, u64::checked_add)
                .unwrap();
            let roots = graph.root_cost().unwrap();
            let before = self.census.certificate();
            let bytes = pending.bytes.0;
            let result = self.census.refine(graph, transient, pending);
            self.precision.borrow_mut().refinements.push(Refinement {
                pending: bytes,
                transient_cost,
                roots,
                before,
                after: self.census.certificate(),
                granted: matches!(result, Ok(RefinedDecision::Granted(_))),
            });
            result
        }
    }

    #[derive(Debug, Default, PartialEq)]
    struct Seen {
        locations: Vec<Option<u64>>,
        holds: Vec<(u64, u64)>,
        checkpoints: Vec<Vec<u8>>,
    }

    fn capture(machine: &mut KernelMachine, seen: &mut Seen) -> u64 {
        seen.locations.append(&mut machine.pins.scalar_locations);
        seen.holds.append(&mut machine.pins.reservations);
        std::mem::take(&mut machine.pins.scalar_root_insertions)
    }

    fn recording(
        machine: &mut KernelMachine,
        bounds: Bounds,
        precision: Rc<RefCell<Precision>>,
        descriptors: bool,
    ) {
        let mut census =
            Recording::restore(machine.heap.certificate(), LogicalBytes(bounds.memory)).unwrap();
        census.precision = precision;
        machine.heap.set_census(Box::new(census)).unwrap();
        machine.pins.descriptor_pins = descriptors;
        machine.pins.force_precision = true;
    }

    /// FIG-6587 / K-BND-003: every original pin location and reservation
    /// retains its multiplicity, at each forced precision point, across
    /// nested-action roles, an escaping return through finally, and parks.
    #[test]
    fn cost_only_pins_preserve_precision_multiplicity() {
        const PROGRAM: &str = "fn escaped(x) { try { return num.add(x, 1) } finally { do yield } }\n\
            main {\n\
            let f = fn(x) { return num.add(x, 1) }\n\
            let a = num.add(num.add(1, 2), perform echo(num.add(3, 4)) as Any)\n\
            let b = call escaped(a)\n\
            let c = apply f(b)\n\
            let values = work.fill(512)\n\
            let out = [a, b, c, list.len(values)]\n\
            print out return out }";
        let mut memory_refused = false;
        let mut charge_refused = false;
        for memory in [4096, 8192, ROOMY.memory] {
            for charge in [15, ROOMY.charge] {
                for slice in [1, u64::MAX] {
                    let bounds = Bounds {
                        memory,
                        charge,
                        ..ROOMY
                    };
                    let mut observations = Vec::new();
                    for descriptors in [true, false] {
                        let mut embedder = Embedder::with(
                            PROGRAM,
                            Setup {
                                bounds,
                                slice,
                                ..Setup::default()
                            },
                        );
                        let precision = Rc::new(RefCell::new(Precision::default()));
                        let mut seen = Seen::default();
                        let mut insertions = 0;
                        recording(
                            &mut embedder.machine,
                            bounds,
                            precision.clone(),
                            descriptors,
                        );
                        let end = loop {
                            let step = embedder.run();
                            if let Step::Ended(end) = step {
                                break end;
                            }
                            // On roomy runs, every cut is sealed and opened
                            // cold: the encoded roots pin saved roles and
                            // the pending return as well as the result.
                            if memory == ROOMY.memory {
                                let (root, certificate) = embedder.checkpoint();
                                seen.checkpoints
                                    .push(lash_kernel_doc::graph_tokens::encode_body(&root));
                                insertions += capture(&mut embedder.machine, &mut seen);
                                embedder.open_at(&root, certificate, Layout::default(), true);
                                recording(
                                    &mut embedder.machine,
                                    bounds,
                                    precision.clone(),
                                    descriptors,
                                );
                            }
                            if matches!(step, Step::Parked(_)) {
                                embedder.deliver_next();
                            }
                        };
                        insertions += capture(&mut embedder.machine, &mut seen);
                        if !descriptors {
                            assert_eq!(insertions, 0);
                        }
                        if memory == ROOMY.memory && charge == ROOMY.charge {
                            assert!(!seen.checkpoints.is_empty());
                            assert!(
                                matches!(&end, End::Finished(finished) if finished.result == Datum::List(vec![int(10), int(11), int(12), int(512)])),
                                "{end:?}"
                            );
                            assert!(seen.locations.iter().any(Option::is_some));
                            assert!(precision.borrow().refinements.len() > 1);
                            if descriptors {
                                assert!(insertions > 0);
                            }
                        }
                        memory_refused |= matches!(&end, End::Error(RunError::Bound(bound)) if bound.bound == crate::Bound::Memory);
                        charge_refused |= matches!(&end, End::Error(RunError::Bound(bound)) if bound.bound == crate::Bound::Charge);
                        let requests = format!("{:?}", embedder.requests);
                        let mut precision = precision.borrow_mut();
                        observations.push((
                            end,
                            embedder.machine.charged,
                            requests,
                            embedder.world.printed,
                            seen,
                            std::mem::take(&mut precision.reservations),
                            std::mem::take(&mut precision.refinements),
                        ));
                    }
                    assert_eq!(
                        observations[0], observations[1],
                        "memory {memory}, charge {charge}, slice {slice}"
                    );
                }
            }
        }
        assert!(
            memory_refused && charge_refused,
            "the corpus must reach both first refusals"
        );
    }
}
