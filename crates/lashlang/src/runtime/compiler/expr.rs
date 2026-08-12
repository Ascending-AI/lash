impl Compiler {
    fn emit_builtin_call(&mut self, name: &str, args: &[Expr]) {
        if name == "format"
            && let Some((Expr::String(template), value_args)) = args.split_first()
        {
            if let [Expr::Variable(slot_name)] = value_args {
                let template = self.push_format_template(template, value_args.len());
                let slot = self.push_slot(slot_name);
                self.code.push(Instruction::Intrinsic(
                    IntrinsicOp::FormatCompiledSlotNumber { template, slot },
                ));
                return;
            }
            if let [Expr::Binary { left, op, right }] = value_args
                && is_numeric_binary_op(*op)
                && let (Expr::Variable(slot_name), Some(Value::Number(right))) =
                    (left.as_ref(), self.fold_compile_time_expr(right))
            {
                let template = self.push_format_template(template, value_args.len());
                let slot = self.push_slot(slot_name);
                self.code.push(Instruction::Intrinsic(
                    IntrinsicOp::FormatCompiledSlotNumberBinary {
                        template,
                        slot,
                        op: *op,
                        right,
                    },
                ));
                return;
            }
            for arg in value_args {
                self.compile_expr(arg);
            }
            let template = self.push_format_template(template, value_args.len());
            self.code
                .push(Instruction::Intrinsic(IntrinsicOp::FormatCompiled(
                    template,
                )));
            return;
        }

        match (name, args.len()) {
            ("len", 1) => {
                self.compile_expr(&args[0]);
                self.code.push(Instruction::Intrinsic(IntrinsicOp::Len));
            }
            ("join", 2) => {
                self.compile_expr(&args[0]);
                self.compile_expr(&args[1]);
                self.code.push(Instruction::Intrinsic(IntrinsicOp::Join));
            }
            ("validate", 2) => {
                if let Some(schema_wrapper) = self.fold_compile_time_expr(&args[1])
                    && let Some(schema) = unwrap_type_value(&schema_wrapper).cloned()
                {
                    self.compile_expr(&args[0]);
                    let schema = self.push_compiled_schema(&schema);
                    self.code
                        .push(Instruction::Intrinsic(IntrinsicOp::ValidateCompiled(
                            schema,
                        )));
                    return;
                }

                self.compile_expr(&args[0]);
                self.compile_expr(&args[1]);
                self.code
                    .push(Instruction::Intrinsic(IntrinsicOp::Validate));
            }
            ("push", 2) => {
                self.compile_expr(&args[0]);
                self.compile_expr(&args[1]);
                self.code.push(Instruction::DeepCopy);
                self.code.push(Instruction::Intrinsic(IntrinsicOp::Push));
            }
            ("range", 1..=3) => {
                for arg in args {
                    self.compile_expr(arg);
                }
                self.code
                    .push(Instruction::Intrinsic(IntrinsicOp::Range(args.len())));
            }
            _ => {
                for arg in args {
                    self.compile_expr(arg);
                }
                let builtin = self.resolve_intrinsic(name, args.len());
                self.code.push(Instruction::Intrinsic(builtin));
            }
        }
    }

    fn compile_expr(&mut self, expr: &Expr) {
        if !contains_type_literal(expr)
            && let Some(value) = self.fold_compile_time_expr(expr)
        {
            self.emit_push_value(value);
            return;
        }

        match expr {
            Expr::LabelAnnotated { label, expr } => {
                if self.try_compile_label_as_effect_step(expr, label, true) {
                    return;
                }
                if !label_attaches_to_concrete_node(expr) {
                    self.emit_lashlang_execution_step(expr, label);
                }
                self.compile_expr(expr);
            }
            Expr::Block(expressions) => self.compile_block_value(expressions),
            Expr::Assign { target, expr } => self.compile_assignment_expr(target, expr, true),
            Expr::For {
                binding,
                iterable,
                body,
            } => self.compile_for_expr(binding, iterable, body, true),
            Expr::While { condition, body } => self.compile_while_expr(condition, body, true),
            Expr::Break => {
                let jump = self.emit_jump();
                self.loop_contexts
                    .last_mut()
                    .expect("parser rejects `break` outside loops")
                    .break_jumps
                    .push(jump);
                self.clear_const_slots();
            }
            Expr::Continue => {
                let continue_target = self
                    .loop_contexts
                    .last()
                    .expect("parser rejects `continue` outside loops")
                    .continue_target;
                self.code.push(Instruction::Jump(continue_target));
                self.clear_const_slots();
            }
            Expr::Null => {
                self.code.push(Instruction::PushNull);
            }
            Expr::Bool(value) => {
                self.code.push(Instruction::PushBool(*value));
            }
            Expr::Number(value) => {
                self.code.push(Instruction::PushNumber(*value));
            }
            Expr::String(value) => {
                let value = self.push_const(Value::String(value.clone()));
                self.code.push(Instruction::PushConst(value));
            }
            Expr::Variable(name) => {
                let name = self.push_slot(name);
                if let Some(value) = self.const_for_slot(name) {
                    self.emit_push_value(value);
                } else {
                    self.code.push(Instruction::LoadName(name));
                }
            }
            Expr::Tuple(items) => {
                for item in items {
                    self.compile_expr(item);
                    self.code.push(Instruction::DeepCopy);
                }
                self.code.push(Instruction::BuildTuple(items.len()));
            }
            Expr::List(items) => {
                for item in items {
                    self.compile_expr(item);
                    self.code.push(Instruction::DeepCopy);
                }
                self.code.push(Instruction::BuildList(items.len()));
            }
            Expr::ListComprehension { element, clauses } => {
                self.compile_list_comprehension(element, clauses);
            }
            Expr::Record(entries) => {
                for (_, value) in entries {
                    self.compile_expr(value);
                    self.code.push(Instruction::DeepCopy);
                }
                let keys = self.push_key_list(entries.iter().map(|(key, _)| key.as_str()));
                self.code.push(Instruction::BuildRecord(keys));
            }
            Expr::StartProcess(process) => {
                let instruction = self.compile_start_process_expr(process);
                if let Some(site) = self.lashlang_execution_site(
                    expr,
                    "child_process",
                    format!("start {}", process.process),
                ) {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::ProcessRef { process } => self.compile_process_ref_expr(process),
            Expr::HostDescriptorConstructor { type_name, input } => {
                self.compile_expr(input);
                let type_name = self.push_name(type_name);
                self.code.push(Instruction::WrapHostDescriptor(type_name));
            }
            Expr::ResourceRef(resource) => {
                self.emit_push_value(Value::Resource(super::ResourceHandle::new(
                    resource.resource_type.to_string(),
                    resource.alias.to_string(),
                )));
            }
            Expr::ReceiverCall { .. } | Expr::Await(_) => {
                self.compile_awaitable_effect_expr(expr, None);
            }
            Expr::SleepFor(duration) => {
                self.compile_expr(duration);
                let instruction = self.code.len();
                self.code.push(Instruction::SleepFor);
                if let Some(site) = self.lashlang_execution_site(expr, "sleep", "sleep for") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::SleepUntil(deadline) => {
                self.compile_expr(deadline);
                let instruction = self.code.len();
                self.code.push(Instruction::SleepUntil);
                if let Some(site) = self.lashlang_execution_site(expr, "sleep", "sleep until") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::WaitSignal { name } => {
                let name = self.push_name(name);
                let instruction = self.code.len();
                self.code.push(Instruction::ProcessWaitSignal { name });
                if let Some(site) = self.lashlang_execution_site(expr, "wait", "wait_signal") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::SignalRun { run, name, payload } => {
                self.compile_expr(run);
                self.compile_expr(payload);
                let name = self.push_name(name);
                let instruction = self.code.len();
                self.code.push(Instruction::ProcessSignalRun { name });
                if let Some(site) = self.lashlang_execution_site(expr, "signal", "signal_run") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::ResultUnwrap(inner) => {
                if self.compile_awaitable_effect_expr(expr, None) {
                    return;
                }
                if let Expr::Field { target, field } = inner.as_ref()
                    && let Expr::Variable(name) = target.as_ref()
                {
                    let slot = self.push_slot(name);
                    let field = self.push_name(field);
                    self.code.push(Instruction::LoadFieldUnwrap { slot, field });
                } else {
                    self.compile_expr(inner);
                    self.code.push(Instruction::ResultUnwrap);
                }
            }
            Expr::BuiltinCall { name, args } => {
                self.emit_builtin_call(name, args);
            }
            Expr::Field { target, field } => {
                if let Expr::Variable(name) = target.as_ref() {
                    let slot = self.push_slot(name);
                    let field = self.push_name(field);
                    self.code.push(Instruction::LoadField { slot, field });
                    return;
                }
                self.compile_expr(target);
                let field = self.push_name(field);
                self.code.push(Instruction::Field(field));
            }
            Expr::Index { target, index } => {
                self.compile_expr(target);
                self.compile_expr(index);
                self.code.push(Instruction::Index);
            }
            Expr::Unary { op, expr } => {
                self.compile_expr(expr);
                self.code.push(Instruction::Unary(*op));
            }
            Expr::If {
                condition,
                then_block,
                else_block,
            } => {
                let jump_to_else = self.compile_condition_jump_if_false(condition);
                if let Some(site) = self.branch_execution_site(expr) {
                    self.mark_lashlang_execution_site(jump_to_else, site);
                }
                let const_slots_before_branches = self.const_slots.clone();
                self.compile_expr(then_block);
                let jump_to_end = self.emit_jump();
                self.patch_jump(jump_to_else, self.code.len());
                self.const_slots = const_slots_before_branches;
                self.compile_expr(else_block);
                self.patch_jump(jump_to_end, self.code.len());
                self.clear_const_slots();
            }
            Expr::Cancel(handle) => {
                self.compile_expr(handle);
                self.code.push(Instruction::CancelHandle);
            }
            Expr::Print(expr) => {
                self.compile_expr(expr);
                self.code.push(Instruction::Print);
            }
            Expr::Yield(value) => {
                self.compile_expr(value);
                let instruction = self.code.len();
                self.code.push(Instruction::ProcessYield);
                if let Some(site) = self.lashlang_execution_site(expr, "process_event", "yield") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::Wake(value) => {
                self.compile_expr(value);
                let instruction = self.code.len();
                self.code.push(Instruction::ProcessWake);
                if let Some(site) = self.lashlang_execution_site(expr, "process_event", "wake") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::Finish(value) => {
                self.compile_expr(value);
                let instruction = self.code.len();
                self.code.push(Instruction::Finish);
                if let Some(site) = self.lashlang_execution_site(expr, "terminal", "result") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::Fail(value) => {
                self.compile_expr(value);
                let instruction = self.code.len();
                self.code.push(Instruction::ProcessFail);
                if let Some(site) = self.lashlang_execution_site(expr, "terminal", "failure") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::TypeLiteral(ty) => self.compile_type_literal(ty),
            Expr::Binary { left, op, right } => match op {
                BinaryOp::And => {
                    self.compile_expr(left);
                    let jump_to_false = self.emit_jump_if_false();
                    self.compile_expr(right);
                    self.code.push(Instruction::ToBool);
                    let jump_to_end = self.emit_jump();
                    self.patch_jump(jump_to_false, self.code.len());
                    self.code.push(Instruction::PushBool(false));
                    self.patch_jump(jump_to_end, self.code.len());
                }
                BinaryOp::Or => {
                    self.compile_expr(left);
                    let jump_to_true = self.emit_jump_if_true();
                    self.compile_expr(right);
                    self.code.push(Instruction::ToBool);
                    let jump_to_end = self.emit_jump();
                    self.patch_jump(jump_to_true, self.code.len());
                    self.code.push(Instruction::PushBool(true));
                    self.patch_jump(jump_to_end, self.code.len());
                }
                _ => {
                    if is_comparison_binary_op(*op) {
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
                            self.code.push(Instruction::SlotNumberBinaryCompare {
                                slot,
                                binary_op: *binary_op,
                                binary_right,
                                compare_op: *op,
                                compare_right,
                            });
                            return;
                        }
                        if let (Expr::Variable(name), Some(Value::Number(right))) =
                            (left.as_ref(), self.fold_compile_time_expr(right))
                        {
                            let slot = self.push_slot(name);
                            self.code.push(Instruction::SlotNumberCompare {
                                slot,
                                op: *op,
                                right,
                            });
                            return;
                        }
                    }
                    if is_numeric_binary_op(*op)
                        && let (Expr::Variable(name), Some(Value::Number(right))) =
                            (left.as_ref(), self.fold_compile_time_expr(right))
                    {
                        let slot = self.push_slot(name);
                        self.code.push(Instruction::SlotNumberBinary {
                            slot,
                            op: *op,
                            right,
                        });
                        return;
                    }
                    self.compile_expr(left);
                    self.compile_expr(right);
                    self.code.push(Instruction::Binary(*op));
                }
            },
        }
    }

}
