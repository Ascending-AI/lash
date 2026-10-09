use super::*;

impl Compiler {
    /// `path` is the path of `expr`, the node under the `LabelAnnotated`
    /// the caller matched on.
    pub(super) fn try_compile_label_as_effect_step(
        &mut self,
        expr: &Expr,
        label: &LabelMetadata,
        leave_value: bool,
        path: &AstPath,
    ) -> bool {
        let Some(site) = self.labeled_effect_site(expr, label, path) else {
            return false;
        };
        match expr {
            Expr::Assign { target, expr } => self.compile_assignment_expr_with_forced_effect_site(
                target,
                expr,
                leave_value,
                site,
                path,
            ),
            _ => self.compile_expr_with_forced_effect_site(expr, site, path),
        }
    }

    fn labeled_effect_site(
        &self,
        expr: &Expr,
        label: &LabelMetadata,
        path: &AstPath,
    ) -> Option<LashVmExecutionSite> {
        if label_attaches_to_concrete_node(expr) {
            self.concrete_labeled_effect_site(expr, path)
        } else {
            self.labeled_step_execution_site(path, label.title.as_str())
        }
    }

    /// `Assign`, `Await`, and `ResultUnwrap` forward their label to the
    /// concrete node inside; its path is the respective child of `path`.
    fn concrete_labeled_effect_site(
        &self,
        expr: &Expr,
        path: &AstPath,
    ) -> Option<LashVmExecutionSite> {
        match expr {
            Expr::Assign { target, expr } => self.concrete_labeled_effect_site(
                expr,
                &path.child(Self::assign_value_index(target) as u32),
            ),
            Expr::Await(expr) | Expr::ResultUnwrap(expr) => {
                self.concrete_labeled_effect_site(expr, &path.child(0))
            }
            _ => self.lash_vm_execution_site_for_expr(expr, path),
        }
    }

    /// `path` is the `Assign` node's path: the target's dynamic index steps
    /// come first in `children()` order, the value last.
    fn compile_assignment_expr_with_forced_effect_site(
        &mut self,
        target: &AssignTarget,
        expr: &Expr,
        leave_value: bool,
        site: LashVmExecutionSite,
        path: &AstPath,
    ) -> bool {
        if !expr_supports_forced_effect_site(expr) {
            return false;
        }
        let value_path = path.child(Self::assign_value_index(target) as u32);
        if target.is_simple() {
            let slot = self.push_slot(&target.root);
            if !self.compile_expr_with_forced_effect_site(expr, site, &value_path) {
                return false;
            }
            self.code.push(Instruction::StoreName(slot));
            self.set_const_slot(slot, None);
            self.push_null_if(leave_value);
            return true;
        }

        let slot = self.push_slot(&target.root);
        let mut index_child = 0usize;
        for step in &target.steps {
            if let AssignPathStep::Index(index) = step {
                self.compile_expr(index, &path.child(index_child as u32));
                index_child += 1;
            }
        }
        if !self.compile_expr_with_forced_effect_site(expr, site, &value_path) {
            return false;
        }
        let path = self.push_assign_path(&target.steps);
        self.code.push(Instruction::PathAssign { slot, path });
        self.set_const_slot(slot, None);
        self.push_null_if(leave_value);
        true
    }

    fn compile_expr_with_forced_effect_site(
        &mut self,
        expr: &Expr,
        site: LashVmExecutionSite,
        path: &AstPath,
    ) -> bool {
        self.compile_awaitable_effect_expr(expr, Some(site), path)
    }

