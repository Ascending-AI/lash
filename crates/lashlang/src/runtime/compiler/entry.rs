use super::*;

/// What an awaited-comprehension loop appends per accepted element.
#[derive(Clone)]
pub(super) enum ListComprehensionElement<'a> {
    /// The ordinary comprehension: evaluate the element and append its value.
    Value(&'a Expr),
    /// The aggregate-await comprehension: evaluate the call's receiver and
    /// arguments, append them as one `(receiver, args...)` tuple, and let the
    /// list batch after the loop start every call together.
    DeferredCall {
        receiver: &'a Expr,
        args: &'a [Expr],
        /// The [`AstPath`] of the receiver-call node `receiver`/`args` belong
        /// to, so deferred compilation still resolves their source spans.
        call_path: AstPath,
    },
}

impl Compiler {
    pub(crate) fn compile_program(program: &Program) -> (Chunk, CompileStats) {
        let stats = Rc::new(RefCell::new(CompileStats::default()));
        let mut compiler = Self::with_slots_and_stats(
            None,
            Rc::new(RefCell::new(SlotTable::default())),
            stats.clone(),
        );
        compiler.expression_source_spans = expression_source_spans(program);
        compiler.compile_program_block(program);
        let chunk = compiler.finish();
        let compile_stats = *stats.borrow();
        (chunk, compile_stats)
    }

    pub(crate) fn compile_linked_program(
        program: &Program,
        module_context: CompiledModuleContext,
        lashlang_execution_context: LashlangExecutionContext,
    ) -> (Chunk, CompileStats) {
        let stats = Rc::new(RefCell::new(CompileStats::default()));
        let mut compiler = Self::with_slots_and_stats(
            Some(module_context),
            Rc::new(RefCell::new(SlotTable::default())),
            stats.clone(),
        );
        compiler.lashlang_execution = Some(LashlangExecutionCompileContext {
            context: lashlang_execution_context,
            paths: lashlang_execution_paths(program),
            sites: Vec::new(),
        });
        compiler.expression_source_spans = expression_source_spans(program);
        compiler.compile_program_block(program);
        let chunk = compiler.finish();
        let compile_stats = *stats.borrow();
        (chunk, compile_stats)
    }

    pub(crate) fn compile_linked_process_program(
        program: &Program,
        module_context: CompiledModuleContext,
        lashlang_execution_context: LashlangExecutionContext,
    ) -> (Chunk, CompileStats) {
        let stats = Rc::new(RefCell::new(CompileStats::default()));
        let mut compiler = Self::with_slots_and_stats(
            Some(module_context),
            Rc::new(RefCell::new(SlotTable::default())),
            stats.clone(),
        );
        compiler.lashlang_execution = Some(LashlangExecutionCompileContext {
            context: lashlang_execution_context,
            paths: lashlang_execution_paths(program),
            sites: Vec::new(),
        });
        compiler.expression_source_spans = expression_source_spans(program);
        compiler.compile_program_block(program);
        let chunk = compiler.finish();
        let compile_stats = *stats.borrow();
        (chunk, compile_stats)
    }

    fn with_slots_and_stats(
        module_context: Option<CompiledModuleContext>,
        slots: Rc<RefCell<SlotTable>>,
        compile_stats: Rc<RefCell<CompileStats>>,
    ) -> Self {
        Self {
            module_context,
            lashlang_execution: None,
            expression_source_spans: FxHashMap::default(),
            code: Vec::new(),
            spans: Vec::new(),
            constants: Vec::new(),
            names: Vec::new(),
            name_lookup: FxHashMap::default(),
            slots,
            key_lists: Vec::new(),
            format_templates: Vec::new(),
            compiled_schemas: Vec::new(),
            assign_paths: Vec::new(),
            resource_operation_batches: Vec::new(),
            resource_operation_list_batches: Vec::new(),
            compile_stats,
            const_slots: Vec::new(),
            loop_contexts: Vec::new(),
            handler_scopes: Vec::new(),
            handler_scope_extents: Vec::new(),
            handler_chain_digests: Vec::new(),
            pending_finally_sites: Vec::new(),
            functions: Vec::new(),
            pending_functions: Vec::new(),
            declared_functions: FxHashMap::default(),
        }
    }

