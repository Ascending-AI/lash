impl Compiler {
    fn try_compile_label_as_effect_step(
        &mut self,
        expr: &Expr,
        label: &LabelMetadata,
        leave_value: bool,
    ) -> bool {
        let Some(site) = self.labeled_effect_site(expr, label) else {
            return false;
        };
        match expr {
            Expr::Assign { target, expr } => self.compile_assignment_expr_with_forced_effect_site(
                target,
                expr,
                leave_value,
                site,
            ),
            _ => self.compile_expr_with_forced_effect_site(expr, site),
        }
    }

    fn labeled_effect_site(
        &self,
        expr: &Expr,
        label: &LabelMetadata,
    ) -> Option<LashlangExecutionSite> {
        if label_attaches_to_concrete_node(expr) {
            self.concrete_labeled_effect_site(expr)
        } else {
            self.lashlang_execution_site(expr, "step", label.title.as_str())
        }
    }

    fn concrete_labeled_effect_site(&self, expr: &Expr) -> Option<LashlangExecutionSite> {
        match expr {
            Expr::Assign { expr, .. } | Expr::Await(expr) | Expr::ResultUnwrap(expr) => {
                self.concrete_labeled_effect_site(expr)
            }
            Expr::ReceiverCall { operation, .. } => {
                self.lashlang_execution_site(expr, "resource_operation", operation.as_str())
            }
            Expr::StartProcess(start) => self.lashlang_execution_site(
                expr,
                "child_process",
                format!("start {}", start.process),
            ),
            Expr::SleepFor(_) => self.lashlang_execution_site(expr, "sleep", "sleep for"),
            Expr::SleepUntil(_) => self.lashlang_execution_site(expr, "sleep", "sleep until"),
            Expr::WaitSignal { .. } => self.lashlang_execution_site(expr, "wait", "wait_signal"),
            Expr::SignalRun { .. } => self.lashlang_execution_site(expr, "signal", "signal_run"),
            Expr::Finish(_) => self.lashlang_execution_site(expr, "terminal", "result"),
            Expr::Fail(_) => self.lashlang_execution_site(expr, "terminal", "failure"),
            Expr::Yield(_) => self.lashlang_execution_site(expr, "process_event", "yield"),
            Expr::Wake(_) => self.lashlang_execution_site(expr, "process_event", "wake"),
            Expr::If { .. } => self.branch_execution_site(expr),
            _ => None,
        }
    }

    fn compile_assignment_expr_with_forced_effect_site(
        &mut self,
        target: &AssignTarget,
        expr: &Expr,
        leave_value: bool,
        site: LashlangExecutionSite,
    ) -> bool {
        if !expr_supports_forced_effect_site(expr) {
            return false;
        }
        if target.is_simple() {
            let slot = self.push_slot(&target.root);
            if !self.compile_expr_with_forced_effect_site(expr, site) {
                return false;
            }
            self.emit_isolation();
            self.code.push(Instruction::StoreName(slot));
            self.set_const_slot(slot, None);
            self.push_null_if(leave_value);
            return true;
        }

        let slot = self.push_slot(&target.root);
        for step in &target.steps {
            if let AssignPathStep::Index(index) = step {
                self.compile_expr(index);
            }
        }
        if !self.compile_expr_with_forced_effect_site(expr, site) {
            return false;
        }
        self.emit_isolation();
        let path = self.push_assign_path(&target.steps);
        self.code.push(Instruction::PathAssign { slot, path });
        self.set_const_slot(slot, None);
        self.push_null_if(leave_value);
        true
    }

    fn compile_expr_with_forced_effect_site(
        &mut self,
        expr: &Expr,
        site: LashlangExecutionSite,
    ) -> bool {
        self.compile_awaitable_effect_expr(expr, Some(site))
    }

    fn compile_awaitable_effect_expr(
        &mut self,
        expr: &Expr,
        forced_site: Option<LashlangExecutionSite>,
    ) -> bool {
        match expr {
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let instruction = self.compile_receiver_call_expr(receiver, operation, args, false);
                self.mark_awaitable_effect_site(instruction, forced_site, expr, operation.as_str());
                true
            }
            Expr::Await(handle) => self.compile_await_handle_expr(handle, false, forced_site),
            Expr::ResultUnwrap(inner) => {
                if let Expr::Await(handle) = inner.as_ref() {
                    return self.compile_await_handle_expr(handle, true, forced_site);
                }
                if let Expr::ReceiverCall {
                    receiver,
                    operation,
                    args,
                } = inner.as_ref()
                {
                    let instruction =
                        self.compile_receiver_call_expr(receiver, operation, args, true);
                    self.mark_awaitable_effect_site(
                        instruction,
                        forced_site,
                        inner,
                        operation.as_str(),
                    );
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    fn compile_await_handle_expr(
        &mut self,
        handle: &Expr,
        unwrap_result: bool,
        forced_site: Option<LashlangExecutionSite>,
    ) -> bool {
        if self.compile_aggregate_await_expr(handle, unwrap_result, forced_site.clone()) {
            return true;
        }
        match handle {
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let instruction =
                    self.compile_receiver_call_expr(receiver, operation, args, unwrap_result);
                self.mark_awaitable_effect_site(
                    instruction,
                    forced_site,
                    handle,
                    operation.as_str(),
                );
            }
            Expr::ResultUnwrap(inner) => {
                if let Expr::ReceiverCall {
                    receiver,
                    operation,
                    args,
                } = inner.as_ref()
                {
                    let instruction =
                        self.compile_receiver_call_expr(receiver, operation, args, true);
                    self.mark_awaitable_effect_site(
                        instruction,
                        forced_site,
                        inner,
                        operation.as_str(),
                    );
                } else {
                    self.compile_expr(inner);
                    let instruction = self.code.len();
                    self.code.push(Instruction::AwaitHandleUnwrap);
                    self.mark_forced_lashlang_execution_site(instruction, forced_site);
                }
            }
            _ => {
                self.compile_expr(handle);
                let instruction = self.code.len();
                self.code.push(if unwrap_result {
                    Instruction::AwaitHandleUnwrap
                } else {
                    Instruction::AwaitHandle
                });
                self.mark_forced_lashlang_execution_site(instruction, forced_site);
            }
        }
        true
    }

    fn compile_aggregate_await_expr(
        &mut self,
        handle: &Expr,
        aggregate_unwrap: bool,
        forced_site: Option<LashlangExecutionSite>,
    ) -> bool {
        let Some(leaf_count) = aggregate_await_shape_leaf_count(handle) else {
            return false;
        };
        if leaf_count == 0 {
            return false;
        }

        let mut leaves = Vec::with_capacity(leaf_count);
        let mut stack_value_count = 0;
        let shape = self.compile_aggregate_await_shape(handle, &mut leaves, &mut stack_value_count);
        // Only a batch that can propagate a leaf's rejection selects by
        // settlement order. `allSettled` reports every leaf as a record and
        // never unwraps one, so it must not validate — let alone die on —
        // metadata it does not read.
        let selects_by_settlement_order =
            self.dialect == CompilationDialect::Typescript && leaves.iter().any(|leaf| leaf.unwrap);
        let batch = self.push_resource_operation_batch(CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape,
            stack_value_count,
            aggregate_unwrap,
            first_settled_rejection: selects_by_settlement_order,
        });
        let instruction = self.code.len();
        self.code.push(Instruction::ResourceOperationBatch(batch));
        self.mark_instruction_source_span(instruction, handle);
        self.mark_forced_lashlang_execution_site(instruction, forced_site);
        true
    }

    fn compile_aggregate_await_shape(
        &mut self,
        expr: &Expr,
        leaves: &mut Vec<CompiledResourceOperationBatchLeaf>,
        stack_value_count: &mut usize,
    ) -> CompiledAggregateAwaitShape {
        match expr {
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => self.compile_aggregate_await_leaf(
                expr,
                receiver,
                operation,
                args,
                false,
                leaves,
                stack_value_count,
            ),
            Expr::ResultUnwrap(inner) => {
                let Expr::ReceiverCall {
                    receiver,
                    operation,
                    args,
                } = inner.as_ref()
                else {
                    unreachable!("aggregate await shape was pre-validated")
                };
                self.compile_aggregate_await_leaf(
                    expr,
                    receiver,
                    operation,
                    args,
                    true,
                    leaves,
                    stack_value_count,
                )
            }
            Expr::Tuple(items) => {
                let values = items
                    .iter()
                    .map(|item| self.compile_aggregate_await_shape(item, leaves, stack_value_count))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                CompiledAggregateAwaitShape::Tuple(values)
            }
            Expr::List(items) => {
                let values = items
                    .iter()
                    .map(|item| self.compile_aggregate_await_shape(item, leaves, stack_value_count))
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                CompiledAggregateAwaitShape::List(values)
            }
            Expr::Record(entries) => {
                let values = entries
                    .iter()
                    .map(|(_, value)| {
                        self.compile_aggregate_await_shape(value, leaves, stack_value_count)
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                let keys = self.push_key_list(entries.iter().map(|(key, _)| key.as_str()));
                CompiledAggregateAwaitShape::Record { keys, values }
            }
            _ => self.compile_aggregate_await_value(expr, stack_value_count),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "aggregate await leaves mirror receiver-call syntax"
    )]
    fn compile_aggregate_await_leaf(
        &mut self,
        site_expr: &Expr,
        receiver: &Expr,
        operation: &str,
        args: &[Expr],
        unwrap: bool,
        leaves: &mut Vec<CompiledResourceOperationBatchLeaf>,
        stack_value_count: &mut usize,
    ) -> CompiledAggregateAwaitShape {
        let receiver_stack_index = *stack_value_count;
        self.compile_expr(receiver);
        for arg in args {
            self.compile_expr(arg);
        }
        let operation_index = self.push_name(operation);
        let site = self.lashlang_execution_site(site_expr, "resource_operation", operation);
        let source_span = self.expression_source_span(site_expr);
        let leaf_index = leaves.len();
        leaves.push(CompiledResourceOperationBatchLeaf {
            operation: operation_index,
            argc: args.len(),
            receiver_stack_index,
            unwrap,
            site,
            source_span,
        });
        *stack_value_count += args.len() + 1;
        CompiledAggregateAwaitShape::BatchLeaf(leaf_index)
    }

    fn compile_aggregate_await_value(
        &mut self,
        expr: &Expr,
        stack_value_count: &mut usize,
    ) -> CompiledAggregateAwaitShape {
        let value_index = *stack_value_count;
        self.compile_expr(expr);
        *stack_value_count += 1;
        CompiledAggregateAwaitShape::Value(value_index)
    }

    fn mark_awaitable_effect_site(
        &mut self,
        instruction: usize,
        forced_site: Option<LashlangExecutionSite>,
        site_expr: &Expr,
        operation: &str,
    ) {
        let site = forced_site
            .or_else(|| self.lashlang_execution_site(site_expr, "resource_operation", operation));
        self.mark_instruction_source_span(instruction, site_expr);
        self.mark_forced_lashlang_execution_site(instruction, site);
    }

    fn mark_forced_lashlang_execution_site(
        &mut self,
        instruction: usize,
        site: Option<LashlangExecutionSite>,
    ) {
        if let Some(site) = site {
            self.mark_lashlang_execution_site(instruction, site);
        }
    }

    fn compile_start_process_expr(&mut self, process: &ProcessStartExpr) -> usize {
        for (_, expr) in &process.args {
            self.compile_expr(expr);
        }
        let keys = self.push_key_list(process.args.iter().map(|(name, _)| name.as_str()));
        let process = self.push_name(&process.process);
        let instruction = self.code.len();
        self.code.push(Instruction::StartProcess { process, keys });
        instruction
    }

    fn compile_process_ref_expr(&mut self, process: &str) {
        let Some(module_context) = self.module_context.as_ref() else {
            self.emit_push_value(Value::Null);
            return;
        };
        let Some(process_ref) = module_context.process_refs.get(process) else {
            self.emit_push_value(Value::Null);
            return;
        };
        let mut record = record_with_capacity(5);
        record.insert(LASH_PROCESS_VALUE_KEY.to_string(), Value::Bool(true));
        record.insert(
            LASH_PROCESS_NAME_KEY.to_string(),
            Value::String(process.into()),
        );
        record.insert(
            LASH_MODULE_REF_KEY.to_string(),
            Value::String(module_context.module_ref.to_string().into()),
        );
        let mut process_ref_record = record_with_capacity(2);
        process_ref_record.insert(
            "component".to_string(),
            Value::String(process_ref.component.to_string().into()),
        );
        process_ref_record.insert("pos".to_string(), Value::Number(process_ref.pos as f64));
        record.insert(
            LASH_PROCESS_REF_KEY.to_string(),
            Value::Record(Arc::new(process_ref_record)),
        );
        record.insert(
            LASH_HOST_REQUIREMENTS_REF_KEY.to_string(),
            Value::String(module_context.host_requirements_ref.to_string().into()),
        );
        self.emit_push_value(Value::Record(Arc::new(record)));
    }

    fn compile_receiver_call_expr(
        &mut self,
        receiver: &Expr,
        operation: &str,
        args: &[Expr],
        unwrap: bool,
    ) -> usize {
        self.compile_expr(receiver);
        for arg in args {
            self.compile_expr(arg);
        }
        let operation = self.push_name(operation);
        let instruction = self.code.len();
        if unwrap {
            self.code.push(Instruction::ResourceCallUnwrap {
                operation,
                argc: args.len(),
            });
        } else {
            self.code.push(Instruction::ResourceCall {
                operation,
                argc: args.len(),
            });
        }
        instruction
    }

    fn emit_jump_if_false(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::JumpIfFalse(usize::MAX));
        index
    }

    fn compile_condition_jump_if_false(&mut self, condition: &Expr) -> usize {
        if !contains_type_literal(condition)
            && let Some(value) = self.fold_compile_time_expr(condition)
        {
            self.emit_push_value(value);
            return self.emit_jump_if_false();
        }

        if let Expr::Binary { left, op, right } = condition
            && is_comparison_binary_op(*op)
        {
            if let (
                Expr::Binary {
                    left: inner_left,
                    op: binary_op,
                    right: inner_right,
                },
                Some(Value::Number(compare_right)),
            ) = (left.as_ref(), self.fold_compile_time_expr(right))
                && is_numeric_binary_op(*binary_op)
                && let (Expr::Variable(name), Some(Value::Number(binary_right))) = (
                    inner_left.as_ref(),
                    self.fold_compile_time_expr(inner_right),
                )
            {
                let slot = self.push_slot(name);
                let index = self.code.len();
                self.code
                    .push(Instruction::JumpIfSlotNumberBinaryCompareFalse {
                        slot,
                        binary_op: *binary_op,
                        binary_right,
                        compare_op: *op,
                        compare_right,
                        target: usize::MAX,
                    });
                return index;
            }

            if let (Expr::Variable(name), Some(Value::Number(right))) =
                (left.as_ref(), self.fold_compile_time_expr(right))
            {
                let slot = self.push_slot(name);
                let index = self.code.len();
                self.code.push(Instruction::JumpIfSlotNumberCompareFalse {
                    slot,
                    op: *op,
                    right,
                    target: usize::MAX,
                });
                return index;
            }
            self.compile_expr(left);
            self.compile_expr(right);
            let index = self.code.len();
            self.code.push(Instruction::JumpIfCompareFalse {
                op: *op,
                target: usize::MAX,
            });
            return index;
        }

        self.compile_expr(condition);
        self.emit_jump_if_false()
    }

    fn emit_jump_if_true(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::JumpIfTrue(usize::MAX));
        index
    }

    fn emit_jump(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::Jump(usize::MAX));
        index
    }

    fn compile_type_literal(&mut self, ty: &TypeExpr) {
        self.compile_stats.borrow_mut().type_literals_total += 1;

        if let Some(schema) = self.fold_type_expr(ty) {
            let idx = self.push_const(wrap_type_schema_value(schema));
            self.code.push(Instruction::PushConst(idx));
            self.compile_stats.borrow_mut().type_literals_const_folded += 1;
            return;
        }

        self.compile_type_expr(ty);
        self.code.push(Instruction::WrapTypeLiteral);
        self.compile_stats.borrow_mut().type_literals_dynamic += 1;
    }

    fn compile_type_expr(&mut self, ty: &TypeExpr) {
        if let Some(value) = self.fold_type_expr(ty) {
            let idx = self.push_const(value);
            self.code.push(Instruction::PushConst(idx));
            return;
        }

        match ty {
            TypeExpr::Ref(name) => {
                let slot = self.push_slot(name);
                self.code.push(Instruction::ResolveTypeRef(slot));
                self.compile_stats.borrow_mut().type_ref_sites += 1;
            }
            TypeExpr::List(inner) => {
                let kind_idx = self.push_const(Value::String(schema_keys::ARRAY.into()));
                self.code.push(Instruction::PushConst(kind_idx));
                self.compile_type_expr(inner);
                let keys = self.push_key_list([schema_keys::TYPE, schema_keys::ITEMS].into_iter());
                self.code.push(Instruction::BuildRecord(keys));
            }
            TypeExpr::Object(fields) => {
                let kind_idx = self.push_const(Value::String(schema_keys::OBJECT.into()));
                self.code.push(Instruction::PushConst(kind_idx));

                for field in fields {
                    self.compile_type_expr(&field.ty);
                }
                let prop_keys = self.push_key_list(fields.iter().map(|f| f.name.as_str()));
                self.code.push(Instruction::BuildRecord(prop_keys));

                let required: Vec<&str> = fields
                    .iter()
                    .filter(|f| !f.optional)
                    .map(|f| f.name.as_str())
                    .collect();
                for name in &required {
                    let idx = self.push_const(Value::String((*name).into()));
                    self.code.push(Instruction::PushConst(idx));
                }
                self.code.push(Instruction::BuildList(required.len()));

                self.code.push(Instruction::PushBool(false));

                let obj_keys = self.push_key_list(
                    [
                        schema_keys::TYPE,
                        schema_keys::PROPERTIES,
                        schema_keys::REQUIRED,
                        schema_keys::ADDITIONAL_PROPERTIES,
                    ]
                    .into_iter(),
                );
                self.code.push(Instruction::BuildRecord(obj_keys));
            }
            TypeExpr::Union(variants) => {
                // Union reaches this arm only when at least one variant
                // contains a `Ref` that couldn't const-fold. Compile
                // each variant and pack them into an `anyOf` list.
                for variant in variants {
                    self.compile_type_expr(variant);
                }
                self.code.push(Instruction::BuildList(variants.len()));
                let keys = self.push_key_list([schema_keys::ANY_OF].into_iter());
                self.code.push(Instruction::BuildRecord(keys));
            }
            TypeExpr::Process { .. } | TypeExpr::TriggerHandle(_) => {
                let idx = self.push_const(interned_scalar_schema(ScalarSchemaKind::Any));
                self.code.push(Instruction::PushConst(idx));
            }
            TypeExpr::Any
            | TypeExpr::Str
            | TypeExpr::Int
            | TypeExpr::Float
            | TypeExpr::Bool
            | TypeExpr::Dict
            | TypeExpr::Null
            | TypeExpr::Enum(_) => {
                unreachable!("scalar/enum types must const-fold")
            }
        }
    }

    fn fold_type_expr(&self, ty: &TypeExpr) -> Option<Value> {
        self.fold_type_expr_inner(ty, &mut SmallVec::new())
    }

    fn fold_type_expr_inner<'a>(
        &self,
        ty: &'a TypeExpr,
        resolving: &mut SmallVec<[&'a str; 4]>,
    ) -> Option<Value> {
        use schema_keys::*;
        match ty {
            TypeExpr::Ref(name) => {
                let name = name.as_str();
                if resolving.contains(&name) {
                    return None;
                }
                let wrapper = self.const_for_name(name)?;
                resolving.push(name);
                let schema = unwrap_type_value(&wrapper).cloned();
                resolving.pop();
                schema
            }
            TypeExpr::List(inner) => {
                let inner_value = self.fold_type_expr_inner(inner, resolving)?;
                let mut rec = record_with_capacity(2);
                rec.insert(TYPE.into(), Value::String(ARRAY.into()));
                rec.insert(ITEMS.into(), inner_value);
                Some(Value::Record(Arc::new(rec)))
            }
            TypeExpr::Object(fields) => {
                let mut properties = record_with_capacity(fields.len());
                for field in fields {
                    properties.insert(
                        field.name.to_string(),
                        self.fold_type_expr_inner(&field.ty, resolving)?,
                    );
                }
                let required: Vec<Value> = fields
                    .iter()
                    .filter(|f| !f.optional)
                    .map(|f| Value::String(f.name.clone()))
                    .collect();
                let mut rec = record_with_capacity(4);
                rec.insert(TYPE.into(), Value::String(OBJECT.into()));
                rec.insert(PROPERTIES.into(), Value::Record(Arc::new(properties)));
                rec.insert(REQUIRED.into(), Value::List(required.into()));
                rec.insert(ADDITIONAL_PROPERTIES.into(), Value::Bool(false));
                Some(Value::Record(Arc::new(rec)))
            }
            TypeExpr::Union(variants) => {
                let folded: Option<Vec<Value>> = variants
                    .iter()
                    .map(|variant| self.fold_type_expr_inner(variant, resolving))
                    .collect();
                let folded = folded?;
                let mut rec = record_with_capacity(1);
                rec.insert(ANY_OF.into(), Value::List(folded.into()));
                Some(Value::Record(Arc::new(rec)))
            }
            _ => fold_type(ty),
        }
    }

    fn patch_jump(&mut self, index: usize, target: usize) {
        match &mut self.code[index] {
            Instruction::Jump(slot)
            | Instruction::JumpIfFalse(slot)
            | Instruction::JumpIfCompareFalse { target: slot, .. }
            | Instruction::JumpIfSlotNumberCompareFalse { target: slot, .. }
            | Instruction::JumpIfSlotNumberBinaryCompareFalse { target: slot, .. }
            | Instruction::JumpIfTrue(slot)
            | Instruction::IterNext { jump_to: slot } => *slot = target,
            _ => unreachable!("patched non-jump instruction"),
        }
    }
}

