use super::*;

#[derive(Clone, Copy)]
pub(super) enum PatternMode {
    Initialize,
    Assign,
}

pub(super) fn pattern_names(pattern: &Pattern, output: &mut Vec<String>) {
    match pattern {
        Pattern::Ident(name) => output.push(name.clone()),
        Pattern::Rest(target) => pattern_names(target, output),
        Pattern::Member { .. } => {}
        Pattern::Assign { target, .. } => pattern_names(target, output),
        Pattern::Array { elements, rest } => {
            for element in elements.iter().flatten() {
                pattern_names(element, output);
            }
            if let Some(rest) = rest {
                pattern_names(rest, output);
            }
        }
        Pattern::Object { properties, rest } => {
            for property in properties {
                pattern_names(&property.value, output);
            }
            if let Some(rest) = rest {
                pattern_names(rest, output);
            }
        }
    }
}

pub(super) fn single_pattern_name(pattern: &Pattern) -> Option<&str> {
    match pattern {
        Pattern::Ident(name) => Some(name),
        Pattern::Rest(target) => single_pattern_name(target),
        _ => None,
    }
}

pub(super) fn function_var_names(statements: &[Stmt]) -> Vec<String> {
    fn visit(statement: &Stmt, names: &mut Vec<String>) {
        match statement {
            Stmt::Enum { name, .. } => names.push(name.clone()),
            Stmt::Var {
                kind: VarKind::Var,
                declarations,
            } => {
                for declaration in declarations {
                    pattern_names(&declaration.pattern, names);
                }
            }
            Stmt::Block(statements) => statements.iter().for_each(|stmt| visit(stmt, names)),
            Stmt::If {
                consequent,
                alternate,
                ..
            } => {
                visit(consequent, names);
                if let Some(alternate) = alternate {
                    visit(alternate, names);
                }
            }
            Stmt::While { body, .. }
            | Stmt::DoWhile { body, .. }
            | Stmt::For { body, .. }
            | Stmt::ForOf { body, .. }
            | Stmt::ForIn { body, .. } => visit(body, names),
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    case.consequent.iter().for_each(|stmt| visit(stmt, names));
                }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                body.iter().for_each(|stmt| visit(stmt, names));
                if let Some(catch) = catch {
                    catch.body.iter().for_each(|stmt| visit(stmt, names));
                }
                if let Some(finally) = finally {
                    finally.iter().for_each(|stmt| visit(stmt, names));
                }
            }
            Stmt::Function { .. }
            | Stmt::Empty
            | Stmt::Expr(_)
            | Stmt::Return(_)
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Throw(_)
            | Stmt::Var { .. } => {}
        }
    }
    let mut names = Vec::new();
    statements.iter().for_each(|stmt| visit(stmt, &mut names));
    names.sort();
    names.dedup();
    names
}

impl Lowerer {
    fn ensure_global_binding(&mut self, name: &str) -> Result<bool, Diagnostic> {
        if self.has_binding(name) {
            return Ok(false);
        }
        if matches!(name, "undefined" | "NaN" | "Infinity") {
            return Err(Diagnostic::new(
                DiagnosticCode::ReservedIdentifier,
                format!("globalThis.{name} is a reserved value identifier"),
                None,
            ));
        }
        let nested_function = self.current_function() != 0;
        let index = self.root_scope_depth.saturating_sub(1);
        let scope = self
            .scopes
            .get_mut(index)
            .expect("program root scope exists");
        scope.bindings.insert(
            name.to_string(),
            Binding {
                internal: name.to_string(),
                kind: BindingKind::Var,
                initialized: true,
                owner_function: 0,
            },
        );
        if nested_function {
            self.intrinsic_global_slots.insert(name.to_string());
        }
        Ok(true)
    }

    /// Lowers `value` in a position that materializes an iterable at once.
    ///
    /// There is one such position list, not two. `matchAll` used to accept
    /// three of these sinks while the collection iterators accepted five, for
    /// no reason either side could state: every sink here is a bounded
    /// materialization, which is the whole property the restriction exists to
    /// guarantee.
    pub(super) fn lower_iterable_sink(&mut self, value: &Expr) -> Result<LashExpr, Diagnostic> {
        self.iterable_sink_depth += 1;
        self.regexp_iterable_sink_depth += 1;
        let result = self.lower_expr(value);
        self.regexp_iterable_sink_depth -= 1;
        self.iterable_sink_depth -= 1;
        result
    }

    pub(super) fn temporary(&mut self, label: &str) -> String {
        let id = self.next_binding;
        self.next_binding += 1;
        format!("{GENERATED_BINDING_PREFIX}{id}_{label}")
    }

    fn temp_assignment(name: &str, value: LashExpr) -> LashExpr {
        LashExpr::Assign {
            target: AssignTarget::variable(name.into()),
            expr: Box::new(value),
        }
    }

    fn variable(name: &str) -> LashExpr {
        LashExpr::Variable(name.into())
    }

