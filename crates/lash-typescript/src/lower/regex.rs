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
                let [regexp] = args else {
                    return Err(regex_arity(method, "one RegExp argument"));
                };
                Ok(regexp_call(
                    method,
                    vec![self.lower_expr(object)?, self.lower_expr(regexp)?],
                ))
            }
            "matchAll" => {
                let [regexp] = args else {
                    return Err(regex_arity(method, "one global RegExp argument"));
                };
                if self.regexp_iterable_sink_depth == 0 {
                    Err(Diagnostic::new(
                        DiagnosticCode::RegexIteratorPosition,
                        "String.matchAll iterators may only be consumed directly by for-of / spread / Array.from; wrap: [...text.matchAll(regexp)]",
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
                    .unwrap_or(LashExpr::Number(u32::MAX as f64));
                Ok(regexp_call(
                    method,
                    vec![self.lower_expr(object)?, self.lower_expr(&args[0])?, limit],
                ))
            }
            "replace" | "replaceAll" => {
                let [search, replacement] = args else {
                    return Err(regex_arity(method, "search and replacement arguments"));
                };
                self.lower_regexp_replace(object, search, replacement, method == "replaceAll")
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
        all: bool,
    ) -> Result<LashExpr, Diagnostic> {
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
        Ok(LashExpr::Block(vec![
            temp_assignment(&input_slot, self.lower_expr(input)?),
            temp_assignment(&search_slot, self.lower_expr(search)?),
            temp_assignment(&replacement_slot, self.lower_expr(replacement)?),
            LashExpr::If {
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
            },
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
    Diagnostic::new(
        DiagnosticCode::MethodUnsupported,
        format!("{method} expects {expected}"),
        None,
    )
}
