//! Safe points publish one bounded root naming the canonical record trees.
use super::*;
use crate::interface::CellStart;
use crate::paged::{OpenError, SealError, SealedRoot};
use lash_kernel_doc::graph_tokens::encode_body;
use lash_kernel_state::kernel_internals::tree::SealWork;
use lash_kernel_state::paged::{
    AllocationCensus, CheckpointRoot, EditEpoch, KeyedRoot, PageKind, PageSource, RootStatus,
    SavedMeters, SealBudget, SequenceRoot,
};

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
        root: &CheckpointRoot,
        admissions: Vec<lash_kernel_state::AdmittedWait>,
        source: Box<dyn PageSource>,
    ) -> Result<Self, OpenError> {
        Self::open_with_layout(program, bounds, root, admissions, source, Layout::default())
    }
    fn run(&mut self, host: &mut dyn Host, slice: u64) -> Result<Step, MachineError> {
        if self.ended {
            return Err(MachineError::Ended);
        }
        self.observe_phase(GuestPhase::Interpret);
        let result = self.run_slice(host, slice);
        self.regs.clear();
        if result.is_ok() {
            self.tasks
                .project_occurrences()
                .map_err(MachineError::Heap)?;
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
        let key = ordinal_key(wait.0);
        let Some(mut pending) = self.waits.get(&key, self.heap.pages())? else {
            return if self.withdrawn_ever.get(&key, self.heap.pages())?.is_some() {
                Ok(Delivered::Dropped)
            } else if wait.0 < self.next_wait {
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
        if matches!(
            pending.kind,
            lash_kernel_state::paged::WaitKindRecord::Sleep
        ) != matches!(outcome, Outcome::Elapsed)
        {
            return Err(DeliverError::WrongOutcome { wait });
        }
        use lash_kernel_state::paged::CommittedOutcomeRecord;
        let exe = std::sync::Arc::clone(&self.exe);
        let declared = |name: &_| exe.declared.contains_key(name);
        let decoder = lash_kernel_doc::Decoder {
            max_depth: self.limits.value_depth(),
            policy: self.program.document.manifest.numbers,
            declared: &declared,
        };
        let rejected = |what: &str, mismatch: lash_kernel_doc::DecodeMismatch| {
            Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
                kind: "effect_result".into(),
                message: format!("{what} does not fit the stated type: {mismatch}"),
                data: Datum::Null,
            }))
        };
        let frozen = match outcome {
            Outcome::Elapsed => CommittedOutcomeRecord::Elapsed,
            // A failure's data is decoded under the document's number
            // policy, as a result is, before the heap freezes it.
            Outcome::Failed(error) => {
                match decoder.decode(&Datum::Error(Box::new(error)), &lash_kernel_doc::Type::Any) {
                    Ok(datum) => CommittedOutcomeRecord::Failed(
                        self.heap.freeze_datum(&datum).map_err(delivery_error)?,
                    ),
                    Err(mismatch) => CommittedOutcomeRecord::Rejected(
                        self.heap
                            .freeze_datum(&rejected("the result", mismatch))
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
                            .freeze_datum(&rejected(&format!("the result of `{effect}`"), mismatch))
                            .map_err(delivery_error)?,
                    ),
                }
            }
        };
        pending.phase = WaitPhase::Committed(frozen);
        let task = pending.task;
        self.waits.write(key, pending, self.heap.pages())?;
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
        if self.ended {
            return Err(SealError::Ended);
        }
        self.tasks.project_occurrences().map_err(SealError::Heap)?;
        self.publish_roots().map_err(|error| SealError::Fault {
            problem: format!("{error:?}"),
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
                .withdrawn
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
            kernel: self.program.document.manifest.kernel,
            document,
            release: self.release,
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
            unreported: sequence_root(&self.withdrawn.rows).map_err(SealError::Heap)?,
            reads: keyed(&self.modes).map_err(SealError::Heap)?,
            session: keyed(self.heap.session_root()).map_err(SealError::Heap)?,
            host: self.host.clone(),
            meters: SavedMeters {
                charged: self.charged,
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
        self.tasks =
            tasks::Tasks::open(self.heap.task_root().clone(), access, Arc::clone(&self.exe));
        Ok(Some(SealedRoot { page, root }))
    }
    fn task_identity(
        &self,
        identity: lash_kernel_state::paged::TaskIdentityRef,
    ) -> Result<lash_kernel_doc::TaskIdentity, lash_kernel_doc::heap::HeapFault> {
        identity.resolve(self.heap.pages())
    }
    fn tasks(
        &self,
        root: KeyedRoot,
        filter: crate::paged::TaskFilter,
        after: Option<lash_kernel_doc::TaskId>,
        limit: u32,
    ) -> Result<crate::paged::TaskPage, lash_kernel_doc::heap::HeapFault> {
        use crate::paged::{TaskFilter, TaskPage};
        if limit == 0 || limit > 64 {
            return Err(lash_kernel_doc::heap::HeapFault::Malformed(
                "a task query examines 1 to 64 rows".into(),
            ));
        }
        let rows = Tree::<lash_kernel_state::paged::TaskRecord>::of_keyed(&root);
        let mut cursor = after;
        let mut tasks = Vec::new();
        let mut finished = after.is_some_and(|id| id.0 >= root.len().saturating_sub(1));
        for _ in 0..limit {
            let key = cursor.map(|id| ordinal_key(id.0));
            let Some((key, task)) = rows.after(key.as_deref(), self.heap.pages())? else {
                finished = true;
                break;
            };
            let id = lash_kernel_doc::TaskId(u64::from_be_bytes(key.try_into().map_err(|_| {
                lash_kernel_doc::heap::HeapFault::Malformed("task key is not an ordinal".into())
            })?));
            cursor = Some(id);
            let include = match filter {
                TaskFilter::All => true,
                TaskFilter::Live => !task.identity.is_main() && task.is_live(),
                TaskFilter::Outstanding => task.is_outstanding(),
            };
            if include {
                tasks.push((id, task.identity));
            }
            if id.0 + 1 >= root.len() {
                finished = true;
                break;
            }
        }
        Ok(TaskPage {
            tasks,
            after: cursor,
            finished,
        })
    }
    fn meters(&self) -> Meters {
        Meters {
            charged: self.charged,
            memory: self.heap.certificate().upper.0,
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
        admissions: Vec<lash_kernel_state::AdmittedWait>,
        mut source: Box<dyn PageSource>,
        layout: Layout,
    ) -> Result<Self, OpenError> {
        let census = session::restored_census(&mut *source, Some(root.heap), bounds.memory)?;
        Self::open_with_census(program, bounds, root, admissions, source, census, layout)
    }

    /// Opens `root` with `census` for its accounting: the laws that hold a
    /// run to a census of their own.
    pub(crate) fn open_with_census(
        program: Program,
        bounds: Bounds,
        root: &CheckpointRoot,
        _admissions: Vec<lash_kernel_state::AdmittedWait>,
        source: Box<dyn PageSource>,
        census: Box<dyn AllocationCensus>,
        layout: Layout,
    ) -> Result<Self, OpenError> {
        let limits = lash_kernel_doc::KernelDataLimits::standard();
        let library = program.library.in_layout(layout);
        let admitted = library.admit(&program.document, limits)?;
        let exe = admitted
            .executable(&program.document, &library)
            .map_err(|missing| OpenError::MissingFunction {
                function: missing.0,
            })?;
        let given = exe.identity();
        if root.document != given {
            return Err(OpenError::DocumentMismatch {
                parked: root.document,
                given,
            });
        }
        if root.kernel != program.document.manifest.kernel {
            return Err(OpenError::KernelVersion {
                parked: root.kernel,
                supported: program.document.manifest.kernel,
            });
        }
        if root.release != program.release {
            return Err(OpenError::Malformed {
                problem: "helper release differs from the checkpoint".into(),
            });
        }
        if !matches!(root.status, RootStatus::Parked) {
            return Err(OpenError::Malformed {
                problem: "an ended root cannot execute".into(),
            });
        }
        let mut heap =
            PagedHeap::open_header(root.heap, source, EditEpoch(0)).map_err(OpenError::Heap)?;
        heap.set_data_limits(limits);
        heap.set_census(census).map_err(OpenError::Heap)?;
        heap.set_family_base(Box::new(
            lash_kernel_state::kernel_internals::family_impl::FrozenBase::default(),
        ));
        heap.restore_execution_roots(root);
        let access = heap.pages().clone();
        let census_refinements = heap.certificate().refinements;
        Ok(Self {
            release: root.release,
            owner: root.owner,
            document: Some(root.document),
            costs: crate::costs::Costs::of_document(&program.document),
            session_cell: matches!(root.target, lash_kernel_state::paged::CellTargetKind::Main),
            program,
            exe: Arc::clone(&exe),
            bounds,
            limits,
            tasks: tasks::Tasks::open(heap.task_root().clone(), access.clone(), exe),
            ready: queue::Queue::open(sequence(root.ready), access.clone()),
            current: None,
            joins: heap.join_root().clone(),
            waits: open_tree(root.waits),
            next_wait: root.meters.waits_issued,
            issued: None,
            next_join: root.meters.next_join,
            withdrawn: queue::Queue::open(sequence(root.unreported), access),
            withdrawn_ever: open_tree(root.withdrawn),
            modes: open_tree(root.reads),
            functions: root.functions,
            host: root.host.clone(),
            heap,
            pins: Pins::default(),
            fresh: Vec::new(),
            charging: true,
            scalar_blocks: false,
            charged: root.meters.charged,
            live_tasks: root.meters.live_tasks,
            census_refinements,
            regs: registers::Registers::default(),
            storage: Storage::default(),
            steps: Default::default(),
            end: None,
            ended: false,
            observer: None,
            ran: Vec::new(),
            #[cfg(test)]
            boundary: None,
            #[cfg(test)]
            boundaries: 0,
            save: None,
        })
    }
}
