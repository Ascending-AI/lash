//! VM construction, suspension and restore; the envelope codec stays in the parent.
use super::*;

impl<'a, H: ExecutionHost> Vm<'a, H> {
    #[cfg(test)]
    pub(crate) fn live_logical_bytes(&self) -> u64 {
        self.heap.live_logical_bytes()
    }

    #[cfg(test)]
    pub(crate) fn instructions_executed(&self) -> u64 {
        self.instructions_executed
    }

    fn new_heap(host: &H) -> Heap {
        let limit = match host.execution_bounds().memory_limit {
            ExecutionBound::Bounded(limit) => limit.get(),
            ExecutionBound::Unbounded => u64::MAX,
        };
        let mut heap = Heap::with_limit(limit);
        heap.set_gc_allocation_interval(host.vm_pacing().heap_gc_allocation_interval);
        heap.set_collect_every_allocation(host.collect_heap_every_allocation());
        heap
    }

    pub(crate) fn install_heap(&mut self, mut heap: Heap) {
        let limit = match self.host.execution_bounds().memory_limit {
            ExecutionBound::Bounded(limit) => limit.get(),
            ExecutionBound::Unbounded => u64::MAX,
        };
        heap.set_limit(limit);
        heap.set_gc_allocation_interval(self.host.vm_pacing().heap_gc_allocation_interval);
        heap.set_collect_every_allocation(self.host.collect_heap_every_allocation());
        self.execution_nonce = mint_execution_nonce(heap.allocations());
        self.heap = heap;
    }

    /// `scratch` is the recycled-buffer handoff — `Some(&mut scratch)` reuses
    /// the scratch execution's stack and iterator stack instead of allocating.
    pub(crate) fn new(
        program: &'a CompiledProgram,
        slots: SlotState,
        host: &'a H,
        scratch: Option<&mut ExecutionScratch>,
        mode: ExecutionMode,
    ) -> Self {
        let (stack, iter_stack) = match scratch {
            Some(scratch) => (
                std::mem::take(&mut scratch.stack),
                std::mem::take(&mut scratch.iter_stack),
            ),
            None => (Vec::new(), Vec::new()),
        };
        Self {
            chunk: &program.chunk,
            executable: &program.executable,
            ip: 0,
            stack,
            last_value: None,
            slots,
            host,
            mode: VmMode::from(mode),
            iter_stack,
            active_function: None,
            frames: Vec::new(),
            slot_scratch: None,
            guest_coercions: Vec::new(),
            projected_bindings: host.projected_bindings(),
            handlers: Vec::new(),
            finally_stack: Vec::new(),
            lashlang_execution_occurrences: FxHashMap::default(),
            profile: None,
            validation_plans: FxHashMap::default(),
            pending_error_span: None,
            instructions_executed: 0,
            heap: Self::new_heap(host),
            heap_initialized: false,
            pending_tools: PendingOperationMap::new(),
            execution_nonce: mint_execution_nonce(0),
            resume_point: VmResumePoint::NextInstruction,
            resume_loop_phase: None,
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
            #[cfg(test)]
            heapify_passes: 0,
        }
    }

