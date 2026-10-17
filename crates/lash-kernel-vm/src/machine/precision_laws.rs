//! Pin and precision laws: what the running statement holds is priced at
//! its holding locations, and every forced precision point sees the same
//! reservations and refinements whatever the pins' representation
//! (FIG-6587, `K-BND-003`).

mod scalar_laws {
    use crate::laws::embedder::Embedder;
    use crate::{End, Machine, Step};
    use lash_kernel_doc::Value;
    use lash_kernel_doc::heap::LogicalBytes;
    use lash_kernel_state::kernel_internals::census_impl::Census;
    use lash_kernel_state::paged::{
        AccountingCertificate, AllocationCensus, CensusFault, CensusGraph, PendingReservation,
        RefinedDecision, ReservationDecision, TransientRoots,
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
        machine
            .pin_value(&Value::Float(lash_kernel_doc::Float::new(1.5)))
            .unwrap();
        assert_eq!(machine.heap.certificate().upper.0, before.upper.0 + 16);
        // K-BND-003: Boolean guard answers are immediate, so they add no
        // allocation beyond the Float holding already installed here.
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

mod precision {
    use crate::laws::embedder::{Embedder, ROOMY, Setup, int};
    use crate::{Bounds, End, KernelMachine, Layout, RunError, Step};
    use lash_kernel_doc::Datum;
    use lash_kernel_doc::heap::LogicalBytes;
    use lash_kernel_state::kernel_internals::census_impl::Census;
    use lash_kernel_state::paged::{
        AccountingCertificate, AllocationCensus, CensusFault, CensusGraph, PendingReservation,
        RefinedDecision, ReservationDecision, TransientRoots,
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