    fn finish(mut self) -> Chunk {
        let root_code_len = self.code.len();
        self.compile_pending_functions();
        let slot_names = self.slots.borrow().names.clone();
        let mut spans = self.spans;
        spans.resize(self.code.len(), None);
        let mut lashlang_execution_sites = self
            .lashlang_execution
            .map(|tracking| tracking.sites)
            .unwrap_or_default();
        lashlang_execution_sites.resize(self.code.len(), None);
        Chunk {
            code: self.code,
            spans,
            lashlang_execution_sites,
            constants: self.constants,
            names: self.names,
            slot_names,
            key_lists: self.key_lists,
            format_templates: self.format_templates,
            compiled_schemas: self.compiled_schemas,
            assign_paths: self.assign_paths,
            resource_operation_batches: self.resource_operation_batches,
            resource_operation_list_batches: self.resource_operation_list_batches,
            functions: self.functions,
            handler_chain_digests: self.handler_chain_digests,
            handler_scopes: {
                let mut scopes = self.handler_scope_extents;
                scopes.sort_unstable_by_key(|scope| scope.handler_ip);
                debug_assert!(
                    scopes
                        .windows(2)
                        .all(|pair| pair[0].handler_ip != pair[1].handler_ip),
                    "each exception scope owns a distinct handler target"
                );
                scopes
            },
            root_code_len,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "each pending function is taken and compiled exactly once from the reserved index in this same queue"
    )]
    fn compile_pending_functions(&mut self) {
        let mut next = 0;
        while next < self.pending_functions.len() {
            let pending = self.pending_functions[next]
                .take()
                .expect("pending function is compiled once");
            let body_path = pending.body_path;
            let definition = pending.definition;
            debug_assert_eq!(next, self.functions.len());

            let root_slots =
                std::mem::replace(&mut self.slots, Rc::new(RefCell::new(SlotTable::default())));
            let root_const_slots = std::mem::take(&mut self.const_slots);
            let root_loops = std::mem::take(&mut self.loop_contexts);
            let root_handler_scopes = std::mem::take(&mut self.handler_scopes);
            // Every scope a function body opens is closed inside it, so the
            // chain each function starts and ends with is the empty one. The
            // digest breakpoints are global to the chunk and carry across.
            debug_assert_eq!(
                self.handler_chain_digests
                    .last()
                    .map_or(EMPTY_HANDLER_CHAIN_DIGEST, |(_, digest)| *digest),
                EMPTY_HANDLER_CHAIN_DIGEST,
                "a function body begins with no handler installed"
            );

            let self_slot = definition.name.as_deref().map(|name| self.push_slot(name));
            let parameter_slots = definition
                .params
                .iter()
                .map(|name| self.push_slot(name))
                .collect::<Vec<_>>();
            let capture_slots = definition
                .captures
                .iter()
                .map(|name| self.push_slot(name))
                .collect::<Vec<_>>();
            let entry_ip = self.code.len();
            self.compile_expr(&definition.body, &body_path);
            self.code.push(Instruction::Return);
            let end_ip = self.code.len();
            let slot_names = self.slots.borrow().names.clone().into_boxed_slice();
            self.functions.push(CompiledFunction {
                entry_ip,
                end_ip,
                parameter_count: definition.params.len(),
                parameter_model: pending.parameter_model,
                capture_count: definition.captures.len(),
                self_slot,
                parameter_slots: parameter_slots.into_boxed_slice(),
                capture_slots: capture_slots.into_boxed_slice(),
                slot_names,
            });

            self.slots = root_slots;
            self.const_slots = root_const_slots;
            self.loop_contexts = root_loops;
            self.handler_scopes = root_handler_scopes;
            next += 1;
        }
    }

    pub(super) fn push_const(&mut self, value: Value) -> usize {
        let index = self.constants.len();
        self.constants.push(value);
        index
    }

    pub(super) fn emit_push_value(&mut self, value: Value) {
        match value {
            Value::Null => self.code.push(Instruction::PushNull),
            Value::Undefined => self.code.push(Instruction::PushUndefined),
            Value::Bool(value) => self.code.push(Instruction::PushBool(value)),
            Value::Number(value) => self.code.push(Instruction::PushNumber(value)),
            value => {
                let index = self.push_const(value);
                self.code.push(Instruction::PushConst(index));
            }
        }
    }

    pub(super) fn push_name(&mut self, name: &str) -> usize {
        let symbol = intern_symbol(name);
        if let Some(index) = self.name_lookup.get(&symbol) {
            return *index;
        }

        let index = self.names.len();
        self.names.push(Name {
            symbol,
            text: symbol_name(symbol),
        });
        self.name_lookup.insert(symbol, index);
        index
    }

    pub(super) fn push_slot(&mut self, name: &str) -> usize {
        let symbol = intern_symbol(name);
        let mut slots = self.slots.borrow_mut();
        if let Some(index) = slots.lookup.get(&symbol) {
            let index = *index;
            drop(slots);
            self.ensure_const_slot(index);
            return index;
        }
        let index = slots.names.len();
        slots.names.push(Name {
            symbol,
            text: symbol_name(symbol),
        });
        slots.lookup.insert(symbol, index);
        drop(slots);
        self.ensure_const_slot(index);
        index
    }

