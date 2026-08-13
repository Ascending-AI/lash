impl Compiler {
    pub(crate) fn compile_program(program: &Program) -> (Chunk, CompileStats) {
        Self::compile_program_with_dialect(program, CompilationDialect::Lashlang)
    }

    pub(crate) fn compile_program_with_dialect(
        program: &Program,
        dialect: CompilationDialect,
    ) -> (Chunk, CompileStats) {
        let stats = Rc::new(RefCell::new(CompileStats::default()));
        let mut compiler = Self::with_slots_and_stats(
            None,
            Rc::new(RefCell::new(SlotTable::default())),
            stats.clone(),
            dialect,
        );
        compiler.expression_source_spans = expression_source_spans(program);
        compiler.compile_program_block(program);
        let chunk = compiler.finish();
        let compile_stats = *stats.borrow();
        (chunk, compile_stats)
    }

    pub(crate) fn compile_linked_program_with_dialect(
        program: &Program,
        module_context: CompiledModuleContext,
        lashlang_execution_context: LashlangExecutionContext,
        dialect: CompilationDialect,
    ) -> (Chunk, CompileStats) {
        let stats = Rc::new(RefCell::new(CompileStats::default()));
        let mut compiler = Self::with_slots_and_stats(
            Some(module_context),
            Rc::new(RefCell::new(SlotTable::default())),
            stats.clone(),
            dialect,
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
            CompilationDialect::Lashlang,
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
        dialect: CompilationDialect,
    ) -> Self {
        Self {
            dialect,
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
            compile_stats,
            const_slots: Vec::new(),
            loop_contexts: Vec::new(),
            handler_scopes: Vec::new(),
            handler_scope_extents: Vec::new(),
            pending_finally_sites: Vec::new(),
            functions: Vec::new(),
            pending_functions: Vec::new(),
        }
    }

    fn emit_isolation(&mut self) {
        if self.dialect == CompilationDialect::Lashlang {
            self.code.push(Instruction::DeepCopy);
        }
    }

    fn emit_loop_binding_isolation(&mut self, binding: usize) {
        if self.dialect == CompilationDialect::Lashlang {
            self.code.push(Instruction::DeepCopyLoopBinding(binding));
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
            module_context: self.module_context,
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
            functions: self.functions,
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

    fn compile_pending_functions(&mut self) {
        let mut next = 0;
        while next < self.pending_functions.len() {
            let definition = self.pending_functions[next]
                .take()
                .expect("pending function is compiled once");
            debug_assert_eq!(next, self.functions.len());

            let root_slots =
                std::mem::replace(&mut self.slots, Rc::new(RefCell::new(SlotTable::default())));
            let root_const_slots = std::mem::take(&mut self.const_slots);
            let root_loops = std::mem::take(&mut self.loop_contexts);
            let root_handler_scopes = std::mem::take(&mut self.handler_scopes);

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
            self.compile_expr(&definition.body);
            self.code.push(Instruction::Return);
            let end_ip = self.code.len();
            let slot_names = self.slots.borrow().names.clone().into_boxed_slice();
            self.functions.push(CompiledFunction {
                entry_ip,
                end_ip,
                parameter_count: definition.params.len(),
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

    fn copy_expression_metadata(&mut self, original: &Expr, cloned: &Expr) {
        let original_key = original as *const Expr as usize;
        let cloned_key = cloned as *const Expr as usize;
        if let Some(span) = self.expression_source_spans.get(&original_key).copied() {
            self.expression_source_spans.insert(cloned_key, span);
        }
        if let Some(tracking) = &mut self.lashlang_execution
            && let Some(path) = tracking.paths.get(&original_key).cloned()
        {
            tracking.paths.insert(cloned_key, path);
        }
        for (original_child, cloned_child) in original.children().zip(cloned.children()) {
            self.copy_expression_metadata(original_child, cloned_child);
        }
    }

    fn push_const(&mut self, value: Value) -> usize {
        let index = self.constants.len();
        self.constants.push(value);
        index
    }

    fn emit_push_value(&mut self, value: Value) {
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

    fn push_name(&mut self, name: &str) -> usize {
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

    fn push_slot(&mut self, name: &str) -> usize {
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

    fn push_key_list<'a>(&mut self, keys: impl Iterator<Item = &'a str>) -> usize {
        let index = self.key_lists.len();
        let keys = keys
            .map(|key| self.push_name(key))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        self.key_lists.push(keys);
        index
    }

    fn push_assign_path(&mut self, steps: &[AssignPathStep]) -> usize {
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

    fn push_resource_operation_batch(&mut self, batch: CompiledResourceOperationBatch) -> usize {
        let index = self.resource_operation_batches.len();
        self.resource_operation_batches.push(batch);
        index
    }

    fn push_format_template(&mut self, template: &str, argc: usize) -> usize {
        let index = self.format_templates.len();
        self.format_templates
            .push(compile_format_template(template, argc));
        index
    }

    fn push_compiled_schema(&mut self, schema: &Value) -> usize {
        let index = self.compiled_schemas.len();
        self.compiled_schemas.push(compile_schema_value(schema));
        index
    }

    fn ensure_const_slot(&mut self, slot: usize) {
        if self.const_slots.len() <= slot {
            self.const_slots.resize(slot + 1, None);
        }
    }

    fn set_const_slot(&mut self, slot: usize, value: Option<Value>) {
        self.ensure_const_slot(slot);
        self.const_slots[slot] = value;
    }

    fn clear_const_slots(&mut self) {
        self.const_slots.fill(None);
    }

    fn const_for_slot(&self, slot: usize) -> Option<Value> {
        self.const_slots.get(slot).cloned().flatten()
    }

    fn const_for_name(&self, name: &str) -> Option<Value> {
        let symbol = lookup_symbol(name)?;
        let slots = self.slots.borrow();
        let slot = *slots.lookup.get(&symbol)?;
        drop(slots);
        self.const_for_slot(slot)
    }

    fn resolve_intrinsic(&mut self, name: &str, argc: usize) -> IntrinsicOp {
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

    fn compile_program_block(&mut self, program: &Program) {
        match &program.main {
            Expr::Block(expressions) => {
                self.compile_block_value_with_spans(expressions, &program.expression_spans);
            }
            expression => self.compile_expr(expression),
        }
        if !is_terminal_expr(&program.main) {
            let pop = self.code.len();
            self.code.push(Instruction::Pop);
            if let Some(span) = program.expression_spans.last().copied() {
                self.mark_instruction_spans(pop, self.code.len(), span);
            }
        }
    }

    fn compile_block_value(&mut self, expressions: &[Expr]) {
        let Some((last, prefix)) = expressions.split_last() else {
            self.code.push(Instruction::PushNull);
            return;
        };
        for expression in prefix {
            self.compile_expr_discarding_value(expression);
        }
        self.compile_expr(last);
    }

    fn compile_block_value_with_spans(&mut self, expressions: &[Expr], spans: &[Span]) {
        let Some((last, prefix)) = expressions.split_last() else {
            self.code.push(Instruction::PushNull);
            return;
        };
        for (index, expression) in prefix.iter().enumerate() {
            let span = spans.get(index).copied();
            self.compile_expr_discarding_value_with_span(expression, span);
        }
        self.compile_expr_with_span(last, spans.get(expressions.len() - 1).copied());
    }

    fn compile_expr_with_span(&mut self, expression: &Expr, span: Option<Span>) {
        let start = self.code.len();
        self.compile_expr(expression);
        if let Some(span) = span {
            self.mark_instruction_spans(start, self.code.len(), span);
        }
    }

    fn compile_expr_discarding_value_with_span(&mut self, expression: &Expr, span: Option<Span>) {
        let start = self.code.len();
        self.compile_expr_discarding_value(expression);
        if let Some(span) = span {
            self.mark_instruction_spans(start, self.code.len(), span);
        }
    }

    fn compile_expr_discarding_value(&mut self, expression: &Expr) {
        match expression {
            Expr::LabelAnnotated { label, expr } => {
                if self.try_compile_label_as_effect_step(expr, label, false) {
                    return;
                }
                if !label_attaches_to_concrete_node(expr) {
                    self.emit_lashlang_execution_step(expression, label);
                }
                self.compile_expr_discarding_value(expr);
            }
            Expr::Block(expressions) => {
                for expression in expressions {
                    self.compile_expr_discarding_value(expression);
                }
            }
            Expr::Assign { target, expr } => self.compile_assignment_expr(target, expr, false),
            Expr::For {
                binding,
                iterable,
                body,
            } => self.compile_for_expr(binding, iterable, body, false),
            Expr::While { condition, body } => self.compile_while_expr(condition, body, false),
            Expr::Finish(_) | Expr::Fail(_) | Expr::Return(_) | Expr::Break | Expr::Continue => {
                self.compile_expr(expression);
            }
            expression => {
                self.compile_expr(expression);
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

    fn mark_instruction_source_span(&mut self, instruction: usize, expr: &Expr) {
        let Some(span) = self.expression_source_span(expr) else {
            return;
        };
        if self.spans.len() <= instruction {
            self.spans.resize(instruction + 1, None);
        }
        self.spans[instruction] = Some(span);
    }

    fn expression_source_span(&self, expr: &Expr) -> Option<Span> {
        self.expression_source_spans.get(&expr_key(expr)).copied()
    }

    fn mark_lashlang_execution_site(&mut self, instruction: usize, site: LashlangExecutionSite) {
        let Some(tracking) = self.lashlang_execution.as_mut() else {
            return;
        };
        if tracking.sites.len() <= instruction {
            tracking.sites.resize(instruction + 1, None);
        }
        tracking.sites[instruction] = Some(site);
    }

    fn lashlang_execution_site(
        &self,
        expression: &Expr,
        kind: &str,
        label: impl Into<String>,
    ) -> Option<LashlangExecutionSite> {
        let tracking = self.lashlang_execution.as_ref()?;
        let path = tracking.paths.get(&expr_key(expression))?;
        Some(tracking.context.builder().node_site(path, kind, label))
    }

    fn emit_lashlang_execution_step(&mut self, expression: &Expr, label: &LabelMetadata) {
        let instruction = self.code.len();
        self.code.push(Instruction::ObserveStep);
        if let Some(site) = self.lashlang_execution_site(expression, "step", label.title.as_str()) {
            self.mark_lashlang_execution_site(instruction, site);
        }
    }

    fn branch_execution_site(&self, expression: &Expr) -> Option<LashlangExecutionSite> {
        let tracking = self.lashlang_execution.as_ref()?;
        let path = tracking.paths.get(&expr_key(expression))?;
        Some(tracking.context.builder().branch_site(path))
    }

    fn compile_block_discarding_values(&mut self, block: &Expr) {
        match block {
            Expr::Block(expressions) => {
                for expression in expressions {
                    self.compile_expr_discarding_value(expression);
                }
            }
            expression => {
                self.compile_expr_discarding_value(expression);
            }
        }
    }

    fn push_null_if(&mut self, leave_value: bool) {
        if leave_value {
            self.code.push(Instruction::PushNull);
        }
    }

    fn compile_assignment_expr(&mut self, target: &AssignTarget, expr: &Expr, leave_value: bool) {
        if target.is_simple() {
            let name = &target.root;
            let slot = self.push_slot(name);
            let has_type_literal = contains_type_literal(expr);
            let const_value = if self.dialect == CompilationDialect::Typescript {
                None
            } else if let Expr::TypeLiteral(ty) = expr {
                self.fold_type_expr(ty).map(wrap_type_schema_value)
            } else if has_type_literal {
                None
            } else {
                self.fold_compile_time_expr(expr)
            };
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
                    self.compile_expr(&items[0]);
                    self.emit_isolation();
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
                self.compile_expr(right);
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
                self.compile_expr(item);
                self.emit_isolation();
                self.code
                    .push(Instruction::Intrinsic(IntrinsicOp::PushAssign(slot)));
                self.set_const_slot(slot, None);
                self.push_null_if(leave_value);
                return;
            }
            if let Some(value) = const_value.clone()
                && !has_type_literal
            {
                let constant = self.push_const(value);
                self.code.push(Instruction::StoreConst { slot, constant });
                self.set_const_slot(slot, const_value);
                self.push_null_if(leave_value);
                return;
            }

            self.compile_expr(expr);
            if store_needs_isolation(expr) {
                self.emit_isolation();
            }
            self.code.push(Instruction::StoreName(slot));
            self.set_const_slot(slot, const_value);
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
                self.compile_expr(index);
                self.code
                    .push(Instruction::AddAssignIndexNumber { slot, right });
            }
            self.set_const_slot(slot, None);
            self.push_null_if(leave_value);
            return;
        }
        for step in &target.steps {
            if let AssignPathStep::Index(index) = step {
                self.compile_expr(index);
            }
        }
        self.compile_expr(expr);
        self.emit_isolation();
        let path = self.push_assign_path(&target.steps);
        self.code.push(match self.dialect {
            CompilationDialect::Lashlang => Instruction::PathAssign { slot, path },
            CompilationDialect::Typescript => Instruction::HeapPathAssign { slot, path },
        });
        self.set_const_slot(slot, None);
        self.push_null_if(leave_value);
    }

    fn compile_for_expr(&mut self, binding: &str, iterable: &Expr, body: &Expr, leave_value: bool) {
        let binding = self.push_slot(binding);
        if let Expr::BuiltinCall { name, args } = iterable
            && name.as_str() == "range"
        {
            for arg in args {
                self.compile_expr(arg);
            }
            self.clear_const_slots();
            self.set_const_slot(binding, None);
            self.code.push(Instruction::BeginRangeIter {
                binding,
                argc: args.len(),
            });
            self.compile_for_loop_body(body, binding);
            self.push_null_if(leave_value);
            return;
        }

        self.compile_expr(iterable);
        self.clear_const_slots();
        self.set_const_slot(binding, None);
        self.code.push(Instruction::BeginIter(binding));
        self.compile_for_loop_body(body, binding);
        self.push_null_if(leave_value);
    }

    fn compile_for_loop_body(&mut self, body: &Expr, binding: usize) {
        let loop_start = self.code.len();
        let iter_next = self.code.len();
        self.code.push(Instruction::IterNext {
            jump_to: usize::MAX,
        });
        self.emit_loop_binding_isolation(binding);
        self.loop_contexts.push(LoopContext {
            continue_target: loop_start,
            break_jumps: SmallVec::new(),
            handler_scope_depth: self.handler_scopes.len(),
        });
        self.compile_block_discarding_values(body);
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

    fn compile_list_comprehension(&mut self, element: &Expr, clauses: &[ListComprehensionClause]) {
        self.code.push(Instruction::BuildList(0));
        self.compile_list_comprehension_clause(element, clauses, 0);
        self.clear_const_slots();
    }

    fn compile_list_comprehension_clause(
        &mut self,
        element: &Expr,
        clauses: &[ListComprehensionClause],
        index: usize,
    ) {
        let Some(clause) = clauses.get(index) else {
            self.compile_expr(element);
            self.emit_isolation();
            self.code.push(Instruction::ListAppend);
            return;
        };

        match clause {
            ListComprehensionClause::For { binding, iterable } => {
                self.compile_list_comprehension_for(binding, iterable, element, clauses, index + 1);
            }
            ListComprehensionClause::If { condition } => {
                let jump_to_next_iteration = self.compile_condition_jump_if_false(condition);
                self.clear_const_slots();
                self.compile_list_comprehension_clause(element, clauses, index + 1);
                self.patch_jump(jump_to_next_iteration, self.code.len());
                self.clear_const_slots();
            }
        }
    }

    fn compile_list_comprehension_for(
        &mut self,
        binding: &str,
        iterable: &Expr,
        element: &Expr,
        clauses: &[ListComprehensionClause],
        next_clause: usize,
    ) {
        let binding = self.push_slot(binding);
        if let Expr::BuiltinCall { name, args } = iterable
            && name.as_str() == "range"
        {
            for arg in args {
                self.compile_expr(arg);
            }
            self.clear_const_slots();
            self.set_const_slot(binding, None);
            self.code.push(Instruction::BeginRangeIter {
                binding,
                argc: args.len(),
            });
            self.compile_list_comprehension_for_body(element, clauses, next_clause, binding);
            return;
        }

        self.compile_expr(iterable);
        self.clear_const_slots();
        self.set_const_slot(binding, None);
        self.code.push(Instruction::BeginIter(binding));
        self.compile_list_comprehension_for_body(element, clauses, next_clause, binding);
    }

    fn compile_list_comprehension_for_body(
        &mut self,
        element: &Expr,
        clauses: &[ListComprehensionClause],
        next_clause: usize,
        binding: usize,
    ) {
        let loop_start = self.code.len();
        let iter_next = self.code.len();
        self.code.push(Instruction::IterNext {
            jump_to: usize::MAX,
        });
        self.emit_loop_binding_isolation(binding);
        self.compile_list_comprehension_clause(element, clauses, next_clause);
        self.code.push(Instruction::Jump(loop_start));
        let loop_end = self.code.len();
        self.code.push(Instruction::EndIter);
        self.patch_jump(iter_next, loop_end);
        self.clear_const_slots();
    }

    fn compile_while_expr(&mut self, condition: &Expr, body: &Expr, leave_value: bool) {
        self.clear_const_slots();
        let loop_start = self.code.len();
        let jump_to_end = self.compile_condition_jump_if_false(condition);
        self.clear_const_slots();
        self.loop_contexts.push(LoopContext {
            continue_target: loop_start,
            break_jumps: SmallVec::new(),
            handler_scope_depth: self.handler_scopes.len(),
        });
        self.compile_block_discarding_values(body);
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

    fn fold_compile_time_expr(&self, expr: &Expr) -> Option<Value> {
        match expr {
            Expr::LabelAnnotated { expr, .. } => self.fold_compile_time_expr(expr),
            Expr::Null => Some(Value::Null),
            Expr::Undefined => Some(Value::Undefined),
            Expr::Bool(value) => Some(Value::Bool(*value)),
            Expr::Number(value) => Some(Value::Number(*value)),
            Expr::String(value) => Some(Value::String(value.clone())),
            Expr::ResourceRef(resource) => Some(Value::Resource(super::ResourceHandle::new(
                resource.resource_type.to_string(),
                resource.alias.to_string(),
            ))),
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
                read_field_direct(target, &transient_name(field)).ok()
            }
            Expr::Index { target, index } => {
                let target = self.fold_compile_time_expr(target)?;
                let index = self.fold_compile_time_expr(index)?;
                read_index_direct(target, index).ok()
            }
            Expr::Unary { op, expr } => {
                let value = self.fold_compile_time_expr(expr)?;
                match op {
                    UnaryOp::Negate => Some(Value::Number(-as_number(&value).ok()?)),
                    UnaryOp::Not => Some(Value::Bool(!is_truthy(&value))),
                }
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                if is_truthy(&self.fold_compile_time_expr(condition)?) {
                    self.fold_compile_time_expr(then_block)
                } else {
                    self.fold_compile_time_expr(else_block)
                }
            }
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    let left = self.fold_compile_time_expr(left)?;
                    if !is_truthy(&left) {
                        Some(Value::Bool(false))
                    } else {
                        Some(Value::Bool(is_truthy(&self.fold_compile_time_expr(right)?)))
                    }
                }
                BinaryOp::Or => {
                    let left = self.fold_compile_time_expr(left)?;
                    if is_truthy(&left) {
                        Some(Value::Bool(true))
                    } else {
                        Some(Value::Bool(is_truthy(&self.fold_compile_time_expr(right)?)))
                    }
                }
                _ => {
                    let left = self.fold_compile_time_expr(left)?;
                    let right = self.fold_compile_time_expr(right)?;
                    eval_binary_values(left, *op, right).ok()
                }
            },
            Expr::JavaScriptUnary { op, expr } => Some(eval_javascript_unary(
                self.fold_compile_time_expr(expr)?,
                *op,
            )),
            Expr::JavaScriptBinary { left, op, right } => Some(eval_javascript_binary(
                self.fold_compile_time_expr(left)?,
                *op,
                self.fold_compile_time_expr(right)?,
            )),
            Expr::JavaScriptLogical { left, op, right } => {
                let left = self.fold_compile_time_expr(left)?;
                let use_right = match op {
                    JavaScriptLogicalOp::And => is_truthy(&left),
                    JavaScriptLogicalOp::Or => !is_truthy(&left),
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
            | Expr::StartProcess(_)
            | Expr::Await(_)
            | Expr::SleepFor(_)
            | Expr::SleepUntil(_)
            | Expr::WaitSignal { .. }
            | Expr::SignalRun { .. }
            | Expr::ResultUnwrap(_)
            | Expr::Cancel(_)
            | Expr::Print(_)
            | Expr::Yield(_)
            | Expr::Wake(_)
            | Expr::Finish(_)
            | Expr::Fail(_) => None,
        }
    }
}