    pub(super) fn compile_awaitable_effect_expr(
        &mut self,
        expr: &Expr,
        forced_site: Option<LashVmExecutionSite>,
        path: &AstPath,
    ) -> bool {
        match expr {
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let instruction =
                    self.compile_receiver_call_expr(receiver, operation, args, false, path);
                self.mark_awaitable_effect_site(instruction, forced_site, expr, path);
                true
            }
            Expr::Await(handle) => {
                let site = forced_site.or_else(|| self.lash_vm_execution_site_for_expr(expr, path));
                self.compile_await_handle_expr(handle, false, site, &path.child(0))
            }
            Expr::ResultUnwrap(inner) => {
                if let Expr::Await(handle) = inner.as_ref() {
                    let site = forced_site
                        .or_else(|| self.lash_vm_execution_site_for_expr(inner, &path.child(0)));
                    return self.compile_await_handle_expr(
                        handle,
                        true,
                        site,
                        &path.child(0).child(0),
                    );
                }
                if let Expr::ReceiverCall {
                    receiver,
                    operation,
                    args,
                } = inner.as_ref()
                {
                    let instruction = self.compile_receiver_call_expr(
                        receiver,
                        operation,
                        args,
                        true,
                        &path.child(0),
                    );
                    self.mark_awaitable_effect_site(
                        instruction,
                        forced_site,
                        inner,
                        &path.child(0),
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
        forced_site: Option<LashVmExecutionSite>,
        path: &AstPath,
    ) -> bool {
        if self.compile_aggregate_await_expr(handle, unwrap_result, forced_site.clone(), path) {
            return true;
        }
        match handle {
            Expr::ReceiverCall {
                receiver,
                operation,
                args,
            } => {
                let instruction =
                    self.compile_receiver_call_expr(receiver, operation, args, unwrap_result, path);
                self.mark_awaitable_effect_site(instruction, forced_site, handle, path);
            }
            Expr::ResultUnwrap(inner) => {
                if let Expr::ReceiverCall {
                    receiver,
                    operation,
                    args,
                } = inner.as_ref()
                {
                    let instruction = self.compile_receiver_call_expr(
                        receiver,
                        operation,
                        args,
                        true,
                        &path.child(0),
                    );
                    self.mark_awaitable_effect_site(
                        instruction,
                        forced_site,
                        inner,
                        &path.child(0),
                    );
                } else {
                    self.compile_expr(inner, &path.child(0));
                    let instruction = self.code.len();
                    self.code.push(Instruction::AwaitHandleUnwrap);
                    self.mark_forced_lash_vm_execution_site(instruction, forced_site);
                }
            }
            _ => {
                self.compile_expr(handle, path);
                let instruction = self.code.len();
                self.code.push(if unwrap_result {
                    Instruction::AwaitHandleUnwrap
                } else {
                    Instruction::AwaitHandle
                });
                self.mark_forced_lash_vm_execution_site(instruction, forced_site);
            }
        }
        true
    }

    fn compile_aggregate_await_expr(
        &mut self,
        handle: &Expr,
        aggregate_unwrap: bool,
        forced_site: Option<LashVmExecutionSite>,
        path: &AstPath,
    ) -> bool {
        let Some(leaf_count) = aggregate_await_shape_leaf_count(handle) else {
            return false;
        };
        if leaf_count == 0 {
            return false;
        }

        let mut leaves = Vec::with_capacity(leaf_count);
        let mut stack_value_count = 0;
        let shape =
            self.compile_aggregate_await_shape(handle, path, &mut leaves, &mut stack_value_count);
        // A LashVm-native aggregate waits for every result and reports its
        // first *written* unwrapped rejection — one input-order rule for the
        // dialect's own aggregates (ADR 0099 §10 L7). Only the TypeScript
        // `Promise.*` aggregates carry an ECMA consumer mode.
        let batch = self.push_resource_operation_batch(CompiledResourceOperationBatch {
            leaves: leaves.into_boxed_slice(),
            shape,
            stack_value_count,
            aggregate_unwrap,
            consumer: AggregateConsumer::AllSettled,
        });
        let instruction = self.code.len();
        self.code.push(Instruction::ResourceOperationBatch(batch));
        self.mark_instruction_source_span(instruction, path);
        self.mark_forced_lash_vm_execution_site(instruction, forced_site);
        true
    }

    fn compile_aggregate_await_shape(
        &mut self,
        expr: &Expr,
        path: &AstPath,
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
                path,
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
                    path,
                    receiver,
                    operation,
                    args,
                    true,
                    leaves,
                    stack_value_count,
                )
            }
            Expr::List(items) => {
                let values = items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| {
                        self.compile_aggregate_await_shape(
                            item,
                            &path.child(index as u32),
                            leaves,
                            stack_value_count,
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                CompiledAggregateAwaitShape::List(values)
            }
            Expr::Record(entries) => {
                let values = entries
                    .iter()
                    .enumerate()
                    .map(|(index, (_, value))| {
                        self.compile_aggregate_await_shape(
                            value,
                            &path.child(index as u32),
                            leaves,
                            stack_value_count,
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice();
                let keys = self.push_key_list(entries.iter().map(|(key, _)| key.as_str()));
                CompiledAggregateAwaitShape::Record { keys, values }
            }
            _ => self.compile_aggregate_await_value(expr, path, stack_value_count),
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "aggregate await leaves mirror receiver-call syntax"
    )]
    /// `site_path` is `site_expr`'s path; the receiver call it wraps (through
    /// `ResultUnwrap` when `unwrap` holds) is `call_path` below.
    fn compile_aggregate_await_leaf(
        &mut self,
        site_expr: &Expr,
        site_path: &AstPath,
        receiver: &Expr,
        operation: &str,
        args: &[Expr],
        unwrap: bool,
        leaves: &mut Vec<CompiledResourceOperationBatchLeaf>,
        stack_value_count: &mut usize,
    ) -> CompiledAggregateAwaitShape {
        let call_path = if unwrap {
            site_path.child(0)
        } else {
            site_path.clone()
        };
        let receiver_stack_index = *stack_value_count;
        self.compile_expr(receiver, &call_path.child(0));
        for (index, arg) in args.iter().enumerate() {
            self.compile_expr(arg, &call_path.child(index as u32 + 1));
        }
        let operation_index = self.push_name(operation);
        let descriptor_expr = match site_expr {
            Expr::ResultUnwrap(inner) => inner.as_ref(),
            _ => site_expr,
        };
        let site = self.lash_vm_execution_site_for_descriptor(&call_path, descriptor_expr);
        let source_span = self.expression_source_span(site_path);
        let leaf_index = leaves.len();
        leaves.push(CompiledResourceOperationBatchLeaf {
            timer: false,
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
        path: &AstPath,
        stack_value_count: &mut usize,
    ) -> CompiledAggregateAwaitShape {
        let value_index = *stack_value_count;
        self.compile_expr(expr, path);
        *stack_value_count += 1;
        CompiledAggregateAwaitShape::Value(value_index)
    }

    fn mark_awaitable_effect_site(
        &mut self,
        instruction: usize,
        forced_site: Option<LashVmExecutionSite>,
        site_expr: &Expr,
        site_path: &AstPath,
    ) {
        let site =
            forced_site.or_else(|| self.lash_vm_execution_site_for_expr(site_expr, site_path));
        self.mark_instruction_source_span(instruction, site_path);
        self.mark_forced_lash_vm_execution_site(instruction, site);
    }

    fn mark_forced_lash_vm_execution_site(
        &mut self,
        instruction: usize,
        site: Option<LashVmExecutionSite>,
    ) {
        if let Some(site) = site {
            self.mark_lash_vm_execution_site(instruction, site);
        }
    }

    pub(super) fn compile_process_ref_expr(&mut self, process: &str) {
        let Some(module_context) = self.module_context.as_ref() else {
            self.emit_push_value(Value::Null);
            return;
        };
        let Some(process_ref) = module_context.process_refs.get(process) else {
            self.emit_push_value(Value::Null);
            return;
        };
        let literal = process_ref_literal(
            &module_context.module_ref,
            &module_context.host_requirements_ref,
            process_ref,
            module_context
                .process_types
                .get(process)
                .map(|ty| {
                    lash_core_execution::ProcessSignature::known(crate::type_expr_to_json_schema(
                        ty,
                    ))
                })
                .unwrap_or(lash_core_execution::ProcessSignature::Unknown),
        );
        self.emit_push_value(literal);
    }

    fn compile_receiver_call_expr(
        &mut self,
        receiver: &Expr,
        operation: &str,
        args: &[Expr],
        unwrap: bool,
        call_path: &AstPath,
    ) -> usize {
        self.compile_expr(receiver, &call_path.child(0));
        for (index, arg) in args.iter().enumerate() {
            self.compile_expr(arg, &call_path.child(index as u32 + 1));
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

    pub(super) fn emit_jump_if_false(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::JumpIfFalse(usize::MAX));
        index
    }

    pub(super) fn compile_condition_jump_if_false(
        &mut self,
        condition: &Expr,
        path: &AstPath,
    ) -> usize {
        if let Some(value) = self.fold_compile_time_expr(condition) {
            self.emit_push_value(value);
            return self.emit_jump_if_false();
        }

        self.compile_expr(condition, path);
        self.emit_jump_if_false()
    }

    pub(super) fn emit_jump_if_true(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::JumpIfTrue(usize::MAX));
        index
    }

    pub(super) fn emit_jump(&mut self) -> usize {
        let index = self.code.len();
        self.code.push(Instruction::Jump(usize::MAX));
        index
    }

    pub(super) fn patch_jump(&mut self, index: usize, target: usize) {
        match &mut self.code[index] {
            Instruction::Jump(slot)
            | Instruction::JumpIfFalse(slot)
            | Instruction::JumpIfTrue(slot)
            | Instruction::IterNext { jump_to: slot } => *slot = target,
            _ => unreachable!("patched non-jump instruction"),
        }
    }
}

fn aggregate_await_shape_leaf_count(expr: &Expr) -> Option<usize> {
    match expr {
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

/// This is the cell-side half of the one definition codec: converted to JSON it
/// uses the immutable definition codec: canonical ID and signature. The
/// descriptor's module references stay in the published definition store.
#[expect(
    clippy::expect_used,
    reason = "compiled references form a validated descriptor"
)]
pub(crate) fn process_ref_literal(
    module_ref: &crate::ModuleRef,
    host_requirements_ref: &crate::HostRequirementsRef,
    process_ref: &crate::ProcessRef,
    signature: lash_core_execution::ProcessSignature,
) -> Value {
    let identity = crate::ProcessDefinitionIdentity::new(
        module_ref.clone(),
        host_requirements_ref.clone(),
        process_ref.clone(),
        "",
    );
    crate::from_json(
        serde_json::to_value(identity.definition(signature).expect("compiled descriptor"))
            .expect("definition serializes"),
    )
}