    /// Captures all mutable execution state without consuming the VM.
    ///
    /// Projected values cross the boundary as the plain data they are: a
    /// resource projection as its name, declared type and `ResourceRef`, a
    /// scalar projection as its name and value (ADR 0132 §9). Refusing them instead —
    /// which is what this did before FIG-2865 — made an ordinary program that
    /// merely put a projected binding in a list unable to park at all.
    pub fn suspend(&mut self) -> Result<VmContinuation, ContinuationError> {
        validate_parked_await_bound(&self.resume_point)?;
        let roots = self.heap_roots();
        self.heap.collect(roots.iter());
        validate_values(&self.stack, "operand stack")?;
        validate_optional_value(self.last_value.as_ref(), "last value")?;
        for (index, value) in self.slots.values.iter().enumerate() {
            validate_optional_value(value.as_ref(), &format!("slot {index}"))?;
        }
        for (key, value) in self.slots.extras.iter() {
            validate_value(value, &format!("global `{key}`"))?;
        }
        for (index, finally) in self.finally_stack.iter().enumerate() {
            if let FinallyCompletion::Throw { value, .. } = &finally.completion {
                validate_value(value, &format!("finally {index} thrown value"))?;
            }
        }

        let iterator_stack = self
            .iter_stack
            .iter()
            .enumerate()
            .map(|(depth, iterator)| {
                iterator_to_continuation(iterator, &format!("iterator {depth}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut frame_stack = Vec::with_capacity(self.frames.len());
        for frame in &self.frames {
            let iterator_stack = frame
                .iter_stack
                .iter()
                .enumerate()
                .map(|(depth, iterator)| {
                    iterator_to_continuation(iterator, &format!("frame iterator {depth}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let return_target = match &frame.return_target {
                ReturnTarget::Direct => VmFrameReturnContinuation::Direct,
                // Unreachable at a suspension point: a hook cannot perform the
                // effect a continuation is captured after.
                ReturnTarget::Coercion(_) => {
                    return Err(ContinuationError::UnserializableValue {
                        location: "frame return target".to_string(),
                        variant: "guest coercion",
                    });
                }
                ReturnTarget::Callback(callback) => {
                    VmFrameReturnContinuation::Callback(Box::new(VmCallbackContinuation {
                        function: callback.function.clone(),
                        this_arg: callback.this_arg.clone(),
                        calls: callback.calls.clone(),
                        next_index: callback.next_index,
                        results: callback.results.clone(),
                        completion: callback_completion_continuation(&callback.completion),
                        allow_effects: callback.allow_effects,
                        live_url_search_params: callback.live_url_search_params,
                        array_like: callback.array_like.as_ref().map(|walk| VmArrayLikeWalk {
                            receiver: walk.receiver.clone(),
                            next: walk.next,
                            length: walk.length,
                            descending: walk.descending,
                            gated: walk.gated,
                            omit_receiver: walk.omit_receiver,
                        }),
                    }))
                }
            };
            frame_stack.push(VmFrameContinuation {
                return_instruction_pointer: frame.return_ip,
                function: frame
                    .function
                    .map(u32::try_from)
                    .transpose()
                    .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                operand_stack_base: frame.operand_stack_base,
                slots: frame.slots.values.clone(),
                globals: frame.slots.extras.clone(),
                iterator_stack,
                return_target,
            });
        }
        let handler_stack = self
            .handlers
            .iter()
            .map(|handler| {
                Ok(VmHandlerContinuation {
                    handler_instruction_pointer: handler.handler_ip,
                    finally_instruction_pointer: handler.finally_ip,
                    catches: handler.catches,
                    frame_depth: handler.frame_depth,
                    frame_function: handler
                        .frame_function
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                    operand_stack_depth: handler.stack_depth,
                    iterator_stack_depth: handler.iterator_depth,
                })
            })
            .collect::<Result<Vec<_>, ContinuationError>>()?;
        let finally_stack = self
            .finally_stack
            .iter()
            .map(|finally| {
                Ok(VmFinallyContinuation {
                    completion: match &finally.completion {
                        FinallyCompletion::Normal { resume_ip } => {
                            VmFinallyCompletionContinuation::Normal {
                                resume_instruction_pointer: *resume_ip,
                            }
                        }
                        FinallyCompletion::Throw { value, origin } => {
                            VmFinallyCompletionContinuation::Throw {
                                value: value.clone(),
                                origin: origin.as_ref().map(|origin| {
                                    VmPendingErrorOriginContinuation {
                                        error: origin.error.clone(),
                                        instruction_pointer: origin.instruction_ip,
                                        span: origin.span,
                                    }
                                }),
                            }
                        }
                    },
                    handler_stack_depth: finally.handler_depth,
                    frame_depth: finally.frame_depth,
                    frame_function: finally
                        .frame_function
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
                    operand_stack_depth: finally.stack_depth,
                })
            })
            .collect::<Result<Vec<_>, ContinuationError>>()?;

        let mut continuation = VmContinuation {
            format_version: VM_CONTINUATION_FORMAT_VERSION,
            executable: self.executable.clone(),
            reference_semantics: false,
            instruction_pointer: self.ip,
            active_function: self
                .active_function
                .map(u32::try_from)
                .transpose()
                .map_err(|_| ContinuationError::FunctionIndexOverflow)?,
            pending_tools: self.pending_tools.clone(),
            execution_nonce: self.execution_nonce,
            operand_stack: self.stack.clone(),
            last_value: self.last_value.clone(),
            slots: self.slots.values.clone(),
            globals: self.slots.extras.clone(),
            iterator_stack,
            frame_stack,
            handler_stack,
            finally_stack,
            occurrence_counters: self
                .lashlang_execution_occurrences
                .iter()
                .map(|(key, value)| (key.clone(), *value))
                .collect(),
            mode: self.mode.into(),
            profile: self.profile.as_ref().map(|profile| VmProfileContinuation {
                instruction_counts: profile.instruction_counts.to_vec(),
                instruction_times: profile.instruction_times.to_vec(),
                builtin_counts: profile.builtin_counts.to_vec(),
                builtin_times: profile.builtin_times.to_vec(),
            }),
            pending_error_span: self.pending_error_span,
            instructions_executed: self.instructions_executed,
            heap: VmHeapContinuation::new(self.heap.clone()),
            resume: self.resume_point.clone(),
            expired_functions: std::collections::BTreeSet::new(),
        };
        if validate_continuation(&continuation).is_err() {
            // The forest form could not hold this heap, so record the shared
            // graph form and re-validate under it. Reference semantics make a
            // shared heap ordinary (ADR 0096), not a dialect fact.
            continuation.reference_semantics = true;
            validate_continuation(&continuation)?;
        }
        Ok(continuation)
    }

    /// Reconstructs a VM at the saved instruction pointer using caller-supplied
    /// immutable bytecode and host dependencies.
    pub fn resume_from(
        continuation: VmContinuation,
        program: &'a CompiledProgram,
        host: &'a H,
    ) -> Result<Self, ContinuationError> {
        if continuation.format_version != VM_CONTINUATION_FORMAT_VERSION {
            return Err(ContinuationError::FormatVersionMismatch {
                expected: VM_CONTINUATION_FORMAT_VERSION,
                found: continuation.format_version,
            });
        }
        // The continuation resumes only the program that parked it: another
        // module, another entry, or the same entry compiled under another
        // build's contracts would run its saved instruction pointer and node
        // counters against different code (FIG-3571).
        if continuation.executable != program.executable {
            return Err(ContinuationError::ExecutableMismatch {
                expected: program.executable.clone(),
                found: continuation.executable,
            });
        }
        // `reference_semantics` records whether this continuation's heap is a
        // shared graph rather than a forest, not which language wrote it: every
        // program this build compiles runs ECMA reference semantics (ADR 0096),
        // and a forest-shaped heap is still the common case. A continuation
        // written by a pre-cutover build is refused by the format-version fence
        // above, so there is nothing left to compare here.
        validate_continuation(&continuation)?;
        validate_program_continuation(&continuation, &program.chunk)?;
        validate_resume_point(&continuation, program)?;
        let resume_loop_phase = match &continuation.resume {
            VmResumePoint::ReissueOperation { loop_phase, .. } => *loop_phase,
            VmResumePoint::NextInstruction => None,
        };
        let active_function = continuation.active_function.map(|index| index as usize);
        let active_slot_count = match active_function {
            Some(index) => program
                .chunk
                .functions
                .get(index)
                .ok_or(ContinuationError::UnknownFunction {
                    index: index as u32,
                })?
                .slot_names
                .len(),
            None => program.chunk.slot_names.len(),
        };
        if continuation.slots.len() != active_slot_count {
            return Err(ContinuationError::SlotCountMismatch {
                expected: active_slot_count,
                actual: continuation.slots.len(),
            });
        }
        for frame in &continuation.frame_stack {
            let expected = match frame.function {
                Some(index) => program
                    .chunk
                    .functions
                    .get(index as usize)
                    .ok_or(ContinuationError::UnknownFunction { index })?
                    .slot_names
                    .len(),
                None => program.chunk.slot_names.len(),
            };
            if frame.slots.len() != expected {
                return Err(ContinuationError::SlotCountMismatch {
                    expected,
                    actual: frame.slots.len(),
                });
            }
        }
        let bounds = host.execution_bounds();
        if continuation.frame_stack.len() as u64 > bounds.max_frame_depth.get() {
            return Err(ContinuationError::FrameDepthExceeded {
                limit: bounds.max_frame_depth.get(),
            });
        }
        if let ExecutionBound::Bounded(limit) = bounds.instruction_budget
            && continuation.instructions_executed > limit.get()
        {
            return Err(ContinuationError::InstructionBudgetExceeded { limit: limit.get() });
        }
        if let ExecutionBound::Bounded(limit) = bounds.memory_limit
            && continuation.heap.live_logical_bytes() > limit.get()
        {
            return Err(ContinuationError::MemoryLimitExceeded {
                limit: limit.get(),
                live: continuation.heap.live_logical_bytes(),
            });
        }
        let profile = continuation
            .profile
            .map(profile_from_continuation)
            .transpose()?;
        let iter_stack = continuation
            .iterator_stack
            .into_iter()
            .map(iterator_from_continuation)
            .collect();
        let handlers = continuation
            .handler_stack
            .into_iter()
            .map(|handler| ExceptionHandler {
                handler_ip: handler.handler_instruction_pointer,
                finally_ip: handler.finally_instruction_pointer,
                catches: handler.catches,
                frame_depth: handler.frame_depth,
                frame_function: handler.frame_function.map(|index| index as usize),
                stack_depth: handler.operand_stack_depth,
                iterator_depth: handler.iterator_stack_depth,
            })
            .collect();
        let finally_stack = continuation
            .finally_stack
            .into_iter()
            .map(|finally| FinallyState {
                completion: match finally.completion {
                    VmFinallyCompletionContinuation::Normal {
                        resume_instruction_pointer,
                    } => FinallyCompletion::Normal {
                        resume_ip: resume_instruction_pointer,
                    },
                    VmFinallyCompletionContinuation::Throw { value, origin } => {
                        FinallyCompletion::Throw {
                            value,
                            origin: origin.map(|origin| {
                                Box::new(PendingErrorOrigin {
                                    error: origin.error,
                                    instruction_ip: origin.instruction_pointer,
                                    span: origin.span,
                                })
                            }),
                        }
                    }
                },
                handler_depth: finally.handler_stack_depth,
                frame_depth: finally.frame_depth,
                frame_function: finally.frame_function.map(|index| index as usize),
                stack_depth: finally.operand_stack_depth,
            })
            .collect();
        let frames = continuation
            .frame_stack
            .into_iter()
            .map(|frame| CallFrame {
                return_ip: frame.return_instruction_pointer,
                function: frame.function.map(|index| index as usize),
                operand_stack_base: frame.operand_stack_base,
                slots: SlotState {
                    values: frame.slots,
                    extras: frame.globals,
                    extras_heapified: false,
                },
                iter_stack: frame
                    .iterator_stack
                    .into_iter()
                    .map(iterator_from_continuation)
                    .collect(),
                return_target: match frame.return_target {
                    VmFrameReturnContinuation::Direct => ReturnTarget::Direct,
                    VmFrameReturnContinuation::Callback(callback) => {
                        let VmCallbackContinuation {
                            function,
                            this_arg,
                            calls,
                            next_index,
                            results,
                            completion,
                            allow_effects,
                            live_url_search_params,
                            array_like,
                        } = *callback;
                        ReturnTarget::Callback(Box::new(CallbackDriver {
                            function,
                            this_arg,
                            calls,
                            next_index,
                            results,
                            completion: callback_completion_from_continuation(completion),
                            allow_effects,
                            live_url_search_params,
                            array_like: array_like.map(|walk| ArrayLikeWalk {
                                receiver: walk.receiver,
                                next: walk.next,
                                length: walk.length,
                                descending: walk.descending,
                                gated: walk.gated,
                                omit_receiver: walk.omit_receiver,
                            }),
                        }))
                    }
                },
            })
            .collect();
        let mut vm = Self {
            chunk: &program.chunk,
            executable: &program.executable,
            ip: continuation.instruction_pointer,
            stack: continuation.operand_stack,
            last_value: continuation.last_value,
            slots: SlotState {
                values: continuation.slots,
                extras: continuation.globals,
                extras_heapified: false,
            },
            host,
            mode: continuation.mode.into(),
            iter_stack,
            active_function,
            frames,
            slot_scratch: None,
            guest_coercions: Vec::new(),
            projected_bindings: host.projected_bindings(),
            handlers,
            finally_stack,
            lashlang_execution_occurrences: continuation.occurrence_counters.into_iter().collect(),
            profile,
            validation_plans: FxHashMap::default(),
            pending_error_span: continuation.pending_error_span,
            instructions_executed: continuation.instructions_executed,
            heap: {
                let mut heap = continuation.heap.into_heap();
                let limit = match bounds.memory_limit {
                    ExecutionBound::Bounded(limit) => limit.get(),
                    ExecutionBound::Unbounded => u64::MAX,
                };
                heap.set_limit(limit);
                heap.set_gc_allocation_interval(host.vm_pacing().heap_gc_allocation_interval);
                heap.set_collect_every_allocation(host.collect_heap_every_allocation());
                heap
            },
            heap_initialized: true,
            // A resumed VM records assignments from here on. Continuations are
            // only used by durable process segments, which run on their own
            // `State` and never recycle into an `ExecutionScratch`, so there are
            // no earlier marks to carry across the handover blob.
            pending_tools: continuation.pending_tools,
            execution_nonce: continuation.execution_nonce,
            resume_point: continuation.resume,
            resume_loop_phase,
            #[cfg(test)]
            test_suspension: TestSuspension::Disabled,
            #[cfg(test)]
            heapify_passes: 0,
        };
        // The host's projected bindings re-occupy their read-only root slots.
        // Every other projection decoded from the wire is plain data that
        // reads through its provider (ADR 0132 §9).
        vm.rebind_projected_slots(&host.projected_bindings());
        Ok(vm)
    }
}
