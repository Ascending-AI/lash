//! Safe points publish one bounded root naming the canonical record trees.
use super::*;
use crate::interface::CellStart;
use crate::paged::{OpenError, SealError, SealedRoot};
use lash_kernel_state::kernel_internals::tree::SealWork;
use lash_kernel_state::paged::{
    AllocationCensus, CheckpointRoot, EditEpoch, PageKind, PageSource, RootStatus, SavedMeters,
    SealBudget, SequenceRoot,
};
use lash_kernel_value::tokens::encode_body;

impl Machine for KernelMachine {
    fn start(
        program: Program,
        bounds: Bounds,
        start: CellStart,
        source: Box<dyn PageSource>,
    ) -> Result<Self, StartError> {
        Self::start_with_layout(program, bounds, start, Layout::default(), source)
    }
    fn open(
        program: Program,
        bounds: Bounds,
        root: &lash_kernel_state::paged::Checkpoint,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        Self::open_with_layout(
            program,
            bounds,
            lash_kernel_state::kernel_internals::checkpoint_root(root),
            source,
            Layout::default(),
        )
    }
    fn set_options(&mut self, options: crate::MachineOptions) {
        self.set_observe(options.observation);
        if options.trace_path {
            self.trace_path();
        } else if let Some(observer) = self.observer.as_deref_mut() {
            observer.path = None;
        }
        self.save = options.cell_save;
        self.captions = options.captions;
        self.function_origins = options.function_origins;
        self.limiters = Limiters {
            memory: options.memory_limiter,
            charge: options.charge_limiter,
        };
    }
    fn take_counts(&mut self) -> crate::SiteCounts {
        self.take_ran()
    }
    fn take_path(&mut self) -> Result<crate::ExecutionPath, HeapFault> {
        KernelMachine::take_execution_path(self)
    }
    fn take_caught(&mut self) -> Vec<WaitId> {
        KernelMachine::take_caught(self)
    }
    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        // The ids a projection gives must not depend on when its budget
        // ran out: nothing runs until it completes (seam G).
        if self.projecting {
            return Err(MachineError::Projecting);
        }
        #[cfg(any(test, feature = "perf-witness"))]
        let (witness, observer_added) = {
            let enabled = crate::perf_witness::enabled();
            let added = enabled && self.observer.is_none();
            if enabled {
                self.observer_mut().work_witness = true;
            }
            (enabled, added)
        };
        self.observe_phase(GuestPhase::Interpret);
        let result = self.run_slice(host, slice);
        #[cfg(any(test, feature = "perf-witness"))]
        if witness {
            if observer_added {
                self.observer = None;
            } else if let Some(observer) = self.observer.as_deref_mut() {
                observer.work_witness = false;
            }
        }
        self.regs.clear();
        if result.is_ok() {
            self.tasks
                .project_occurrences()
                .map_err(MachineError::Heap)?;
        }
        self.logical
            .handoff(&mut self.arena.borrow_mut(), &self.heap)
            .map_err(MachineError::Heap)?;
        self.publish_logical_meter().map_err(MachineError::Heap)?;
        if self.ended {
            self.release_garbage();
        }
        self.observe_phase(if matches!(result, Ok(Step::Parked(_))) {
            GuestPhase::Park
        } else {
            GuestPhase::Inactive
        });
        result
    }
    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.ended {
            return Err(DeliverError::Ended);
        }
        let key = ordinal_key(lash_kernel_doc::kernel_internals::wait_number(wait));
        let Some(mut pending) = self.waits.get(&key, self.heap.pages())? else {
            return if self.withdrawn_ever.get(&key, self.heap.pages())?.is_some() {
                Ok(Delivered::Dropped)
            } else if lash_kernel_doc::kernel_internals::wait_number(wait) < self.next_wait {
                Err(DeliverError::AlreadyDelivered { wait })
            } else {
                Err(DeliverError::UnknownWait { wait })
            };
        };
        match pending.phase {
            WaitPhase::Issued { .. } => return Err(DeliverError::UnknownWait { wait }),
            WaitPhase::Committed(_) => return Err(DeliverError::AlreadyDelivered { wait }),
            WaitPhase::Admitted => (),
        }
        use lash_kernel_state::paged::CommittedOutcomeRecord;
        let exe = std::sync::Arc::clone(&self.exe);
        let declared =
            |callable: &lash_kernel_value::names::CallableId| exe.admits_callable(callable);
        let decoder = lash_kernel_doc::Decoder {
            max_depth: self.limits.value_depth(),
            policy: self.program.document.numbers,
            declared: &declared,
        };
        let rejected =
            |_what: &str, refusal: lash_kernel_doc::DecodeRefusal| -> Result<Datum, DeliverError> {
                refusal
                    .error_datum()
                    .map(|error| Datum::Error(Box::new(error)))
                    .map_err(|e| DeliverError::Heap(HeapFault::Malformed(e.to_string())))
            };
        let frozen = match outcome {
            // A failure's data is decoded under the document's number
            // policy, as a result is, before the heap freezes it.
            Outcome::Failed(error) => {
                match decoder.decode(
                    &Datum::Error(Box::new(error)),
                    &lash_kernel_value::types::Type::Any,
                ) {
                    Ok(datum) => CommittedOutcomeRecord::Failed(
                        self.heap.freeze_datum(&datum).map_err(delivery_error)?,
                    ),
                    Err(mismatch) => CommittedOutcomeRecord::Rejected(
                        self.heap
                            .freeze_datum(&rejected("the result", mismatch)?)
                            .map_err(delivery_error)?,
                    ),
                }
            }
            Outcome::Completed(datum) => {
                let task = pending.task;
                let frame = self.frame(task).map_err(|error| {
                    DeliverError::Heap(HeapFault::Malformed(format!("{error:?}")))
                })?;
                let (effect, ty) = super::actions::waiting(&exe, frame)
                    .and_then(|(_, perform)| perform)
                    .ok_or_else(|| {
                        DeliverError::Heap(HeapFault::Malformed("outcome has no perform".into()))
                    })?;
                match decoder.decode(&datum, &ty) {
                    Ok(datum) => CommittedOutcomeRecord::Completed(
                        self.heap.freeze_datum(&datum).map_err(delivery_error)?,
                    ),
                    Err(mismatch) => CommittedOutcomeRecord::Rejected(
                        self.heap
                            .freeze_datum(&rejected(
                                &format!("the result of `{effect:?}`"),
                                mismatch,
                            )?)
                            .map_err(delivery_error)?,
                    ),
                }
            }
        };
        pending.phase = WaitPhase::Committed(frozen);
        let task = pending.task;
        self.waits.write(key, pending, self.heap.pages())?;
        self.logical_incoming(task, &Incoming::Outcome(wait))?;
        self.logical_flush()?;
        let requeue = self
            .frame(task)
            .map(|frame| super::actions::then_yields(&exe, frame))
            .map_err(|error| DeliverError::Heap(HeapFault::Malformed(format!("{error:?}"))))?;
        self.task(task)
            .map_err(|error| DeliverError::Heap(HeapFault::Malformed(format!("{error:?}"))))?
            .wake(Incoming::Outcome(wait), requeue);
        self.ready.push_back(task)?;
        Ok(Delivered::Accepted)
    }
    fn seal(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        self.logical_flush().map_err(SealError::Heap)?;
        let sealed = self.seal_projection(budget)?;
        self.projecting = sealed.is_none();
        Ok(sealed)
    }
    fn meters(&self) -> Meters {
        Meters {
            charged: (self.bounds.charge == crate::Charge::Exact).then_some(self.charged),
            memory: if self.bounds.memory_meter == crate::MemoryMeter::Off {
                0
            } else {
                self.logical_memory.get()
            },
            live_tasks: self.live_tasks,
        }
    }
}
fn sequence<T: lash_kernel_state::kernel_internals::tree::Cell>(root: SequenceRoot) -> Tree<T> {
    Tree::of_sequence(&root)
}
fn sequence_root<T: lash_kernel_state::kernel_internals::tree::Cell>(
    tree: &Tree<T>,
) -> Result<SequenceRoot, HeapFault> {
    tree.sequence_root()
}
fn delivery_error(error: HeapWriteError) -> DeliverError {
    match error {
        HeapWriteError::Fault(error) => DeliverError::Heap(error),
        other => DeliverError::Heap(HeapFault::Malformed(format!("frozen delivery: {other}"))),
    }
}

