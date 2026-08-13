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
                let scope_depth = self
                    .loop_contexts
                    .last()
                    .expect("parser rejects `break` outside loops")
                    .handler_scope_depth;
                self.emit_exception_scope_exit(scope_depth);
                let jump = self.emit_jump();
                self.loop_contexts
                    .last_mut()
                    .expect("parser rejects `break` outside loops")
                    .break_jumps
                    .push(jump);
                self.clear_const_slots();
            }
            Expr::Continue => {
                let (continue_target, scope_depth) = {
                    let context = self
                        .loop_contexts
                        .last()
                        .expect("parser rejects `continue` outside loops");
                    (context.continue_target, context.handler_scope_depth)
                };
                self.emit_exception_scope_exit(scope_depth);
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
            Expr::Function(function) => {
                let function_index = self.pending_functions.len();
                let cloned = (**function).clone();
                self.copy_expression_metadata(&function.body, &cloned.body);
                self.pending_functions.push(Some(cloned));
                for capture in &function.captures {
                    let slot = self.push_slot(capture);
                    self.code.push(Instruction::LoadName(slot));
                    // The lashlang AST lowering chooses by-value capture. The
                    // VM opcode itself merely stores the values it receives,
                    // so a later dialect may intentionally pass references.
                    self.code.push(Instruction::DeepCopy);
                }
                self.code.push(Instruction::MakeClosure {
                    function: function_index,
                    captures: function.captures.len(),
                });
            }
            Expr::Call { function, args } => {
                self.compile_expr(function);
                for arg in args {
                    self.compile_expr(arg);
                    self.code.push(Instruction::DeepCopy);
                }
                let instruction = self.code.len();
                self.code.push(Instruction::Call { argc: args.len() });
                if let Some(site) = self.lashlang_execution_site(expr, "call", "function call") {
                    self.mark_lashlang_execution_site(instruction, site);
                }
            }
            Expr::Map { items, function } => {
                self.compile_expr(items);
                self.compile_expr(function);
                self.code.push(Instruction::Map);
            }
            Expr::Try(scope) => self.compile_try_expr(scope),
            Expr::Throw(value) => {
                self.compile_expr(value);
                self.code.push(Instruction::Throw);
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

    fn compile_try_expr(&mut self, scope: &crate::ast::TryExpr) {
        if scope.catch.is_none() && scope.finally.is_none() {
            self.compile_expr(&scope.body);
            return;
        }

        let handler_push = self.code.len();
        self.code.push(Instruction::PushHandler {
            handler: usize::MAX,
            finally: None,
            catches: scope.catch.is_some(),
        });
        let finally_sites = scope.finally.as_ref().map(|_| {
            self.pending_finally_sites.push(Vec::new());
            self.pending_finally_sites.len() - 1
        });
        self.handler_scopes
            .push(HandlerScope::Protected { finally_sites });
        self.compile_expr(&scope.body);
        self.handler_scopes
            .pop()
            .expect("the try body's scope is popped once");
        self.code.push(Instruction::PopHandler);

        let normal_exit = self.code.len();
        if scope.finally.is_some() {
            self.code.push(Instruction::EnterFinally {
                finally: usize::MAX,
                resume: usize::MAX,
            });
        } else {
            self.code.push(Instruction::Jump(usize::MAX));
        }

        let catch_ip = self.code.len();
        let mut catch_cleanup = None;
        let mut catch_exit = None;
        if let Some(catch) = &scope.catch {
            let binding = self.push_slot(&catch.binding);
            self.code.push(Instruction::StoreName(binding));
            if scope.finally.is_some() {
                catch_cleanup = Some(self.code.len());
                self.code.push(Instruction::PushHandler {
                    handler: usize::MAX,
                    finally: None,
                    catches: false,
                });
                self.handler_scopes
                    .push(HandlerScope::Protected { finally_sites });
            }
            self.compile_expr(&catch.body);
            if scope.finally.is_some() {
                self.handler_scopes
                    .pop()
                    .expect("the catch body's cleanup scope is popped once");
                self.code.push(Instruction::PopHandler);
                catch_exit = Some(self.code.len());
                self.code.push(Instruction::EnterFinally {
                    finally: usize::MAX,
                    resume: usize::MAX,
                });
            }
        }

        let finally_ip = self.code.len();
        if let Some(finally) = &scope.finally {
            self.handler_scopes.push(HandlerScope::FinallyBody);
            self.compile_expr(finally);
            self.handler_scopes
                .pop()
                .expect("the finally body's scope is popped once");
            self.code.push(Instruction::EndFinally);
        }
        let end_ip = self.code.len();

        let handler_ip = if scope.catch.is_some() {
            catch_ip
        } else {
            finally_ip
        };
        self.code[handler_push] = Instruction::PushHandler {
            handler: handler_ip,
            finally: scope.finally.as_ref().map(|_| finally_ip),
            catches: scope.catch.is_some(),
        };
        self.handler_scope_extents.push(HandlerScopeExtent {
            push_ip: handler_push,
            handler_ip,
            finally_ip: scope.finally.as_ref().map(|_| finally_ip),
            catches: scope.catch.is_some(),
            end_ip,
        });
        if scope.finally.is_some() {
            self.code[normal_exit] = Instruction::EnterFinally {
                finally: finally_ip,
                resume: end_ip,
            };
            if let Some(catch_cleanup) = catch_cleanup {
                self.code[catch_cleanup] = Instruction::PushHandler {
                    handler: finally_ip,
                    finally: Some(finally_ip),
                    catches: false,
                };
                // The catch body's cleanup scope protects the catch body only;
                // the try scope's own handler is already gone by then, so the
                // two never sit on the handler stack together.
                self.handler_scope_extents.push(HandlerScopeExtent {
                    push_ip: catch_cleanup,
                    handler_ip: finally_ip,
                    finally_ip: Some(finally_ip),
                    catches: false,
                    end_ip: finally_ip,
                });
            }
            if let Some(catch_exit) = catch_exit {
                self.code[catch_exit] = Instruction::EnterFinally {
                    finally: finally_ip,
                    resume: end_ip,
                };
            }
            let sites = self
                .pending_finally_sites
                .pop()
                .expect("a try with a finally owns exactly one patch bucket");
            debug_assert_eq!(finally_sites, Some(self.pending_finally_sites.len()));
            for site in sites {
                let Instruction::EnterFinally { resume, .. } = self.code[site] else {
                    unreachable!("a pending finally site is an EnterFinally")
                };
                self.code[site] = Instruction::EnterFinally {
                    finally: finally_ip,
                    resume,
                };
            }
        } else {
            self.code[normal_exit] = Instruction::Jump(end_ip);
        }
        self.clear_const_slots();
    }

    /// Emits the instructions a jump edge owes the exception scopes it leaves.
    ///
    /// `break` and `continue` are abrupt completions: per ECMA-262 they run
    /// every pending `finally` between the jump and its target loop, innermost
    /// first, and every handler they cross has to come off the VM's handler
    /// stack on the way. `target_depth` is the scope depth the target loop was
    /// entered at, so nested loops unwind only as far as their own loop.
    fn emit_exception_scope_exit(&mut self, target_depth: usize) {
        for index in (target_depth..self.handler_scopes.len()).rev() {
            match self.handler_scopes[index] {
                HandlerScope::Protected { finally_sites } => {
                    self.code.push(Instruction::PopHandler);
                    if let Some(bucket) = finally_sites {
                        let site = self.code.len();
                        // The finally target is patched by `compile_try_expr`
                        // once the block has been emitted; the resume site is
                        // the rest of this jump edge, which is known now.
                        self.code.push(Instruction::EnterFinally {
                            finally: usize::MAX,
                            resume: site + 1,
                        });
                        self.pending_finally_sites[bucket].push(site);
                    }
                }
                // The pending completion of the `finally` being left is
                // replaced by the jump completion, so it is discarded rather
                // than resumed or rethrown.
                HandlerScope::FinallyBody => self.code.push(Instruction::AbandonFinally),
            }
        }
    }
}