fn aggregate_await_shape_leaf_count(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::Tuple(items) => items.iter().try_fold(0usize, |count, item| {
            Some(count + aggregate_await_leaf_count(item)?)
        }),
        Expr::List(items) => items.iter().try_fold(0usize, |count, item| {
            Some(count + aggregate_await_leaf_count(item)?)
        }),
        Expr::Record(entries) => entries.iter().try_fold(0usize, |count, (_, value)| {
            Some(count + aggregate_await_leaf_count(value)?)
        }),
        _ => None,
    }
}

fn aggregate_await_leaf_count(expr: &Expr) -> Option<usize> {
    match expr {
        Expr::ReceiverCall { .. } => Some(1),
        Expr::ResultUnwrap(inner) if matches!(inner.as_ref(), Expr::ReceiverCall { .. }) => Some(1),
        Expr::Tuple(items) => items.iter().try_fold(0usize, |count, item| {
            Some(count + aggregate_await_leaf_count(item)?)
        }),
        Expr::List(items) => items.iter().try_fold(0usize, |count, item| {
            Some(count + aggregate_await_leaf_count(item)?)
        }),
        Expr::Record(entries) => entries.iter().try_fold(0usize, |count, (_, value)| {
            Some(count + aggregate_await_leaf_count(value)?)
        }),
        expr if is_pure_expr(expr) => Some(0),
        _ => None,
    }
}