impl KernelMachine {
    pub(crate) fn open_with_layout(
        program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        mut source: Box<dyn PageSource>,
        layout: Layout,
    ) -> Result<Self, OpenError> {
        let census = match root.limits.memory {
            None => None,
            Some(_) => Some(session::restored_census(&mut *source, Some(root.heap))?),
        };
        Self::open_with_optional_census(program, bounds, root, source, census, layout)
    }

    /// Opens `root` with `census` for its accounting: the laws that hold a
    /// run to a census of their own.
    #[cfg(test)]
    pub(crate) fn open_with_census(
        program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
        layout: Layout,
    ) -> Result<Self, OpenError> {
        Self::open_with_optional_census(program, bounds, root, source, Some(census), layout)
    }

    fn open_with_optional_census(
        mut program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        source: Box<dyn PageSource>,
        census: Option<Box<dyn AllocationCensus>>,
        layout: Layout,
    ) -> Result<Self, OpenError> {
        let limits = lash_kernel_value::data_limits::KernelDataLimits::standard();
        // The checkpoint owns its policies and its limits, as its limiters
        // grew them, even on a worker configured otherwise (FIG-6768).
        let bounds = Bounds {
            memory_meter: match root.limits.memory {
                Some(_) => crate::MemoryMeter::On,
                None => crate::MemoryMeter::Off,
            },
            memory: root.limits.memory.unwrap_or(bounds.memory),
            charge: root.meters.charge(),
            charge_limit: root.limits.charge,
            ..bounds
        };
        let given = program.library.document_identity(&program.document);
        if root.document != given {
            return Err(OpenError::DocumentMismatch {
                parked: root.document,
                given,
            });
        }
        if root.link != program.link {
            return Err(OpenError::Malformed {
                problem: format!(
                    "checkpoint Link {} differs from supplied {}",
                    root.link, program.link
                ),
            });
        }
        if root.kernel != program.document.kernel() {
            return Err(OpenError::KernelVersion {
                parked: root.kernel,
                supported: program.document.kernel(),
            });
        }
        if root.closure != program.closure {
            return Err(OpenError::Malformed {
                problem: "the document's execution closure differs from the checkpoint's".into(),
            });
        }
        if !matches!(root.status, RootStatus::Parked) {
            return Err(OpenError::Malformed {
                problem: "an ended root cannot execute".into(),
            });
        }
        let source = check_program_helpers(root, &mut program, source)?;
        if let lash_kernel_doc::Selection::Restored(selection) = &program.selection {
            program.functions = selection
                .live
                .iter()
                .map(|callable| callable.function)
                .chain(selection.pins.iter().copied())
                .collect();
        }
        program
            .validate_links()
            .map_err(|error| OpenError::Malformed {
                problem: error.to_string(),
            })?;
        let library = program.library.in_charge(bounds.charge).in_layout(layout);
        let admitted = library.admit(&program.document, limits, &program.selection)?;
        let exe = admitted
            .executable(
                &program.document,
                &library,
                &program.documents,
                &program.functions,
                &program.instances,
            )
            .map_err(|missing| OpenError::MissingFunction {
                function: missing.0,
            })?;
        let mut heap =
            PagedHeap::open_header(root.heap, source, EditEpoch(0)).map_err(OpenError::Heap)?;
        heap.set_data_limits(limits);
        if bounds.memory_meter == crate::MemoryMeter::Off {
            heap.without_memory_accounting();
        } else {
            heap.set_census(census.ok_or_else(|| OpenError::Malformed {
                problem: "metered open has no census".into(),
            })?)
            .map_err(OpenError::Heap)?;
        }
        heap.set_family_base(Box::new(
            lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
        ));
        heap.restore_execution_roots(root);
        let access = heap.pages().clone();
        let census_refinements = heap.certificate().refinements;
        // What the run held when it was sealed, as the capacity meter
        // counted it: the run holds at least that until it sweeps.
        let capacity_floor = if bounds.memory_meter == crate::MemoryMeter::Off {
            0
        } else {
            heap.certificate().capacity
        };
        let arena: super::SharedArena = std::rc::Rc::new(std::cell::RefCell::new(
            lash_kernel_state::arena::Arena::from_ordinal(heap.next_ordinal()),
        ));
        arena.borrow_mut().grow_control(exe.admission_bytes());
        arena.borrow_mut().watch_logical_members();
        let logical =
            lash_kernel_state::kernel_internals::logical::Lifecycle::new(heap.logical_ledger());
        let mut machine = Self {
            logical: logical.clone(),
            logical_memory: Default::default(),
            active_link: program.link,
            closure: root.closure,
            owner: root.owner,
            document: Some(root.document),
            session_cell: matches!(root.target, lash_kernel_state::paged::CellTargetKind::Main),
            program,
            exe: Arc::clone(&exe),
            bounds,
            capacity_floor,
            limits,
            tasks: tasks::Tasks::open(
                logical.clone(),
                heap.task_root().clone(),
                access.clone(),
                std::rc::Rc::clone(&arena),
                exe,
            ),
            arena,
            ready: queue::Queue::open(sequence(root.ready), access.clone()),
            current: None,
            joins: heap.join_root().clone(),
            waits: open_tree(root.waits),
            next_wait: root.meters.waits_issued,
            issued: None,
            next_join: root.meters.next_join,
            cancellations: queue::Queue::open(sequence(root.unreported), access),
            withdrawn_ever: open_tree(root.withdrawn),
            modes: open_tree(root.reads),
            functions: root.functions,
            host: root.host.clone(),
            heap,
            session: super::session::Session::with_lifecycle(logical),
            pins: Pins::default(),
            projecting: false,
            charging: true,
            paced: false,
            region: None,
            slice_end: u64::MAX,
            sites: Default::default(),
            islands: Default::default(),
            charged: root.meters.charged.unwrap_or(0),
            live_tasks: root.meters.live_tasks,
            census_refinements,
            regs: registers::Registers::default(),
            storage: Storage::default(),
            end: None,
            ended: false,
            observer: None,
            ran: Vec::new(),
            raised: None,
            catches: Default::default(),
            #[cfg(test)]
            boundary: None,
            #[cfg(test)]
            boundaries: 0,
            save: None,
            captions: Vec::new(),
            function_origins: Default::default(),
            limiters: Limiters::default(),
        };
        machine.publish_logical_meter().map_err(OpenError::Heap)?;
        machine.capacity_counted();
        Ok(machine)
    }
}