    pub(super) fn push_key_list<'a>(&mut self, keys: impl Iterator<Item = &'a str>) -> usize {
        let index = self.key_lists.len();
        let keys = keys
            .map(|key| self.push_name(key))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        self.key_lists.push(keys);
        index
    }

    pub(super) fn push_assign_path(&mut self, steps: &[AssignPathStep]) -> usize {
        let index = self.assign_paths.len();
        let mut dynamic_index_count = 0;
        let steps = steps
            .iter()
            .map(|step| match step {
                AssignPathStep::Field(field) => {
                    CompiledAssignPathStep::Field(self.push_name(field))
                }
                AssignPathStep::Index(_) => {
                    dynamic_index_count += 1;
                    CompiledAssignPathStep::Index
                }
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        self.assign_paths.push(CompiledAssignPath {
            steps,
            dynamic_index_count,
        });
        index
    }

    pub(super) fn push_resource_operation_batch(
        &mut self,
        batch: CompiledResourceOperationBatch,
    ) -> usize {
        let index = self.resource_operation_batches.len();
        self.resource_operation_batches.push(batch);
        index
    }

    pub(super) fn push_resource_operation_list_batch(
        &mut self,
        batch: CompiledResourceOperationListBatch,
    ) -> usize {
        let index = self.resource_operation_list_batches.len();
        self.resource_operation_list_batches.push(batch);
        index
    }

    pub(super) fn push_format_template(&mut self, template: &str, argc: usize) -> usize {
        let index = self.format_templates.len();
        self.format_templates
            .push(compile_format_template(template, argc));
        index
    }

    pub(super) fn push_compiled_schema(&mut self, schema: &Value) -> usize {
        let index = self.compiled_schemas.len();
        self.compiled_schemas.push(compile_schema_value(schema));
        index
    }

    fn ensure_const_slot(&mut self, slot: usize) {
        if self.const_slots.len() <= slot {
            self.const_slots.resize(slot + 1, None);
        }
    }

    pub(super) fn set_const_slot(&mut self, slot: usize, value: Option<Value>) {
        self.ensure_const_slot(slot);
        self.const_slots[slot] = value;
    }

    pub(super) fn clear_const_slots(&mut self) {
        self.const_slots.fill(None);
    }

    pub(super) fn const_for_slot(&self, slot: usize) -> Option<Value> {
        self.const_slots.get(slot).cloned().flatten()
    }

    pub(super) fn const_for_name(&self, name: &str) -> Option<Value> {
        let symbol = lookup_symbol(name)?;
        let slots = self.slots.borrow();
        let slot = *slots.lookup.get(&symbol)?;
        drop(slots);
        self.const_for_slot(slot)
    }

    pub(super) fn resolve_intrinsic(&mut self, name: &str, argc: usize) -> IntrinsicOp {
        // Unknown names are not arity-checked here; they fall through to
        // `IntrinsicOp::Unknown`. Known builtins must satisfy their registered
        // arity or compile to `InvalidArity`.
        if let Some(builtin) = crate::builtins::lookup(name)
            && !builtin.arity.accepts(argc)
        {
            return IntrinsicOp::InvalidArity {
                name: self.push_name(name),
                argc,
            };
        }
        intrinsic_for_builtin(name, argc).unwrap_or_else(|| IntrinsicOp::Unknown {
            name: self.push_name(name),
            argc,
        })
    }

    /// Registers every declared `fn` as a compiled function before any code is
    /// emitted.
    ///
    /// Registration is deliberately separate from emission. Bodies are compiled
    /// by `compile_pending_functions` *after* the root code, so declaring a
    /// function adds no instruction ahead of `main` and shifts no root ip: a
    /// program that gains an unused declaration produces a byte-identical
    /// continuation. Registering all of them up front is also what makes the
    /// call graph order-free — a body reaches any other declared function, in
    /// either direction, because a call site names a chunk function index
    /// rather than a value that had to exist first.
    fn register_declared_functions(&mut self, program: &Program) {
        for (declaration_index, declaration) in program.declarations.iter().enumerate() {
            let Declaration::Function(function) = declaration else {
                continue;
            };
            let index = self.pending_functions.len();
            let definition = FunctionExpr {
                // No self-binding slot: a recursive call re-materializes the
                // callee from the chunk, so the body never needs to see itself
                // as a value.
                name: None,
                params: function
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect(),
                captures: Vec::new(),
                body: Box::new(function.body.clone()),
            };
            // The clone shares its original's [`AstPath`]: node identity is
            // structural, so the deferred body's facts resolve without any
            // copying.
            let body_path = AstPath::declaration(declaration_index as u32, Vec::new());
            self.pending_functions.push(Some(PendingFunction {
                definition,
                body_path,
                parameter_model: ClosureParameterModel::Exact,
            }));
            self.declared_functions
                .insert(function.name.to_string(), index);
        }
    }

    fn compile_program_block(&mut self, program: &Program) {
        self.register_declared_functions(program);
        let main_path = AstPath::main(Vec::new());
        let last_statement = match &program.main {
            Expr::Block(expressions) => {
                self.compile_block_value_with_spans(expressions, &main_path);
                expressions
                    .len()
                    .checked_sub(1)
                    .map(|index| main_path.child(index as u32))
            }
            expression => {
                self.compile_expr(expression, &main_path);
                Some(main_path.clone())
            }
        };
        if !is_terminal_expr(&program.main) {
            let pop = self.code.len();
            self.code.push(Instruction::Pop);
            if let Some(span) = last_statement.and_then(|path| self.expression_source_span(&path)) {
                self.mark_instruction_spans(pop, self.code.len(), span);
            }
        }
    }

    pub(super) fn compile_block_value(&mut self, expressions: &[Expr], path: &AstPath) {
        let Some((last_index, last)) = expressions.iter().enumerate().next_back() else {
            self.code.push(Instruction::PushNull);
            return;
        };
        for (index, expression) in expressions.iter().enumerate().take(last_index) {
            self.compile_expr_discarding_value(expression, &path.child(index as u32));
        }
        self.compile_expr(last, &path.child(last_index as u32));
    }

    fn compile_block_value_with_spans(&mut self, expressions: &[Expr], path: &AstPath) {
        let Some((last_index, last)) = expressions.iter().enumerate().next_back() else {
            self.code.push(Instruction::PushNull);
            return;
        };
        for (index, expression) in expressions.iter().enumerate().take(last_index) {
            let expression_path = path.child(index as u32);
            let span = self.expression_source_span(&expression_path);
            self.compile_expr_discarding_value_with_span(expression, &expression_path, span);
        }
        let last_path = path.child(last_index as u32);
        let span = self.expression_source_span(&last_path);
        self.compile_expr_with_span(last, &last_path, span);
    }

    fn compile_expr_with_span(&mut self, expression: &Expr, path: &AstPath, span: Option<Span>) {
        let start = self.code.len();
        self.compile_expr(expression, path);
        if let Some(span) = span {
            self.mark_instruction_spans(start, self.code.len(), span);
        }
    }

    fn compile_expr_discarding_value_with_span(
        &mut self,
        expression: &Expr,
        path: &AstPath,
        span: Option<Span>,
    ) {
        let start = self.code.len();
        self.compile_expr_discarding_value(expression, path);
        if let Some(span) = span {
            self.mark_instruction_spans(start, self.code.len(), span);
        }
    }

    fn compile_expr_discarding_value(&mut self, expression: &Expr, path: &AstPath) {
        match expression {
            Expr::LabelAnnotated { label, expr } => {
                if self.try_compile_label_as_effect_step(expr, label, false, &path.child(0)) {
                    return;
                }
                if !label_attaches_to_concrete_node(expr) {
                    self.emit_lashlang_execution_step(&path.child(0), label);
                }
                self.compile_expr_discarding_value(expr, &path.child(0));
            }
            Expr::Block(expressions) => {
                for (index, expression) in expressions.iter().enumerate() {
                    self.compile_expr_discarding_value(expression, &path.child(index as u32));
                }
            }
            Expr::Assign { target, expr } => {
                self.compile_assignment_expr(target, expr, false, path)
            }
            Expr::For {
                binding,
                iterable,
                body,
            } => self.compile_for_expr(binding, iterable, body, false, path),
            Expr::While { condition, body } => {
                self.compile_while_expr(condition, body, false, path)
            }
            Expr::Finish(_) | Expr::Fail(_) | Expr::Return(_) | Expr::Break | Expr::Continue => {
                self.compile_expr(expression, path);
            }
            expression => {
                self.compile_expr(expression, path);
                self.code.push(Instruction::Pop);
            }
        }
    }

    fn mark_instruction_spans(&mut self, start: usize, end: usize, span: Span) {
        if self.spans.len() < end {
            self.spans.resize(end, None);
        }
        for instruction_span in &mut self.spans[start..end] {
            if instruction_span.is_none() {
                *instruction_span = Some(span);
            }
        }
    }

    pub(super) fn mark_instruction_source_span(&mut self, instruction: usize, path: &AstPath) {
        let Some(span) = self.expression_source_span(path) else {
            return;
        };
        if self.spans.len() <= instruction {
            self.spans.resize(instruction + 1, None);
        }
        self.spans[instruction] = Some(span);
    }

    pub(super) fn expression_source_span(&self, path: &AstPath) -> Option<Span> {
        self.expression_source_spans.get(path).copied()
    }

    pub(super) fn mark_lashlang_execution_site(
        &mut self,
        instruction: usize,
        site: LashlangExecutionSite,
    ) {
        let Some(tracking) = self.lashlang_execution.as_mut() else {
            return;
        };
        if tracking.sites.len() <= instruction {
            tracking.sites.resize(instruction + 1, None);
        }
        tracking.sites[instruction] = Some(site);
    }

    pub(super) fn lashlang_execution_site_for_expr(
        &self,
        expression: &Expr,
        path: &AstPath,
    ) -> Option<LashlangExecutionSite> {
        self.lashlang_execution_site_for_descriptor(path, expression)
    }

    pub(super) fn lashlang_execution_site_for_descriptor(
        &self,
        path: &AstPath,
        descriptor_expression: &Expr,
    ) -> Option<LashlangExecutionSite> {
        let tracking = self.lashlang_execution.as_ref()?;
        let path = tracking.paths.get(path)?;
        let (kind, label) = execution_site_descriptor(descriptor_expression)?;
        Some(if kind == BRANCH_EXECUTION_SITE_KIND {
            tracking.context.builder().branch_site(path)
        } else {
            tracking.context.builder().node_site(path, kind, label)
        })
    }

    pub(super) fn labeled_step_execution_site(
        &self,
        path: &AstPath,
        label: &str,
    ) -> Option<LashlangExecutionSite> {
        let tracking = self.lashlang_execution.as_ref()?;
        let path = tracking.paths.get(path)?;
        Some(
            tracking
                .context
                .builder()
                .node_site(path, STEP_EXECUTION_SITE_KIND, label),
        )
    }

    pub(super) fn emit_lashlang_execution_step(&mut self, path: &AstPath, label: &LabelMetadata) {
        let instruction = self.code.len();
        self.code.push(Instruction::ObserveStep);
        if let Some(site) = self.labeled_step_execution_site(path, label.title.as_str()) {
            self.mark_lashlang_execution_site(instruction, site);
        }
    }

    fn compile_block_discarding_values(&mut self, block: &Expr, path: &AstPath) {
        match block {
            Expr::Block(expressions) => {
                for (index, expression) in expressions.iter().enumerate() {
                    self.compile_expr_discarding_value(expression, &path.child(index as u32));
                }
            }
            expression => {
                self.compile_expr_discarding_value(expression, path);
            }
        }
    }

    pub(super) fn push_null_if(&mut self, leave_value: bool) {
        if leave_value {
            self.code.push(Instruction::PushNull);
        }
    }

    /// `expr` is the `Assign`'s value child: it sits after the target's
    /// dynamic index steps in `children()` order, so its path is
    /// `path.child(value_index)`.
    pub(super) fn assign_value_index(target: &AssignTarget) -> usize {
        target
            .steps
            .iter()
            .filter(|step| matches!(step, AssignPathStep::Index(_)))
            .count()
    }

    pub(super) fn compile_assignment_expr(
        &mut self,
        target: &AssignTarget,
        expr: &Expr,
        leave_value: bool,
        path: &AstPath,
    ) {
        let value_path = || path.child(Self::assign_value_index(target) as u32);
        if target.is_simple() {
            let name = &target.root;
            let slot = self.push_slot(name);

            if let Expr::Binary {
                left,
                op: BinaryOp::Add,
                right,
            } = expr
                && matches!(left.as_ref(), Expr::Variable(var) if var == name)
            {
                if let Expr::List(items) = right.as_ref()
                    && items.len() == 1
                {
                    // The optimized single-item concat is an insertion like any
                    // other: the entering item is isolated before it joins the
                    // accumulator.
                    self.compile_expr(&items[0], &value_path().child(1).child(0));
                    self.code.push(Instruction::AppendAssign(slot));
                    self.set_const_slot(slot, None);
                    self.push_null_if(leave_value);
                    return;
                }
                if let Some(Value::Number(right)) = self.fold_compile_time_expr(right) {
                    self.code.push(Instruction::AddAssignNumber { slot, right });
                    self.set_const_slot(slot, None);
                    self.push_null_if(leave_value);
                    return;
                }
                if let Expr::Variable(right_name) = right.as_ref() {
                    let right = self.push_slot(right_name);
                    self.code.push(Instruction::AddAssignSlot { slot, right });
                    self.set_const_slot(slot, None);
                    self.push_null_if(leave_value);
                    return;
                }
                // A general concat copies the right operand's members into the
                // accumulator. The copy happens per member at the insertion
                // itself, so the operand does not need isolating as a whole.
                self.compile_expr(right, &value_path().child(1));
                self.code.push(Instruction::AddAssign(slot));
                self.set_const_slot(slot, None);
                self.push_null_if(leave_value);
                return;
            }
            if let Expr::BuiltinCall {
                name: builtin_name,
                args,
            } = expr
                && builtin_name == "push"
                && let [Expr::Variable(first_arg), item] = args.as_slice()
                && first_arg == name
            {
                self.compile_expr(item, &value_path().child(1));
                self.code
                    .push(Instruction::Intrinsic(IntrinsicOp::PushAssign(slot)));
                self.set_const_slot(slot, None);
                self.push_null_if(leave_value);
                return;
            }
            self.compile_expr(expr, &value_path());
            self.code.push(Instruction::StoreName(slot));
            self.set_const_slot(slot, None);
            self.push_null_if(leave_value);
            return;
        }

        let slot = self.push_slot(&target.root);
        if let [AssignPathStep::Index(index)] = target.steps.as_slice()
            && is_pure_expr(index)
            && let Expr::Binary {
                left,
                op: BinaryOp::Add,
                right,
            } = expr
            && let Expr::Index {
                target: left_target,
                index: left_index,
            } = left.as_ref()
            && matches!(left_target.as_ref(), Expr::Variable(name) if name == target.root)
            && left_index.as_ref() == index
            && let Some(Value::Number(right)) = self.fold_compile_time_expr(right)
        {
            if let Expr::Variable(index_name) = index {
                let index = self.push_slot(index_name);
                self.code
                    .push(Instruction::AddAssignIndexSlotNumber { slot, index, right });
            } else {
                self.compile_expr(index, &path.child(0));
                self.code
                    .push(Instruction::AddAssignIndexNumber { slot, right });
            }
            self.set_const_slot(slot, None);
            self.push_null_if(leave_value);
            return;
        }
        let mut index_child = 0usize;
        for step in &target.steps {
            if let AssignPathStep::Index(index) = step {
                self.compile_expr(index, &path.child(index_child as u32));
                index_child += 1;
            }
        }
        self.compile_expr(expr, &value_path());
        let path = self.push_assign_path(&target.steps);
        self.code.push(Instruction::HeapPathAssign { slot, path });
        self.set_const_slot(slot, None);
        self.push_null_if(leave_value);
    }

    pub(super) fn compile_for_expr(
        &mut self,
        binding: &str,
        iterable: &Expr,
        body: &Expr,
        leave_value: bool,
        path: &AstPath,
    ) {
        let binding = self.push_slot(binding);
        if let Expr::BuiltinCall { name, args } = iterable
            && name.as_str() == "range"
        {
            for (index, arg) in args.iter().enumerate() {
                self.compile_expr(arg, &path.child(0).child(index as u32));
            }
            self.clear_const_slots();
            self.set_const_slot(binding, None);
            self.code.push(Instruction::BeginRangeIter {
                binding,
                argc: args.len(),
            });
            self.compile_for_loop_body(body, &path.child(1));
            self.push_null_if(leave_value);
            return;
        }

        self.compile_expr(iterable, &path.child(0));
        self.clear_const_slots();
        self.set_const_slot(binding, None);
        self.code.push(Instruction::BeginIter(binding));
        self.compile_for_loop_body(body, &path.child(1));
        self.push_null_if(leave_value);
    }

    #[expect(
        clippy::expect_used,
        reason = "the loop context pushed a few lines above is popped exactly once at the end of the body"
    )]
    fn compile_for_loop_body(&mut self, body: &Expr, body_path: &AstPath) {
        let loop_start = self.code.len();
        let iter_next = self.code.len();
        self.code.push(Instruction::IterNext {
            jump_to: usize::MAX,
        });
        self.loop_contexts.push(LoopContext {
            continue_target: loop_start,
            break_jumps: SmallVec::new(),
            handler_scope_depth: self.handler_scopes.len(),
        });
        self.compile_block_discarding_values(body, body_path);
        let loop_context = self
            .loop_contexts
            .pop()
            .expect("loop context should exist while compiling `for`");
        self.code.push(Instruction::Jump(loop_start));
        let loop_end = self.code.len();
        self.code.push(Instruction::EndIter);
        self.patch_jump(iter_next, loop_end);
        for break_jump in loop_context.break_jumps {
            self.patch_jump(break_jump, loop_end);
        }
        self.clear_const_slots();
    }

    /// `path` is the comprehension node's path. In `children()` order the
    /// clause expressions come first (`For` contributes its iterable, `If`
    /// its condition) and the element is last.
    pub(super) fn compile_list_comprehension(
        &mut self,
        element: ListComprehensionElement<'_>,
        clauses: &[ListComprehensionClause],
        path: &AstPath,
    ) {
        let element_path = path.child(clauses.len() as u32);
        self.compile_list_comprehension_with(
            &mut |compiler| match &element {
                ListComprehensionElement::Value(element) => {
                    compiler.compile_expr(element, &element_path)
                }
                ListComprehensionElement::DeferredCall {
                    receiver,
                    args,
                    call_path,
                } => {
                    compiler.compile_expr(receiver, &call_path.child(0));
                    for (index, arg) in args.iter().enumerate() {
                        compiler.compile_expr(arg, &call_path.child(index as u32 + 1));
                    }
                    compiler.code.push(Instruction::BuildTuple(args.len() + 1));
                }
            },
            clauses,
            path,
        );
    }

    pub(super) fn compile_list_comprehension_with(
        &mut self,
        element: &mut dyn FnMut(&mut Self),
        clauses: &[ListComprehensionClause],
        path: &AstPath,
    ) {
        self.code.push(Instruction::BuildList(0));
        self.compile_list_comprehension_clause(element, clauses, 0, path);
        self.clear_const_slots();
    }

    fn compile_list_comprehension_clause(
        &mut self,
        element: &mut dyn FnMut(&mut Self),
        clauses: &[ListComprehensionClause],
        index: usize,
        path: &AstPath,
    ) {
        let Some(clause) = clauses.get(index) else {
            element(self);
            self.code.push(Instruction::ListAppend);
            return;
        };

        let clause_path = path.child(index as u32);
        match clause {
            ListComprehensionClause::For { binding, iterable } => {
                self.compile_list_comprehension_for(
                    binding,
                    iterable,
                    element,
                    clauses,
                    index + 1,
                    path,
                    &clause_path,
                );
            }
            ListComprehensionClause::If { condition } => {
                let jump_to_next_iteration =
                    self.compile_condition_jump_if_false(condition, &clause_path);
                self.clear_const_slots();
                self.compile_list_comprehension_clause(element, clauses, index + 1, path);
                self.patch_jump(jump_to_next_iteration, self.code.len());
                self.clear_const_slots();
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "comprehension compilation carries its loop state through the recursion"
    )]
    fn compile_list_comprehension_for(
        &mut self,
        binding: &str,
        iterable: &Expr,
        element: &mut dyn FnMut(&mut Self),
        clauses: &[ListComprehensionClause],
        next_clause: usize,
        path: &AstPath,
        iterable_path: &AstPath,
    ) {
        let binding = self.push_slot(binding);
        if let Expr::BuiltinCall { name, args } = iterable
            && name.as_str() == "range"
        {
            for (index, arg) in args.iter().enumerate() {
                self.compile_expr(arg, &iterable_path.child(index as u32));
            }
            self.clear_const_slots();
            self.set_const_slot(binding, None);
            self.code.push(Instruction::BeginRangeIter {
                binding,
                argc: args.len(),
            });
            self.compile_list_comprehension_for_body(element, clauses, next_clause, path);
            return;
        }

        self.compile_expr(iterable, iterable_path);
        self.clear_const_slots();
        self.set_const_slot(binding, None);
        self.code.push(Instruction::BeginIter(binding));
        self.compile_list_comprehension_for_body(element, clauses, next_clause, path);
    }

    fn compile_list_comprehension_for_body(
        &mut self,
        element: &mut dyn FnMut(&mut Self),
        clauses: &[ListComprehensionClause],
        next_clause: usize,
        path: &AstPath,
    ) {
        let loop_start = self.code.len();
        let iter_next = self.code.len();
        self.code.push(Instruction::IterNext {
            jump_to: usize::MAX,
        });
        self.compile_list_comprehension_clause(element, clauses, next_clause, path);
        self.code.push(Instruction::Jump(loop_start));
        let loop_end = self.code.len();
        self.code.push(Instruction::EndIter);
        self.patch_jump(iter_next, loop_end);
        self.clear_const_slots();
    }

    #[expect(
        clippy::expect_used,
        reason = "the loop context pushed a few lines above is popped exactly once at the end of the body"
    )]
    pub(super) fn compile_while_expr(
        &mut self,
        condition: &Expr,
        body: &Expr,
        leave_value: bool,
        path: &AstPath,
    ) {
        self.clear_const_slots();
        let loop_start = self.code.len();
        let jump_to_end = self.compile_condition_jump_if_false(condition, &path.child(0));
        self.clear_const_slots();
        self.loop_contexts.push(LoopContext {
            continue_target: loop_start,
            break_jumps: SmallVec::new(),
            handler_scope_depth: self.handler_scopes.len(),
        });
        self.compile_block_discarding_values(body, &path.child(1));
        let loop_context = self
            .loop_contexts
            .pop()
            .expect("loop context should exist while compiling `while`");
        self.code.push(Instruction::Jump(loop_start));
        let loop_end = self.code.len();
        self.patch_jump(jump_to_end, loop_end);
        for break_jump in loop_context.break_jumps {
            self.patch_jump(break_jump, loop_end);
        }
        self.clear_const_slots();
        self.push_null_if(leave_value);
    }

    pub(super) fn fold_compile_time_expr(&self, expr: &Expr) -> Option<Value> {
        match expr {
            Expr::ProcessLiteral(_) => None,
            Expr::LabelAnnotated { expr, .. } => self.fold_compile_time_expr(expr),
            Expr::Null => Some(Value::Null),
            Expr::Undefined => Some(Value::Undefined),
            Expr::Bool(value) => Some(Value::Bool(*value)),
            Expr::Number(value) => Some(Value::Number(*value)),
            Expr::String(value) => Some(Value::String(value.clone())),
            Expr::ResourceRef(resource) => {
                Some(Value::Resource(crate::runtime::ResourceHandle::new(
                    resource.resource_type.to_string(),
                    resource.alias.to_string(),
                )))
            }
            Expr::ProcessRef { .. } | Expr::HostDescriptorConstructor { .. } => None,
            Expr::Variable(name) => self.const_for_name(name),
            Expr::Tuple(items) => Some(Value::Tuple(
                items
                    .iter()
                    .map(|item| self.fold_compile_time_expr(item))
                    .collect::<Option<Vec<_>>>()?
                    .into(),
            )),
            Expr::List(items) => Some(Value::List(
                items
                    .iter()
                    .map(|item| self.fold_compile_time_expr(item))
                    .collect::<Option<Vec<_>>>()?
                    .into(),
            )),
            Expr::ListComprehension { .. } => None,
            Expr::Record(entries) => {
                let mut record = record_with_capacity(entries.len());
                for (key, value) in entries {
                    record.insert(key.to_string(), self.fold_compile_time_expr(value)?);
                }
                Some(Value::Record(Arc::new(record)))
            }
            Expr::BuiltinCall { name, args } => {
                let values = args
                    .iter()
                    .map(|arg| self.fold_compile_time_expr(arg))
                    .collect::<Option<Vec<_>>>()?;
                let builtin = intrinsic_for_builtin(name.as_str(), args.len())?;
                match builtin {
                    IntrinsicOp::Len => {
                        if values.len() == 1 {
                            execute_len_direct(&values[0]).ok()
                        } else {
                            None
                        }
                    }
                    IntrinsicOp::Range(_) => execute_range_builtin(&values).ok(),
                    IntrinsicOp::CeilDiv => {
                        execute_integer_div_builtin("ceil_div", &values, f64::ceil).ok()
                    }
                    IntrinsicOp::FloorDiv => {
                        execute_integer_div_builtin("floor_div", &values, f64::floor).ok()
                    }
                    IntrinsicOp::Push => {
                        if let [Value::List(items), item] = values.as_slice() {
                            let mut values = items.to_vec();
                            values.push(item.clone());
                            Some(Value::List(values.into()))
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            Expr::Field { target, field } => {
                let target = self.fold_compile_time_expr(target)?;
                read_javascript_field_direct(target, &transient_name(field)).ok()
            }
            Expr::Index { target, index } => {
                let target = self.fold_compile_time_expr(target)?;
                let index = self.fold_compile_time_expr(index)?;
                read_javascript_index_direct(target, index).ok()
            }
            Expr::Unary { op, expr } => {
                let value = self.fold_compile_time_expr(expr)?;
                match op {
                    UnaryOp::Negate => Some(Value::Number(-as_number(&value).ok()?)),
                    UnaryOp::Not => Some(Value::Bool(!is_truthy(&value).ok()?)),
                }
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                if is_truthy(&self.fold_compile_time_expr(condition)?).ok()? {
                    self.fold_compile_time_expr(then_block)
                } else {
                    self.fold_compile_time_expr(else_block)
                }
            }
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    let left = self.fold_compile_time_expr(left)?;
                    if !is_truthy(&left).ok()? {
                        Some(Value::Bool(false))
                    } else {
                        Some(Value::Bool(
                            is_truthy(&self.fold_compile_time_expr(right)?).ok()?,
                        ))
                    }
                }
                BinaryOp::Or => {
                    let left = self.fold_compile_time_expr(left)?;
                    if is_truthy(&left).ok()? {
                        Some(Value::Bool(true))
                    } else {
                        Some(Value::Bool(
                            is_truthy(&self.fold_compile_time_expr(right)?).ok()?,
                        ))
                    }
                }
                _ => {
                    let left = self.fold_compile_time_expr(left)?;
                    let right = self.fold_compile_time_expr(right)?;
                    eval_binary_values(left, *op, right).ok()
                }
            },
            Expr::JavaScriptUnary { op, expr } => {
                eval_javascript_unary(self.fold_compile_time_expr(expr)?, *op).ok()
            }
            Expr::JavaScriptBinary { left, op, right } => Some(eval_javascript_binary(
                self.fold_compile_time_expr(left)?,
                *op,
                self.fold_compile_time_expr(right)?,
            )),
            Expr::JavaScriptLogical { left, op, right } => {
                let left = self.fold_compile_time_expr(left)?;
                let use_right = match op {
                    JavaScriptLogicalOp::And => is_truthy(&left).ok()?,
                    JavaScriptLogicalOp::Or => !is_truthy(&left).ok()?,
                    JavaScriptLogicalOp::NullishCoalesce => {
                        matches!(left, Value::Null | Value::Undefined)
                    }
                };
                if use_right {
                    self.fold_compile_time_expr(right)
                } else {
                    Some(left)
                }
            }
            Expr::TypeLiteral(ty) => self.fold_type_expr(ty).map(wrap_type_schema_value),
            Expr::Block(_)
            | Expr::Function(_)
            | Expr::Call { .. }
            | Expr::FunctionCall { .. }
            | Expr::Map { .. }
            | Expr::Try(_)
            | Expr::Throw(_)
            | Expr::Return(_)
            | Expr::Assign { .. }
            | Expr::For { .. }
            | Expr::While { .. }
            | Expr::Break
            | Expr::Continue
            | Expr::ReceiverCall { .. }
            | Expr::Await(_)
            | Expr::SleepFor(_)
            | Expr::SleepUntil(_)
            | Expr::WaitSignal { .. }
            | Expr::ResultUnwrap(_)
            | Expr::Print(_)
            | Expr::Yield(_)
            | Expr::Finish(_)
            | Expr::Fail(_) => None,
        }
    }
}
