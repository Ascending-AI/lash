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
                self.lower_string_split(object, &args[0], args.get(1))
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

    /// `String.prototype.split` coerces its arguments the ECMA way even when
    /// the separator is an ordinary value: the limit runs `ToUint32` — which
    /// reaches a guest `valueOf` on a record — and a non-RegExp separator is
    /// `ToString`'d, reaching a guest `toString`. Only lowered code can call
    /// a guest method, so the coercions run here in evaluation order (limit
    /// before the separator's `toString`) and the runtime receives already
    /// primitive operands.
    fn lower_string_split(
        &mut self,
        object: &Expr,
        separator: &Expr,
        limit: Option<&Expr>,
    ) -> Result<LashExpr, Diagnostic> {
        // Literals coerce to themselves, so a statically literal separator
        // with a literal (or absent) limit keeps the plain call: no guest
        // method can run.
        let plain_limit = match limit {
            None | Some(Expr::Number(_)) => true,
            Some(Expr::Ident(name, _)) => name == "undefined",
            _ => false,
        };
        if matches!(separator, Expr::String(_) | Expr::RegExp { .. }) && plain_limit {
            let limit = limit
                .map(|limit| self.lower_expr(limit))
                .transpose()?
                .unwrap_or(LashExpr::Number(u32::MAX as f64));
            return Ok(regexp_call(
                "split",
                vec![self.lower_expr(object)?, self.lower_expr(separator)?, limit],
            ));
        }
        let input = self.temporary("split_input");
        let separator_slot = self.temporary("split_separator");
        let limit_slot = self.temporary("split_limit");
        let coerced_limit = self.temporary("split_coerced_limit");
        let coerced_separator = self.temporary("split_coerced_separator");
        let variable = |name: &str| LashExpr::Variable(name.into());

        let regexp_branch = regexp_call(
            "split",
            vec![
                variable(&input),
                variable(&separator_slot),
                variable(&coerced_limit),
            ],
        );
        let string_branch = LashExpr::Block(vec![
            temp_assignment(
                &coerced_separator,
                self.lower_to_primitive(variable(&separator_slot), "toString", "valueOf"),
            ),
            regexp_call(
                "split",
                vec![
                    variable(&input),
                    variable(&coerced_separator),
                    variable(&coerced_limit),
                ],
            ),
        ]);
        Ok(LashExpr::Block(vec![
            temp_assignment(&input, self.lower_expr(object)?),
            temp_assignment(&separator_slot, self.lower_expr(separator)?),
            temp_assignment(
                &limit_slot,
                limit
                    .map(|limit| self.lower_expr(limit))
                    .transpose()?
                    .unwrap_or(LashExpr::Undefined),
            ),
            // `limit === undefined` keeps the ECMA default of 2^32 - 1, which
            // the runtime reads off the `Undefined` sentinel; coercing it to
            // ToUint32 would say zero.
            temp_assignment(
                &coerced_limit,
                LashExpr::If {
                    condition: Box::new(LashExpr::JavaScriptBinary {
                        left: Box::new(variable(&limit_slot)),
                        op: JavaScriptBinaryOp::StrictEqual,
                        right: Box::new(LashExpr::Undefined),
                    }),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(self.lower_to_primitive(
                        variable(&limit_slot),
                        "valueOf",
                        "toString",
                    )),
                },
            ),
            LashExpr::If {
                condition: Box::new(LashExpr::BuiltinCall {
                    name: "__typescript_heap_instanceof".into(),
                    args: vec![variable(&separator_slot), LashExpr::String("RegExp".into())],
                }),
                then_block: Box::new(regexp_branch),
                else_block: Box::new(string_branch),
            },
        ]))
    }

    /// OrdinaryToPrimitive on an already-evaluated operand: a record carrying
    /// a callable `first` method runs it as guest code, then `second` when
    /// the first is absent or still returned an object. Neither being
    /// callable leaves the value to the runtime's built-in coercion, which
    /// answers what the inherited `Object.prototype` methods would.
    fn lower_to_primitive(&mut self, source: LashExpr, first: &str, second: &str) -> LashExpr {
        let input = self.temporary("coerce_input");
        let method_slot = self.temporary("coerce_method");
        let primitive = self.temporary("coerce_primitive");
        let result = self.temporary("coerce_result");
        let variable = |name: &str| LashExpr::Variable(name.into());
        let is_scalar = |value: LashExpr| LashExpr::JavaScriptBinary {
            left: Box::new(stdlib_call("__jsonContainerKind", vec![value])),
            op: JavaScriptBinaryOp::StrictEqual,
            right: Box::new(LashExpr::String("scalar".into())),
        };
        let method_of = |name: &str| LashExpr::Index {
            target: Box::new(variable(&input)),
            index: Box::new(LashExpr::String(name.into())),
        };
        // OrdinaryToPrimitive invokes the method with the object as `this`.
        let call = |function: LashExpr| LashExpr::BuiltinCall {
            name: "__typescript_call_this".into(),
            args: vec![function, variable(&input), LashExpr::List(Vec::new())],
        };
        // `m = in[name]`; when callable, `p = m()` wins if it is a primitive.
        // Anything else falls to `miss`, and a record with no callable
        // methods at all keeps `input`: the runtime's built-in coercion then
        // answers what ECMA's inherited `Object.prototype` methods would.
        let try_method = |name: &str, miss: LashExpr| {
            LashExpr::Block(vec![
                temp_assignment(&method_slot, method_of(name)),
                LashExpr::If {
                    condition: Box::new(stdlib_call(
                        "Lash.IsCallable",
                        vec![variable(&method_slot)],
                    )),
                    then_block: Box::new(LashExpr::Block(vec![
                        temp_assignment(&primitive, call(variable(&method_slot))),
                        LashExpr::If {
                            condition: Box::new(is_scalar(variable(&primitive))),
                            then_block: Box::new(temp_assignment(&result, variable(&primitive))),
                            else_block: Box::new(miss.clone()),
                        },
                    ])),
                    else_block: Box::new(miss),
                },
            ])
        };
        LashExpr::Block(vec![
            temp_assignment(&input, source),
            temp_assignment(&result, variable(&input)),
            LashExpr::If {
                condition: Box::new(LashExpr::JavaScriptBinary {
                    left: Box::new(stdlib_call("__jsonContainerKind", vec![variable(&input)])),
                    op: JavaScriptBinaryOp::StrictEqual,
                    right: Box::new(LashExpr::String("record".into())),
                }),
                then_block: Box::new(try_method(first, try_method(second, LashExpr::Undefined))),
                else_block: Box::new(LashExpr::Undefined),
            },
            variable(&result),
        ])
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

fn stdlib_call(method: &str, mut args: Vec<LashExpr>) -> LashExpr {
    args.insert(0, LashExpr::String(method.into()));
    LashExpr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args,
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