impl KernelMachine {
    pub(crate) fn task_identity(
        &self,
        identity: lash_kernel_state::paged::TaskIdentityRef,
    ) -> Result<lash_kernel_value::sites::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
        identity.resolve(self.heap.pages())
    }
}

impl KernelMachine {
    /// Projects what changed and seals it within `budget`: `None` when the
    /// budget ran out first, every dirty mark kept for the next seal.
    fn seal_projection(&mut self, budget: SealBudget) -> Result<Option<SealedRoot>, SealError> {
        #[cfg(feature = "perf-witness")]
        let _seal = lash_kernel_state::kernel_internals::perf_witness::SealScope::enter();
        if self.ended {
            return Err(SealError::Ended);
        }
        self.task_table()
            .and_then(|tasks| tasks.project_occurrences())
            .map_err(SealError::Heap)?;
        self.publish_roots().map_err(|error| match error {
            Halt::Heap(fault) => SealError::Heap(*fault),
            other => SealError::Fault {
                problem: format!("{other:?}"),
            },
        })?;
        let access = self.heap.pages().clone();
        // The running task is first on reopen; retain the untouched ready tree.
        if let Some(task) = self.current.take() {
            self.ready.push_front(task).map_err(SealError::Heap)?;
        }
        let mut work = SealWork::new(budget);
        if !self
            .heap
            .seal_task_root(&mut work)
            .map_err(SealError::Heap)?
            || !self
                .ready
                .rows
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .waits
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .joins
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .withdrawn_ever
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .cancellations
                .rows
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .modes
                .seal(&access, &mut work)
                .map_err(SealError::Heap)?
            || !self
                .heap
                .seal_session_root(&mut work)
                .map_err(SealError::Heap)?
        {
            return Ok(None);
        }
        // Reserve the maximum bounded root before giving the rest to the heap.
        if !work.take(4096 + 128) {
            return Ok(None);
        }
        // The certificate records what the capacity meter counts the run
        // holding, which a run opened from this seal holds until it sweeps
        // (FIG-6791). Sealing changes nothing the running meter counts.
        if self.bounds.memory_meter != crate::MemoryMeter::Off {
            let held = self.capacity_held();
            self.heap.record_capacity(held);
        }
        if self
            .heap
            .seal(work.remaining)
            .map_err(SealError::Heap)?
            .is_none()
        {
            return Ok(None);
        }
        let document = match self.document {
            Some(document) => document,
            None => {
                let document = self.exe.identity();
                self.document = Some(document);
                document
            }
        };
        let root = CheckpointRoot {
            link: self.program.link,
            limits: lash_kernel_state::paged::SavedLimits {
                memory: (self.bounds.memory_meter == crate::MemoryMeter::On)
                    .then_some(self.bounds.memory),
                charge: self.bounds.charge_limit,
            },
            kernel: self.program.document.kernel(),
            document,
            closure: self.closure,
            helper_library: self.program.helpers().identity(),
            owner: self.owner,
            target: if self.session_cell {
                lash_kernel_state::paged::CellTargetKind::Main
            } else {
                lash_kernel_state::paged::CellTargetKind::Entry
            },
            functions: self.functions,
            status: RootStatus::Parked,
            heap: self.heap.sealed_root().ok_or_else(|| SealError::Fault {
                problem: "sealed heap has no header".into(),
            })?,
            tasks: keyed(self.heap.task_root()).map_err(SealError::Heap)?,
            ready: sequence_root(&self.ready.rows).map_err(SealError::Heap)?,
            waits: keyed(&self.waits).map_err(SealError::Heap)?,
            joins: keyed(&self.joins).map_err(SealError::Heap)?,
            withdrawn: keyed(&self.withdrawn_ever).map_err(SealError::Heap)?,
            unreported: sequence_root(&self.cancellations.rows).map_err(SealError::Heap)?,
            reads: keyed(&self.modes).map_err(SealError::Heap)?,
            session: keyed(self.heap.session_root()).map_err(SealError::Heap)?,
            host: self.host.clone(),
            meters: SavedMeters {
                charged: (self.bounds.charge == crate::Charge::Exact).then_some(self.charged),
                waits_issued: self.next_wait,
                next_join: self.next_join,
                live_tasks: self.live_tasks,
            },
        };
        let tokens = encode_body(&root);
        if tokens.len() > 4096 {
            return Err(SealError::Fault {
                problem: "checkpoint header exceeds the root bound".into(),
            });
        }
        let children = [
            Some(root.heap),
            root.tasks.page(),
            root.ready.page(),
            root.waits.page(),
            root.joins.page(),
            root.withdrawn.page(),
            root.unreported.page(),
            root.reads.page(),
            root.session.page(),
            root.functions.page(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let page = access
            .seal(PageKind::CHECKPOINT_ROOT, tokens, children)
            .map_err(SealError::Heap)?;
        self.tasks = tasks::Tasks::open(
            self.logical.clone(),
            self.heap.task_root().clone(),
            access,
            std::rc::Rc::clone(&self.arena),
            Arc::clone(&self.exe),
        );
        Ok(Some(SealedRoot { page, root }))
    }
}

/// Shared ordering for ordinary resume and foreign carry: establish the helper
/// proof before link admission can prepare executable helper bodies.
pub(crate) fn check_program_helpers(
    root: &CheckpointRoot,
    program: &mut Program,
    source: Box<dyn PageSource>,
) -> Result<Box<dyn PageSource>, OpenError> {
    use lash_kernel_state::paged::{
        HelperCompatibilityError, HelperPins, check_helper_compatibility,
    };
    let checked = if root.helper_library == program.helpers().identity() {
        check_helper_compatibility(
            root.helper_library,
            || unreachable!("equal image never walks"),
            program.helpers(),
        )
        .map(|()| source)
    } else if let lash_kernel_doc::Selection::Restored(selection) = &program.selection {
        check_helper_compatibility(
            root.helper_library,
            || Ok(HelperPins(selection.pins.clone())),
            program.helpers(),
        )
        .map(|()| source)
    } else {
        // A direct kernel embedder has no parent selection. Derive it before
        // admission, then require the caller to prepare restored code with it.
        let documents = program
            .instances
            .iter()
            .filter_map(|(link, instance)| {
                program
                    .documents
                    .get(&instance.document)
                    .map(|document| (*link, document.as_ref().clone()))
            })
            .collect();
        let imports = program
            .instances
            .iter()
            .flat_map(|(link, instance)| {
                instance
                    .slots
                    .iter()
                    .filter_map(move |(slot, fill)| match fill {
                        crate::interface::PreparedSlot::Function { callable, .. } => {
                            Some(((*link, *slot), *callable))
                        }
                        _ => None,
                    })
            })
            .collect();
        lash_kernel_state::kernel_internals::checkpoint(root.clone())
            .check_helper_compatibility(source, &documents, &imports, program.helpers())
            .map(|(source, selection)| {
                program.selection = selection;
                source
            })
    };
    checked.map_err(|error| match error {
        HelperCompatibilityError::Heap(fault) => OpenError::Heap(fault),
        HelperCompatibilityError::HelperUnavailable { reason, function } => {
            OpenError::HelperUnavailable { reason, function }
        }
    })
}