    fn nullish(value: LashExpr) -> LashExpr {
        LashExpr::JavaScriptLogical {
            left: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(value.clone()),
                op: JavaScriptBinaryOp::StrictEqual,
                right: Box::new(LashExpr::Null),
            }),
            op: JavaScriptLogicalOp::Or,
            right: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(value),
                op: JavaScriptBinaryOp::StrictEqual,
                right: Box::new(LashExpr::Undefined),
            }),
        }
    }

    fn stdlib_call(method: &str, args: Vec<LashExpr>) -> LashExpr {
        let mut values = vec![LashExpr::String(method.into())];
        values.extend(args);
        LashExpr::BuiltinCall {
            name: "__typescript_stdlib".into(),
            args: values,
        }
    }

    pub(super) fn lower_conversion_function(&mut self, name: &str) -> LashExpr {
        let value = self.temporary("conversion_value");
        let input = Self::variable(&value);
        let body = match name {
            "String" => js_add(LashExpr::String("".into()), input),
            "Number" => js_unary(JavaScriptUnaryOp::Plus, input),
            "Boolean" => js_unary(
                JavaScriptUnaryOp::Not,
                js_unary(JavaScriptUnaryOp::Not, input),
            ),
            _ => unreachable!(),
        };
        LashExpr::Function(Box::new(FunctionExpr {
            name: None,
            params: vec![value.into()],
            captures: Vec::new(),
            body: Box::new(body),
        }))
    }

    fn iterable_copy(value: LashExpr) -> LashExpr {
        Self::stdlib_call("Lash.ArrayFromIterable", vec![value])
    }

    pub(super) fn lower_array_literal(
        &mut self,
        elements: &[ArrayElement],
    ) -> Result<LashExpr, Diagnostic> {
        if elements
            .iter()
            .all(|element| matches!(element, ArrayElement::Value(_)))
        {
            return Ok(LashExpr::List(
                elements
                    .iter()
                    .map(|element| match element {
                        ArrayElement::Value(value) => self.lower_expr(value),
                        ArrayElement::Spread(_) => unreachable!(),
                    })
                    .collect::<Result<_, _>>()?,
            ));
        }
        let result = self.temporary("array_spread");
        let mut expressions = vec![Self::temp_assignment(&result, LashExpr::List(Vec::new()))];
        for element in elements {
            let next = match element {
                ArrayElement::Value(value) => LashExpr::List(vec![self.lower_expr(value)?]),
                ArrayElement::Spread(value) => {
                    let value = self.lower_iterable_sink(value)?;
                    Self::iterable_copy(value)
                }
            };
            expressions.push(Self::temp_assignment(
                &result,
                Self::stdlib_call("concat", vec![Self::variable(&result), next]),
            ));
        }
        expressions.push(Self::variable(&result));
        Ok(LashExpr::Block(expressions))
    }

    pub(super) fn lower_for_each(
        &mut self,
        pattern: &Pattern,
        kind: Option<VarKind>,
        source: &Expr,
        body: &Stmt,
        keys: bool,
    ) -> Result<Vec<LashExpr>, Diagnostic> {
        self.scopes.push(Scope::default());
        let mode = if let Some(kind) = kind {
            let mut names = Vec::new();
            pattern_names(pattern, &mut names);
            for name in names {
                self.declare(
                    &name,
                    match kind {
                        VarKind::Const => BindingKind::Const,
                        VarKind::Let => BindingKind::Let,
                        VarKind::Var => BindingKind::Var,
                    },
                    false,
                    false,
                )?;
            }
            PatternMode::Initialize
        } else {
            PatternMode::Assign
        };
        let iteration = self.temporary(if keys { "for_in_key" } else { "for_of_value" });
        let direct_exotic = match source {
            Expr::New { constructor, .. } if constructor == "Map" => Some("entries"),
            Expr::New { constructor, .. } if constructor == "Set" => Some("values"),
            Expr::New { constructor, .. } if constructor == "URLSearchParams" => Some("entries"),
            _ => None,
        };
        let source = if keys {
            self.lower_expr(source)?
        } else {
            self.lower_iterable_sink(source)?
        };
        let iterable = if keys {
            Self::stdlib_call("Object.keys", vec![source])
        } else if let Some(method) = direct_exotic {
            Self::stdlib_call(method, vec![source])
        } else {
            Self::iterable_copy(source)
        };
        self.loop_depth += 1;
        self.continue_epilogues.push(None);
        let mut expressions =
            self.lower_pattern(pattern, LashExpr::Variable(iteration.as_str().into()), mode)?;
        expressions.push(self.lower_stmt_block(body)?);
        self.continue_epilogues.pop();
        self.loop_depth -= 1;
        self.scopes.pop();
        Ok(vec![LashExpr::For {
            binding: iteration.into(),
            iterable: Box::new(iterable),
            body: Box::new(LashExpr::Block(expressions)),
        }])
    }

    pub(super) fn lower_switch(
        &mut self,
        discriminant: &Expr,
        cases: &[adapter::SwitchCase],
    ) -> Result<LashExpr, Diagnostic> {
        let value = self.temporary("switch_value");
        let matched = self.temporary("switch_match");
        let broken = self.temporary("switch_broken");
        let mut output = vec![
            Self::temp_assignment(&value, self.lower_expr(discriminant)?),
            Self::temp_assignment(&matched, LashExpr::Number(-1.0)),
            Self::temp_assignment(&broken, LashExpr::Bool(false)),
        ];
        let default_index = cases.iter().position(|case| case.test.is_none());
        for (index, case) in cases.iter().enumerate() {
            let Some(test) = &case.test else {
                continue;
            };
            output.push(LashExpr::If {
                condition: Box::new(LashExpr::JavaScriptBinary {
                    left: Box::new(Self::variable(&matched)),
                    op: JavaScriptBinaryOp::StrictEqual,
                    right: Box::new(LashExpr::Number(-1.0)),
                }),
                then_block: Box::new(LashExpr::If {
                    condition: Box::new(LashExpr::JavaScriptBinary {
                        left: Box::new(Self::variable(&value)),
                        op: JavaScriptBinaryOp::StrictEqual,
                        right: Box::new(self.lower_expr(test)?),
                    }),
                    then_block: Box::new(Self::temp_assignment(
                        &matched,
                        LashExpr::Number(index as f64),
                    )),
                    else_block: Box::new(LashExpr::Undefined),
                }),
                else_block: Box::new(LashExpr::Undefined),
            });
        }
        if let Some(default_index) = default_index {
            output.push(LashExpr::If {
                condition: Box::new(LashExpr::JavaScriptBinary {
                    left: Box::new(Self::variable(&matched)),
                    op: JavaScriptBinaryOp::StrictEqual,
                    right: Box::new(LashExpr::Number(-1.0)),
                }),
                then_block: Box::new(Self::temp_assignment(
                    &matched,
                    LashExpr::Number(default_index as f64),
                )),
                else_block: Box::new(LashExpr::Undefined),
            });
        }
        self.switch_breaks.push((broken.clone(), self.loop_depth));
        for (index, case) in cases.iter().enumerate() {
            let body = LashExpr::Block(self.lower_statements(&case.consequent, false)?);
            output.push(LashExpr::If {
                condition: Box::new(LashExpr::JavaScriptLogical {
                    left: Box::new(LashExpr::JavaScriptLogical {
                        left: Box::new(LashExpr::JavaScriptBinary {
                            left: Box::new(Self::variable(&matched)),
                            op: JavaScriptBinaryOp::GreaterEqual,
                            right: Box::new(LashExpr::Number(0.0)),
                        }),
                        op: JavaScriptLogicalOp::And,
                        right: Box::new(LashExpr::JavaScriptBinary {
                            left: Box::new(Self::variable(&matched)),
                            op: JavaScriptBinaryOp::LessEqual,
                            right: Box::new(LashExpr::Number(index as f64)),
                        }),
                    }),
                    op: JavaScriptLogicalOp::And,
                    right: Box::new(js_unary(JavaScriptUnaryOp::Not, Self::variable(&broken))),
                }),
                then_block: Box::new(body),
                else_block: Box::new(LashExpr::Undefined),
            });
        }
        self.switch_breaks.pop();
        output.push(LashExpr::Undefined);
        Ok(LashExpr::Block(output))
    }

    pub(super) fn lower_object_literal(
        &mut self,
        properties: &[ObjectProperty],
    ) -> Result<LashExpr, Diagnostic> {
        if properties.iter().all(|property| {
            matches!(
                property,
                ObjectProperty::KeyValue(PropertyKey::Static(_), _)
            )
        }) {
            return Ok(LashExpr::Record(
                properties
                    .iter()
                    .map(|property| match property {
                        ObjectProperty::KeyValue(PropertyKey::Static(name), value) => {
                            Ok((name.as_str().into(), self.lower_expr(value)?))
                        }
                        _ => unreachable!(),
                    })
                    .collect::<Result<_, Diagnostic>>()?,
            ));
        }
        let result = self.temporary("object_literal");
        let mut expressions = vec![Self::temp_assignment(&result, LashExpr::Record(Vec::new()))];
        for property in properties {
            match property {
                ObjectProperty::KeyValue(key, value) => {
                    let key = self.lower_property_key(key)?;
                    expressions.push(LashExpr::Assign {
                        target: AssignTarget {
                            root: result.as_str().into(),
                            steps: vec![AssignPathStep::Index(key)],
                        },
                        expr: Box::new(self.lower_expr(value)?),
                    });
                }
                ObjectProperty::Spread(value) => {
                    let source = self.temporary("object_spread_source");
                    let entry = self.temporary("object_spread_entry");
                    expressions.push(Self::temp_assignment(&source, self.lower_expr(value)?));
                    let copy = LashExpr::If {
                        condition: Box::new(Self::nullish(Self::variable(&source))),
                        then_block: Box::new(LashExpr::Undefined),
                        else_block: Box::new(LashExpr::For {
                            binding: entry.as_str().into(),
                            iterable: Box::new(Self::stdlib_call(
                                "Object.entries",
                                vec![Self::variable(&source)],
                            )),
                            body: Box::new(LashExpr::Assign {
                                target: AssignTarget {
                                    root: result.as_str().into(),
                                    steps: vec![AssignPathStep::Index(LashExpr::Index {
                                        target: Box::new(Self::variable(&entry)),
                                        index: Box::new(LashExpr::Number(0.0)),
                                    })],
                                },
                                expr: Box::new(LashExpr::Index {
                                    target: Box::new(Self::variable(&entry)),
                                    index: Box::new(LashExpr::Number(1.0)),
                                }),
                            }),
                        }),
                    };
                    expressions.push(copy);
                }
            }
        }
        expressions.push(Self::variable(&result));
        Ok(LashExpr::Block(expressions))
    }

    fn lower_property_key(&mut self, key: &PropertyKey) -> Result<LashExpr, Diagnostic> {
        match key {
            PropertyKey::Static(name) => Ok(LashExpr::String(name.as_str().into())),
            PropertyKey::Computed(value) => self.lower_expr(value),
        }
    }

    pub(super) fn lower_pattern(
        &mut self,
        pattern: &Pattern,
        value: LashExpr,
        mode: PatternMode,
    ) -> Result<Vec<LashExpr>, Diagnostic> {
        match pattern {
            Pattern::Ident(name) => {
                let target = match mode {
                    PatternMode::Initialize => {
                        let internal = self.binding(name)?.internal.clone();
                        self.initialize(name);
                        AssignTarget::variable(internal.into())
                    }
                    PatternMode::Assign => {
                        self.lower_assign_target(&TsAssignTarget::Ident(name.clone()))?
                    }
                };
                Ok(vec![LashExpr::Assign {
                    target,
                    expr: Box::new(value),
                }])
            }
            Pattern::Rest(target) => self.lower_pattern(target, value, mode),
            Pattern::Member { object, property } => {
                if matches!(mode, PatternMode::Initialize) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        "member targets are not valid binding patterns",
                        None,
                    ));
                }
                Ok(vec![LashExpr::Assign {
                    target: self.member_assign_target(object, property)?,
                    expr: Box::new(value),
                }])
            }
            Pattern::Assign { target, default } => {
                let input = self.temporary("pattern_default");
                let chosen = LashExpr::If {
                    condition: Box::new(LashExpr::JavaScriptBinary {
                        left: Box::new(Self::variable(&input)),
                        op: JavaScriptBinaryOp::StrictEqual,
                        right: Box::new(LashExpr::Undefined),
                    }),
                    then_block: Box::new(self.lower_expr(default)?),
                    else_block: Box::new(Self::variable(&input)),
                };
                let mut output = vec![Self::temp_assignment(&input, value)];
                output.extend(self.lower_pattern(target, chosen, mode)?);
                Ok(output)
            }
            Pattern::Array { elements, rest } => {
                let input = self.temporary("array_pattern");
                let mut output = vec![Self::temp_assignment(&input, Self::iterable_copy(value))];
                for (index, element) in elements.iter().enumerate() {
                    if let Some(element) = element {
                        output.extend(self.lower_pattern(
                            element,
                            LashExpr::Index {
                                target: Box::new(Self::variable(&input)),
                                index: Box::new(LashExpr::Number(index as f64)),
                            },
                            mode,
                        )?);
                    }
                }
                if let Some(rest) = rest {
                    output.extend(self.lower_pattern(
                        rest,
                        Self::stdlib_call(
                            "slice",
                            vec![
                                Self::variable(&input),
                                LashExpr::Number(elements.len() as f64),
                            ],
                        ),
                        mode,
                    )?);
                }
                Ok(output)
            }
            Pattern::Object { properties, rest } => {
                let input = self.temporary("object_pattern");
                let mut output = vec![Self::temp_assignment(&input, value)];
                let mut keys = Vec::new();
                for property in properties {
                    let key_name = self.temporary("object_pattern_key");
                    output.push(Self::temp_assignment(
                        &key_name,
                        self.lower_property_key(&property.key)?,
                    ));
                    keys.push(key_name.clone());
                    output.extend(self.lower_pattern(
                        &property.value,
                        LashExpr::Index {
                            target: Box::new(Self::variable(&input)),
                            index: Box::new(Self::variable(&key_name)),
                        },
                        mode,
                    )?);
                }
                if let Some(rest) = rest {
                    let copy = self.temporary("object_rest");
                    output.push(Self::temp_assignment(
                        &copy,
                        Self::stdlib_call(
                            "Object.fromEntries",
                            vec![Self::stdlib_call(
                                "Object.entries",
                                vec![Self::variable(&input)],
                            )],
                        ),
                    ));
                    for key in keys {
                        output.push(LashExpr::BuiltinCall {
                            name: "__typescript_heap_delete_member".into(),
                            args: vec![Self::variable(&copy), Self::variable(&key)],
                        });
                    }
                    output.extend(self.lower_pattern(rest, Self::variable(&copy), mode)?);
                }
                Ok(output)
            }
        }
    }

    fn reference(
        &mut self,
        target: &TsAssignTarget,
    ) -> Result<(Vec<LashExpr>, LashExpr, AssignTarget), Diagnostic> {
        match target {
            TsAssignTarget::Ident(_) => {
                let target = self.lower_assign_target(target)?;
                Ok((Vec::new(), LashExpr::Variable(target.root.clone()), target))
            }
            TsAssignTarget::Member { object, property } => {
                if let Some(global) = global_this_member_name(object, property) {
                    if self.current_function() != 0 {
                        return Err(Diagnostic::new(
                            DiagnosticCode::UnsupportedExpression,
                            "Unsupported: assigning a direct globalThis property inside a function. Pass a session-state object into the function and mutate one of its nested fields.",
                            None,
                        ));
                    }
                    let created = self.ensure_global_binding(global)?;
                    let target = AssignTarget::variable(global.into());
                    let setup = created
                        .then(|| Self::temp_assignment(global, LashExpr::Undefined))
                        .into_iter()
                        .collect();
                    return Ok((setup, LashExpr::Variable(global.into()), target));
                }
                let base = self.temporary("reference_base");
                let mut setup = vec![Self::temp_assignment(&base, self.lower_expr(object)?)];
                let (step, read) = match property {
                    MemberProperty::Field(field) => (
                        AssignPathStep::Field(field.as_str().into()),
                        LashExpr::Field {
                            target: Box::new(Self::variable(&base)),
                            field: field.as_str().into(),
                        },
                    ),
                    MemberProperty::Index(index) => {
                        let key = self.temporary("reference_key");
                        setup.push(Self::temp_assignment(&key, self.lower_expr(index)?));
                        (
                            AssignPathStep::Index(Self::variable(&key)),
                            LashExpr::Index {
                                target: Box::new(Self::variable(&base)),
                                index: Box::new(Self::variable(&key)),
                            },
                        )
                    }
                };
                Ok((
                    setup,
                    read,
                    AssignTarget {
                        root: base.into(),
                        steps: vec![step],
                    },
                ))
            }
            TsAssignTarget::Pattern(_) => Err(Diagnostic::new(
                DiagnosticCode::UnsupportedExpression,
                "a destructuring pattern is not a scalar reference",
                None,
            )),
        }
    }

    pub(super) fn lower_assignment(
        &mut self,
        target: &TsAssignTarget,
        op: AssignOp,
        value: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        if matches!(op, AssignOp::Assign) && matches!(target, TsAssignTarget::Ident(_)) {
            let target = self.lower_assign_target(target)?;
            let result = LashExpr::Variable(target.root.clone());
            return Ok(LashExpr::Block(vec![
                LashExpr::Assign {
                    target,
                    expr: Box::new(self.lower_expr(value)?),
                },
                result,
            ]));
        }
        if matches!(op, AssignOp::Assign)
            && let TsAssignTarget::Member { object, property } = target
            && let Some(global) = global_this_member_name(object, property)
        {
            self.ensure_global_binding(global)?;
            let result = self.temporary("global_assignment");
            if self.current_function() != 0 {
                return Ok(LashExpr::Block(vec![
                    Self::temp_assignment(&result, self.lower_expr(value)?),
                    LashExpr::BuiltinCall {
                        name: "__typescript_global_set".into(),
                        args: vec![LashExpr::String(global.into()), Self::variable(&result)],
                    },
                ]));
            }
            return Ok(LashExpr::Block(vec![
                Self::temp_assignment(&result, self.lower_expr(value)?),
                LashExpr::Assign {
                    target: AssignTarget::variable(global.into()),
                    expr: Box::new(Self::variable(&result)),
                },
                Self::variable(&result),
            ]));
        }
        if let TsAssignTarget::Pattern(pattern) = target {
            if !matches!(op, AssignOp::Assign) {
                return Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    "destructuring targets only support plain assignment",
                    None,
                ));
            }
            let result = self.temporary("destructure_result");
            let mut output = vec![Self::temp_assignment(&result, self.lower_expr(value)?)];
            output.extend(self.lower_pattern(
                pattern,
                Self::variable(&result),
                PatternMode::Assign,
            )?);
            output.push(Self::variable(&result));
            return Ok(LashExpr::Block(output));
        }
        let (mut output, old, target) = self.reference(target)?;
        let result = self.temporary("assignment_result");
        match op {
            AssignOp::Assign => {
                output.push(Self::temp_assignment(&result, self.lower_expr(value)?));
                output.push(LashExpr::Assign {
                    target,
                    expr: Box::new(Self::variable(&result)),
                });
                output.push(Self::variable(&result));
            }
            AssignOp::Binary(op) => {
                let rhs = self.lower_expr(value)?;
                output.push(Self::temp_assignment(
                    &result,
                    self.lower_binary_values(old, op, rhs)?,
                ));
                output.push(LashExpr::Assign {
                    target,
                    expr: Box::new(Self::variable(&result)),
                });
                output.push(Self::variable(&result));
            }
            AssignOp::Logical(op) => {
                let should_keep = match op {
                    LogicalOp::And => js_unary(JavaScriptUnaryOp::Not, old.clone()),
                    LogicalOp::Or => old.clone(),
                    LogicalOp::Nullish => {
                        js_unary(JavaScriptUnaryOp::Not, Self::nullish(old.clone()))
                    }
                };
                let rhs = self.lower_expr(value)?;
                let write = LashExpr::Block(vec![
                    Self::temp_assignment(&result, rhs),
                    LashExpr::Assign {
                        target,
                        expr: Box::new(Self::variable(&result)),
                    },
                    Self::variable(&result),
                ]);
                output.push(LashExpr::If {
                    condition: Box::new(should_keep),
                    then_block: Box::new(old),
                    else_block: Box::new(write),
                });
            }
        }
        Ok(LashExpr::Block(output))
    }

    pub(super) fn lower_update(
        &mut self,
        target: &TsAssignTarget,
        delta: f64,
        prefix: bool,
    ) -> Result<LashExpr, Diagnostic> {
        let (mut output, old, target) = self.reference(target)?;
        let old_number = self.temporary("update_old");
        let new_number = self.temporary("update_new");
        output.push(Self::temp_assignment(
            &old_number,
            js_unary(JavaScriptUnaryOp::Plus, old),
        ));
        output.push(Self::temp_assignment(
            &new_number,
            js_add(Self::variable(&old_number), LashExpr::Number(delta)),
        ));
        output.push(LashExpr::Assign {
            target,
            expr: Box::new(Self::variable(&new_number)),
        });
        output.push(Self::variable(if prefix {
            &new_number
        } else {
            &old_number
        }));
        Ok(LashExpr::Block(output))
    }

    pub(super) fn lower_delete(
        &mut self,
        object: &Expr,
        property: &MemberProperty,
    ) -> Result<LashExpr, Diagnostic> {
        if let Some(global) = global_this_member_name(object, property) {
            return Ok(LashExpr::BuiltinCall {
                name: "__typescript_global_delete".into(),
                args: vec![LashExpr::String(global.into())],
            });
        }
        let key = match property {
            MemberProperty::Field(field) => LashExpr::String(field.as_str().into()),
            MemberProperty::Index(index) => self.lower_expr(index)?,
        };
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_heap_delete_member".into(),
            args: vec![self.lower_expr(object)?, key],
        })
    }

    pub(super) fn lower_optional_chain(
        &mut self,
        base: &Expr,
        operations: &[OptionalOperation],
    ) -> Result<LashExpr, Diagnostic> {
        let current = self.temporary("optional_chain");
        let base = self.lower_expr(base)?;
        let tail = self.lower_optional_operations(Self::variable(&current), operations)?;
        Ok(LashExpr::Block(vec![
            Self::temp_assignment(&current, base),
            tail,
        ]))
    }

    fn lower_optional_operations(
        &mut self,
        current: LashExpr,
        operations: &[OptionalOperation],
    ) -> Result<LashExpr, Diagnostic> {
        let Some((operation, tail)) = operations.split_first() else {
            return Ok(current);
        };
        let optional = match operation {
            OptionalOperation::Member { optional, .. }
            | OptionalOperation::Call { optional, .. } => *optional,
        };
        let apply = match operation {
            OptionalOperation::Member { property, .. } => match property {
                MemberProperty::Field(field) => LashExpr::Field {
                    target: Box::new(current.clone()),
                    field: field.as_str().into(),
                },
                MemberProperty::Index(index) => LashExpr::Index {
                    target: Box::new(current.clone()),
                    index: Box::new(self.lower_expr(index)?),
                },
            },
            OptionalOperation::Call { args, .. } => {
                self.lower_dynamic_call_value(current.clone(), args)?
            }
        };
        let next = self.temporary("optional_value");
        let continuation = LashExpr::Block(vec![
            Self::temp_assignment(&next, apply),
            self.lower_optional_operations(Self::variable(&next), tail)?,
        ]);
        if optional {
            Ok(LashExpr::If {
                condition: Box::new(Self::nullish(current)),
                then_block: Box::new(LashExpr::Undefined),
                else_block: Box::new(continuation),
            })
        } else {
            Ok(continuation)
        }
    }

    pub(super) fn lower_dynamic_call_value(
        &mut self,
        callee: LashExpr,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        let arguments = self.lower_argument_list(args)?;
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_call_dynamic".into(),
            args: vec![callee, arguments],
        })
    }

    pub(super) fn lower_argument_list(&mut self, args: &[CallArg]) -> Result<LashExpr, Diagnostic> {
        let result = self.temporary("call_arguments");
        let mut output = vec![Self::temp_assignment(&result, LashExpr::List(Vec::new()))];
        for argument in args {
            let next = match argument {
                CallArg::Value(value) => LashExpr::List(vec![self.lower_expr(value)?]),
                CallArg::Spread(value) => {
                    let value = self.lower_iterable_sink(value)?;
                    Self::iterable_copy(value)
                }
            };
            output.push(Self::temp_assignment(
                &result,
                Self::stdlib_call("concat", vec![Self::variable(&result), next]),
            ));
        }
        output.push(Self::variable(&result));
        Ok(LashExpr::Block(output))
    }

    pub(super) fn lower_binary_expr(
        &mut self,
        left: &Expr,
        op: BinaryOp,
        right: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        if op == BinaryOp::InstanceOf {
            return self.lower_instanceof(left, right);
        }
        if op == BinaryOp::In {
            if matches!(right, Expr::Ident(name) if name == "globalThis" && !self.has_binding(name))
            {
                let Expr::String(name) = left else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        "globalThis presence checks require a string literal key",
                        None,
                    ));
                };
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_global_has".into(),
                    args: vec![LashExpr::String(name.as_str().into())],
                });
            }
            return Ok(Self::stdlib_call(
                "Object.hasOwn",
                vec![self.lower_expr(right)?, self.lower_expr(left)?],
            ));
        }
        let left = self.lower_expr(left)?;
        let right = self.lower_expr(right)?;
        self.lower_binary_values(left, op, right)
    }

    fn lower_binary_values(
        &mut self,
        left: LashExpr,
        op: BinaryOp,
        right: LashExpr,
    ) -> Result<LashExpr, Diagnostic> {
        Ok(match op {
            BinaryOp::Exponent => Self::stdlib_call("Math.pow", vec![left, right]),
            BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                let left_name = self.temporary("bitwise_left");
                let right_name = self.temporary("bitwise_right");
                LashExpr::Block(vec![
                    Self::temp_assignment(&left_name, left),
                    Self::temp_assignment(&right_name, right),
                    self.lower_bitwise_pair(
                        Self::variable(&left_name),
                        op,
                        Self::variable(&right_name),
                    ),
                ])
            }
            BinaryOp::ShiftLeft | BinaryOp::ShiftRight | BinaryOp::ShiftRightUnsigned => {
                let left_name = self.temporary("shift_left");
                let right_name = self.temporary("shift_right");
                LashExpr::Block(vec![
                    Self::temp_assignment(&left_name, left),
                    Self::temp_assignment(&right_name, right),
                    self.lower_shift(Self::variable(&left_name), op, Self::variable(&right_name)),
                ])
            }
            BinaryOp::In | BinaryOp::InstanceOf => unreachable!("handled before value lowering"),
            op => LashExpr::JavaScriptBinary {
                left: Box::new(left),
                op: map_binary(op),
                right: Box::new(right),
            },
        })
    }

    pub(super) fn lower_bit_not(&mut self, value: &Expr) -> Result<LashExpr, Diagnostic> {
        let lowered = self.lower_expr(value)?;
        let input = self.temporary("bit_not");
        let value = self.to_uint32(Self::variable(&input));
        Ok(LashExpr::Block(vec![
            Self::temp_assignment(&input, lowered),
            self.to_int32(js_subtract(LashExpr::Number(4_294_967_295.0), value)),
        ]))
    }

    fn to_uint32(&self, value: LashExpr) -> LashExpr {
        let finite = Self::stdlib_call("Number.isFinite", vec![value.clone()]);
        let truncated = Self::stdlib_call("Math.trunc", vec![value]);
        let modulo = LashExpr::JavaScriptBinary {
            left: Box::new(truncated),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(4_294_967_296.0)),
        };
        let positive = LashExpr::JavaScriptBinary {
            left: Box::new(js_add(modulo, LashExpr::Number(4_294_967_296.0))),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(4_294_967_296.0)),
        };
        LashExpr::If {
            condition: Box::new(finite),
            then_block: Box::new(positive),
            else_block: Box::new(LashExpr::Number(0.0)),
        }
    }

    fn to_int32(&self, value: LashExpr) -> LashExpr {
        LashExpr::If {
            condition: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(value.clone()),
                op: JavaScriptBinaryOp::GreaterEqual,
                right: Box::new(LashExpr::Number(2_147_483_648.0)),
            }),
            then_block: Box::new(js_subtract(
                value.clone(),
                LashExpr::Number(4_294_967_296.0),
            )),
            else_block: Box::new(value),
        }
    }

    fn bit_at(value: LashExpr, power: f64) -> LashExpr {
        let quotient = Self::stdlib_call(
            "Math.floor",
            vec![LashExpr::JavaScriptBinary {
                left: Box::new(value),
                op: JavaScriptBinaryOp::Divide,
                right: Box::new(LashExpr::Number(power)),
            }],
        );
        LashExpr::JavaScriptBinary {
            left: Box::new(quotient),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(2.0)),
        }
    }

    fn balanced_sum(mut values: Vec<LashExpr>) -> LashExpr {
        while values.len() > 1 {
            values = values
                .chunks(2)
                .map(|chunk| match chunk {
                    [left, right] => js_add(left.clone(), right.clone()),
                    [value] => value.clone(),
                    _ => unreachable!(),
                })
                .collect();
        }
        values.pop().unwrap_or(LashExpr::Number(0.0))
    }

    fn lower_bitwise_pair(&mut self, left: LashExpr, op: BinaryOp, right: LashExpr) -> LashExpr {
        let left = self.to_uint32(left);
        let right = self.to_uint32(right);
        let bits = (0..32)
            .map(|index| {
                let power = 2_f64.powi(index);
                let left_bit = Self::bit_at(left.clone(), power);
                let right_bit = Self::bit_at(right.clone(), power);
                let bit = match op {
                    BinaryOp::BitAnd => LashExpr::JavaScriptBinary {
                        left: Box::new(left_bit),
                        op: JavaScriptBinaryOp::Multiply,
                        right: Box::new(right_bit),
                    },
                    BinaryOp::BitOr => LashExpr::If {
                        condition: Box::new(js_add(left_bit, right_bit)),
                        then_block: Box::new(LashExpr::Number(1.0)),
                        else_block: Box::new(LashExpr::Number(0.0)),
                    },
                    BinaryOp::BitXor => LashExpr::JavaScriptBinary {
                        left: Box::new(js_add(left_bit, right_bit)),
                        op: JavaScriptBinaryOp::Remainder,
                        right: Box::new(LashExpr::Number(2.0)),
                    },
                    _ => unreachable!(),
                };
                LashExpr::JavaScriptBinary {
                    left: Box::new(bit),
                    op: JavaScriptBinaryOp::Multiply,
                    right: Box::new(LashExpr::Number(power)),
                }
            })
            .collect();
        self.to_int32(Self::balanced_sum(bits))
    }

    fn lower_shift(&mut self, left: LashExpr, op: BinaryOp, right: LashExpr) -> LashExpr {
        let left = self.to_uint32(left);
        let shift = LashExpr::JavaScriptBinary {
            left: Box::new(self.to_uint32(right)),
            op: JavaScriptBinaryOp::Remainder,
            right: Box::new(LashExpr::Number(32.0)),
        };
        let factor = Self::stdlib_call("Math.pow", vec![LashExpr::Number(2.0), shift]);
        match op {
            BinaryOp::ShiftLeft => self.to_int32(LashExpr::JavaScriptBinary {
                left: Box::new(LashExpr::JavaScriptBinary {
                    left: Box::new(left),
                    op: JavaScriptBinaryOp::Multiply,
                    right: Box::new(factor),
                }),
                op: JavaScriptBinaryOp::Remainder,
                right: Box::new(LashExpr::Number(4_294_967_296.0)),
            }),
            BinaryOp::ShiftRightUnsigned => Self::stdlib_call(
                "Math.floor",
                vec![LashExpr::JavaScriptBinary {
                    left: Box::new(left),
                    op: JavaScriptBinaryOp::Divide,
                    right: Box::new(factor),
                }],
            ),
            BinaryOp::ShiftRight => Self::stdlib_call(
                "Math.floor",
                vec![LashExpr::JavaScriptBinary {
                    left: Box::new(self.to_int32(left)),
                    op: JavaScriptBinaryOp::Divide,
                    right: Box::new(factor),
                }],
            ),
            _ => unreachable!(),
        }
    }

    fn lower_instanceof(&mut self, left: &Expr, right: &Expr) -> Result<LashExpr, Diagnostic> {
        let Expr::Ident(constructor) = right else {
            return Err(Diagnostic::new(
                DiagnosticCode::InstanceOfUnsupported,
                "Unsupported: instanceof with a dynamic RHS. Use err.name checks or Array.isArray(value).",
                None,
            ));
        };
        if self.has_binding(constructor) {
            return Err(Diagnostic::new(
                DiagnosticCode::InstanceOfUnsupported,
                "Unsupported: instanceof with an authored RHS. Use err.name checks or Array.isArray(value).",
                None,
            ));
        }
        match constructor.as_str() {
            "Array" => Ok(Self::stdlib_call(
                "Array.isArray",
                vec![self.lower_expr(left)?],
            )),
            "Object" => {
                let input = self.temporary("instanceof_object");
                let value = Self::variable(&input);
                Ok(LashExpr::Block(vec![
                    Self::temp_assignment(&input, self.lower_expr(left)?),
                    LashExpr::JavaScriptLogical {
                        left: Box::new(LashExpr::JavaScriptBinary {
                            left: Box::new(js_unary(JavaScriptUnaryOp::TypeOf, value.clone())),
                            op: JavaScriptBinaryOp::StrictEqual,
                            right: Box::new(LashExpr::String("object".into())),
                        }),
                        op: JavaScriptLogicalOp::And,
                        right: Box::new(js_unary(JavaScriptUnaryOp::Not, Self::nullish(value))),
                    },
                ]))
            }
            "Error" | "TypeError" | "RangeError" | "SyntaxError" | "ReferenceError"
            | "URIError" | "EvalError" | "AggregateError" | "Map" | "Set" | "Date" | "RegExp"
            | "URL" | "URLSearchParams" => Ok(LashExpr::BuiltinCall {
                name: "__typescript_heap_instanceof".into(),
                args: vec![
                    self.lower_expr(left)?,
                    LashExpr::String(constructor.as_str().into()),
                ],
            }),
            "Promise" => Err(Diagnostic::new(
                DiagnosticCode::InstanceOfUnsupported,
                "Unsupported: instanceof Promise. Await agent promises directly.",
                None,
            )),
            _ => Err(Diagnostic::new(
                DiagnosticCode::InstanceOfUnsupported,
                format!(
                    "Unsupported: instanceof {constructor}. Use err.name checks or Array.isArray(value)."
                ),
                None,
            )),
        }
    }

    pub(super) fn lower_constructor(
        &mut self,
        constructor: &str,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        if constructor == "Promise" && !self.has_binding(constructor) {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                "Unsupported: new Promise/setTimeout. Await durable sleep(milliseconds) directly, or await the agent operation itself.",
                None,
            ));
        }
        if args.iter().any(|arg| matches!(arg, CallArg::Spread(_))) {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                "Unsupported: spread arguments in built-in constructors. Materialize constructor arguments explicitly.",
                None,
            ));
        }
        let allowed = matches!(
            constructor,
            "Error"
                | "TypeError"
                | "RangeError"
                | "SyntaxError"
                | "ReferenceError"
                | "URIError"
                | "EvalError"
                | "AggregateError"
                | "Map"
                | "Set"
                | "Date"
                | "RegExp"
                | "URL"
                | "URLSearchParams"
        );
        if !allowed || self.has_binding(constructor) {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                format!(
                    "Unsupported: new {constructor}. Use Error-family, Map, Set, Date, RegExp, URL, or URLSearchParams constructors."
                ),
                None,
            ));
        }
        let valid_arity = match constructor {
            "URL" => matches!(args.len(), 1 | 2),
            "URLSearchParams" => args.len() <= 1,
            "RegExp" => args.len() <= 2,
            _ => true,
        };
        if !valid_arity {
            return Err(Diagnostic::new(
                DiagnosticCode::NewUnsupported,
                format!(
                    "new `{constructor}` does not accept {} argument(s) in the TypeScript runtime surface",
                    args.len()
                ),
                None,
            ));
        }
        if constructor == "RegExp" {
            for (index, argument) in args.iter().enumerate() {
                let CallArg::Value(argument) = argument else {
                    unreachable!("constructor spread was rejected before arity validation")
                };
                if matches!(
                    argument,
                    Expr::Null
                        | Expr::Bool(_)
                        | Expr::Number(_)
                        | Expr::RegExp { .. }
                        | Expr::Array(_)
                        | Expr::Object(_)
                        | Expr::Function(_)
                ) {
                    let label = if index == 0 { "pattern" } else { "flags" };
                    return Err(Diagnostic::new(
                        DiagnosticCode::NewUnsupported,
                        format!(
                            "new RegExp {label} must be a string or undefined; pass an explicit string"
                        ),
                        None,
                    ));
                }
            }
        }
        if constructor == "Date" && args.is_empty() {
            return Ok(LashExpr::BuiltinCall {
                name: "__typescript_heap_new".into(),
                args: vec![
                    LashExpr::String("Date".into()),
                    LashExpr::ResultUnwrap(Box::new(journaled_runtime_call("now"))),
                ],
            });
        }
        let mut values = vec![LashExpr::String(constructor.into())];
        values.extend(
            args.iter()
                .map(|arg| match arg {
                    CallArg::Value(value) if matches!(constructor, "Map" | "Set") => {
                        self.lower_iterable_sink(value)
                    }
                    CallArg::Value(value) => self.lower_expr(value),
                    CallArg::Spread(_) => unreachable!(),
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_heap_new".into(),
            args: values,
        })
    }
}

