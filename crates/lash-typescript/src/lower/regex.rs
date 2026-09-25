use super::*;

impl Lowerer {
    pub(super) fn lower_regexp_method(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<Option<LashExpr>, Diagnostic> {
        if !matches!(
            method,
            "exec" | "test" | "match" | "search" | "matchAll" | "replace" | "replaceAll" | "split"
        ) {
            return Ok(None);
        }
        let lowered = match method {
            "exec" | "test" => {
                let [input] = args else {
                    return Err(regex_arity(method, "one argument"));
                };
                Ok(regexp_call(
                    method,
                    vec![self.lower_expr(object)?, self.lower_expr(input)?],
                ))
            }
            "match" | "search" => {
                // ECMA-262 evaluates every argument in order for its side
                // effects but reads only the first; an absent argument is
                // `undefined` (the empty pattern). The runtime operation
                // pattern-matches the leading values and ignores the rest.
                let mut call_args = Vec::with_capacity(args.len() + 1);
                call_args.push(self.lower_expr(object)?);
                for arg in args {
                    call_args.push(self.lower_expr(arg)?);
                }
                if args.is_empty() {
                    call_args.push(LashExpr::Undefined);
                }
                Ok(regexp_call(method, call_args))
            }
            "matchAll" => {
                let [regexp] = args else {
                    return Err(regex_arity(method, "one global RegExp argument"));
                };
                if self.position.iterable_sink_depth == 0 {
                    Err(Diagnostic::with_repair(
                        DiagnosticCode::RegexIteratorPosition,
                        "String.matchAll iterators may only be consumed directly by for-of / spread / Array.from / new Map|Set / Object.fromEntries",
                        "wrap: [...text.matchAll(regexp)]",
                        None,
                    ))
                } else {
                    Ok(regexp_call(
                        method,
                        vec![self.lower_expr(object)?, self.lower_expr(regexp)?],
                    ))
                }
            }
            "split" => {
                if !(1..=2).contains(&args.len()) {
                    return Err(regex_arity(method, "a separator and optional limit"));
                }
                let limit = args
                    .get(1)
                    .map(|limit| self.lower_expr(limit))
                    .transpose()?
                    .unwrap_or(LashExpr::Undefined);
                Ok(regexp_call(
                    method,
                    vec![self.lower_expr(object)?, self.lower_expr(&args[0])?, limit],
                ))
            }
            "replace" | "replaceAll" => {
                let [search, replacement] = args else {
                    return Err(regex_arity(method, "search and replacement arguments"));
                };
                self.lower_regexp_replace(object, search, replacement, method)
            }
            _ => unreachable!(),
        }?;
        Ok(Some(lowered))
    }

    fn lower_regexp_replace(
        &mut self,
        input: &Expr,
        search: &Expr,
        replacement: &Expr,
        method: &str,
    ) -> Result<LashExpr, Diagnostic> {
        let all = method == "replaceAll";
        let own_method_guard = super::array_callbacks::may_be_plain_object(input);
        let input_slot = self.temporary("replace_input");
        let search_slot = self.temporary("replace_search");
        let replacement_slot = self.temporary("replace_value");
        let plan_slot = self.temporary("replace_plan");
        let entry_slot = self.temporary("replace_entry");
        let variable = |name: &str| LashExpr::Variable(name.into());

        let plan = regexp_call(
            "replacePlan",
            vec![
                variable(&input_slot),
                variable(&search_slot),
                LashExpr::Bool(all),
            ],
        );
        let wrapper = LashExpr::Function(Box::new(FunctionExpr {
            name: None,
            js_name: None,
            receiver: None,
            params: vec![entry_slot.as_str().into()],
            captures: vec![replacement_slot.as_str().into()],
            body: Box::new(LashExpr::Return(Box::new(js_add(
                LashExpr::String("".into()),
                LashExpr::BuiltinCall {
                    name: "__typescript_call_dynamic".into(),
                    args: vec![
                        variable(&replacement_slot),
                        LashExpr::Index {
                            target: Box::new(variable(&entry_slot)),
                            index: Box::new(LashExpr::Number(0.0)),
                        },
                    ],
                },
            )))),
        }));
        let callback_results = LashExpr::Map {
            items: Box::new(variable(&plan_slot)),
            function: Box::new(wrapper),
        };
        let callback_branch = LashExpr::Block(vec![
            temp_assignment(&plan_slot, plan),
            regexp_call(
                "replaceFinish",
                vec![
                    variable(&input_slot),
                    variable(&plan_slot),
                    callback_results,
                ],
            ),
        ]);
        let string_branch = regexp_call(
            "replaceString",
            vec![
                variable(&input_slot),
                variable(&search_slot),
                variable(&replacement_slot),
                LashExpr::Bool(all),
            ],
        );
        let builtin = LashExpr::If {
            condition: Box::new(LashExpr::JavaScriptBinary {
                left: Box::new(js_unary(
                    JavaScriptUnaryOp::TypeOf,
                    variable(&replacement_slot),
                )),
                op: JavaScriptBinaryOp::StrictEqual,
                right: Box::new(LashExpr::String("function".into())),
            }),
            then_block: Box::new(callback_branch),
            else_block: Box::new(string_branch),
        };
        // A plain object has no string `replace`: its own member is called
        // with the arguments as given, and the generated plan never sees it.
        let call = if own_method_guard {
            LashExpr::If {
                condition: Box::new(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String("Lash.OwnMethod".into()),
                        variable(&input_slot),
                        LashExpr::String(method.into()),
                    ],
                }),
                then_block: Box::new(LashExpr::MethodCall {
                    receiver: Box::new(variable(&input_slot)),
                    method: MethodKey::Field(method.into()),
                    args: vec![variable(&search_slot), variable(&replacement_slot)],
                }),
                else_block: Box::new(builtin),
            }
        } else {
            builtin
        };
        Ok(LashExpr::Block(vec![
            temp_assignment(&input_slot, self.lower_expr(input)?),
            temp_assignment(&search_slot, self.lower_expr(search)?),
            temp_assignment(&replacement_slot, self.lower_expr(replacement)?),
            call,
        ]))
    }
}

fn regexp_call(operation: &str, mut args: Vec<LashExpr>) -> LashExpr {
    args.insert(0, LashExpr::String(operation.into()));
    LashExpr::BuiltinCall {
        name: "__typescript_regexp".into(),
        args,
    }
}

fn temp_assignment(name: &str, value: LashExpr) -> LashExpr {
    LashExpr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(value),
    }
}

fn regex_arity(method: &str, expected: &str) -> Diagnostic {
    // Arity, not availability. See `callback_arity`.
    Diagnostic::defect(
        DiagnosticCode::MethodUnsupported,
        format!("{method} expects {expected}"),
        None,
    )
    .with_hint(format!("call `{method}` with {expected}"))
}