pub(super) fn global_this_member_name<'a>(
    object: &'a Expr,
    property: &'a MemberProperty,
) -> Option<&'a str> {
    match (object, property) {
        (Expr::Ident(root), MemberProperty::Field(field)) if root == "globalThis" => {
            Some(field.as_str())
        }
        _ => None,
    }
}

fn js_subtract(left: LashExpr, right: LashExpr) -> LashExpr {
    LashExpr::JavaScriptBinary {
        left: Box::new(left),
        op: JavaScriptBinaryOp::Subtract,
        right: Box::new(right),
    }
}

pub(super) fn reserved_identifier(name: &str) -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::ReservedIdentifier,
        format!(
            "`{name}` is reserved: identifiers starting with `{GENERATED_BINDING_PREFIX}` name the lowerer's generated bindings"
        ),
        None,
    )
}

pub(super) fn js_unary(op: JavaScriptUnaryOp, expr: LashExpr) -> LashExpr {
    LashExpr::JavaScriptUnary {
        op,
        expr: Box::new(expr),
    }
}

pub(super) fn js_add(left: LashExpr, right: LashExpr) -> LashExpr {
    LashExpr::JavaScriptBinary {
        left: Box::new(left),
        op: JavaScriptBinaryOp::Add,
        right: Box::new(right),
    }
}
