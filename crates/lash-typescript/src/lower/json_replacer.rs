//! In-VM JSON function-replacer traversal.
//!
//! Function values cannot cross the stdlib host boundary. The traversal is
//! therefore guest AST running under the existing effect-rejecting callback
//! frame; the final JSON byte rendering remains the single heap-aware runtime
//! implementation used by ordinary stringify calls.

use lashlang::{AssignPathStep, AssignTarget, Expr as LashExpr, FunctionExpr, JavaScriptBinaryOp};

use super::{GENERATED_BINDING_PREFIX, Lowerer};
use crate::adapter::Expr;
use crate::{Diagnostic, DiagnosticCode};

impl Lowerer {
    pub(super) fn lower_json_stringify(
        &mut self,
        value: &Expr,
        function_replacer: Option<&Expr>,
        property_replacer: Option<&Expr>,
        space: Option<&Expr>,
    ) -> Result<LashExpr, Diagnostic> {
        let has_function_replacer = function_replacer.is_some();
        let input = self.temporary("json_input");
        let replacer_name = self.temporary("json_replacer");
        let active = self.temporary("json_active");
        let transform = self.temporary("json_transform");
        let worker = self.temporary("json_worker");
        let holder = self.temporary("json_holder");
        let key = self.temporary("json_key");
        let current = self.temporary("json_current");
        let output = self.temporary("json_output");
        let index = self.temporary("json_index");
        let keys = self.temporary("json_keys");
        let property = self.temporary("json_property");
        let transformed = self.temporary("json_value");
        let transformed_root = self.temporary("json_transformed_root");
        let cycle = self.temporary("json_cycle");
        let kind = self.temporary("json_kind");
        let property_replacer_name = self.temporary("json_property_replacer");
        let space_name = self.temporary("json_space");

        let current_value = || variable(&current);
        let cycle_guard = || LashExpr::If {
            condition: Box::new(stdlib(
                "__jsonActiveContains",
                vec![variable(&active), current_value()],
            )),
            then_block: Box::new(LashExpr::Block(vec![
                LashExpr::Assign {
                    target: AssignTarget {
                        root: cycle.as_str().into(),
                        steps: vec![AssignPathStep::Index(LashExpr::Number(0.0))],
                    },
                    expr: Box::new(LashExpr::Bool(true)),
                },
                LashExpr::Return(Box::new(LashExpr::Undefined)),
            ])),
            else_block: Box::new(LashExpr::Undefined),
        };
        let push_active = || append(&active, current_value());
        let pop_active = || {
            stdlib(
                "splice",
                vec![
                    variable(&active),
                    sub(field(&active, "length"), LashExpr::Number(1.0)),
                    LashExpr::Number(1.0),
                ],
            )
        };
        let recurse = |holder: LashExpr, key: LashExpr| LashExpr::Call {
            function: Box::new(variable(&transform)),
            args: vec![holder, key],
        };

        let array_branch = LashExpr::Block(vec![
            cycle_guard(),
            push_active(),
            assign(&output, LashExpr::List(Vec::new())),
            assign(&index, LashExpr::Number(0.0)),
            LashExpr::While {
                condition: Box::new(binary(
                    variable(&index),
                    JavaScriptBinaryOp::Less,
                    field(&current, "length"),
                )),
                body: Box::new(LashExpr::Block(vec![
                    assign(
                        &transformed,
                        recurse(
                            current_value(),
                            add(LashExpr::String("".into()), variable(&index)),
                        ),
                    ),
                    append(
                        &output,
                        LashExpr::If {
                            condition: Box::new(binary(
                                variable(&transformed),
                                JavaScriptBinaryOp::StrictEqual,
                                LashExpr::Undefined,
                            )),
                            then_block: Box::new(LashExpr::Null),
                            else_block: Box::new(variable(&transformed)),
                        },
                    ),
                    assign(&index, add(variable(&index), LashExpr::Number(1.0))),
                ])),
            },
            pop_active(),
            LashExpr::Return(Box::new(variable(&output))),
        ]);
        let object_branch = LashExpr::Block(vec![
            cycle_guard(),
            push_active(),
            assign(&output, LashExpr::Record(Vec::new())),
            assign(&keys, stdlib("Object.keys", vec![current_value()])),
            LashExpr::For {
                binding: property.as_str().into(),
                iterable: Box::new(variable(&keys)),
                body: Box::new(LashExpr::Block(vec![
                    assign(&transformed, recurse(current_value(), variable(&property))),
                    LashExpr::If {
                        condition: Box::new(binary(
                            variable(&transformed),
                            JavaScriptBinaryOp::StrictNotEqual,
                            LashExpr::Undefined,
                        )),
                        then_block: Box::new(LashExpr::Assign {
                            target: AssignTarget {
                                root: output.as_str().into(),
                                steps: vec![AssignPathStep::Index(variable(&property))],
                            },
                            expr: Box::new(variable(&transformed)),
                        }),
                        else_block: Box::new(LashExpr::Undefined),
                    },
                ])),
            },
            pop_active(),
            LashExpr::Return(Box::new(variable(&output))),
        ]);

        let transform_body = LashExpr::Block(vec![
            assign(
                &current,
                LashExpr::Index {
                    target: Box::new(variable(&holder)),
                    index: Box::new(variable(&key)),
                },
            ),
            // User-authored own `toJSON` hooks run before the replacer. Exotic
            // prototype hooks remain in the heap-aware renderer.
            LashExpr::If {
                condition: Box::new(stdlib("__jsonHasOwnToJSON", vec![current_value()])),
                then_block: Box::new(assign(
                    &current,
                    LashExpr::Call {
                        function: Box::new(LashExpr::Field {
                            target: Box::new(current_value()),
                            field: "toJSON".into(),
                        }),
                        args: vec![variable(&key)],
                    },
                )),
                else_block: Box::new(LashExpr::Undefined),
            },
            assign(
                &current,
                LashExpr::Call {
                    function: Box::new(variable(&replacer_name)),
                    args: vec![variable(&key), current_value()],
                },
            ),
            assign(&kind, stdlib("__jsonContainerKind", vec![current_value()])),
            LashExpr::If {
                condition: Box::new(binary(
                    variable(&kind),
                    JavaScriptBinaryOp::StrictEqual,
                    LashExpr::String("array".into()),
                )),
                then_block: Box::new(array_branch),
                else_block: Box::new(LashExpr::Undefined),
            },
            LashExpr::If {
                condition: Box::new(binary(
                    variable(&kind),
                    JavaScriptBinaryOp::StrictEqual,
                    LashExpr::String("record".into()),
                )),
                then_block: Box::new(object_branch),
                else_block: Box::new(LashExpr::Undefined),
            },
            LashExpr::Return(Box::new(current_value())),
        ]);
        let transformer = LashExpr::Function(Box::new(FunctionExpr {
            name: Some(transform.as_str().into()),
            params: vec![holder.as_str().into(), key.as_str().into()],
            captures: vec![
                replacer_name.as_str().into(),
                active.as_str().into(),
                cycle.as_str().into(),
            ],
            body: Box::new(transform_body),
        }));
        let root = LashExpr::Record(vec![("".into(), variable(&input))]);
        let worker_body = recurse(root, LashExpr::String("".into()));

        let replacer_value = if let Some(replacer) = function_replacer {
            self.lower_expr(replacer)?
        } else {
            let identity_key = self.temporary("identity_key");
            let identity_value = self.temporary("identity_value");
            LashExpr::Function(Box::new(FunctionExpr {
                name: None,
                params: vec![identity_key.into(), identity_value.as_str().into()],
                captures: Vec::new(),
                body: Box::new(variable(&identity_value)),
            }))
        };
        let property_replacer_value = property_replacer
            .map(|value| self.lower_expr(value))
            .transpose()?
            .unwrap_or(LashExpr::Null);
        let space_value = space
            .map(|value| self.lower_expr(value))
            .transpose()?
            .unwrap_or(LashExpr::Undefined);
        let mut prefix = vec![
            assign(&input, self.lower_expr(value)?),
            assign(&replacer_name, replacer_value),
            assign(&property_replacer_name, property_replacer_value),
            assign(&space_name, space_value),
        ];
        let mut expressions = vec![
            // Reject a durable cycle before the callback driver captures the
            // graph. The v1 heap wire cannot encode cycles, so allowing a
            // replacer to erase one would make the callback frame itself
            // unsuspendable.
            LashExpr::If {
                condition: Box::new(stdlib("__jsonHasCycle", vec![variable(&input)])),
                then_block: Box::new(LashExpr::Throw(Box::new(LashExpr::BuiltinCall {
                    name: "__typescript_heap_new".into(),
                    args: vec![
                        LashExpr::String("TypeError".into()),
                        LashExpr::String(
                            "Converting circular structure to JSON\n    --> starting at object with constructor 'Object'\n    --- property 'self' closes the circle".into(),
                        ),
                    ],
                }))),
                else_block: Box::new(LashExpr::Undefined),
            },
            assign(&active, LashExpr::List(Vec::new())),
            assign(&cycle, LashExpr::List(vec![LashExpr::Bool(false)])),
            assign(&transform, transformer),
            assign(
                &worker,
                LashExpr::Function(Box::new(FunctionExpr {
                    name: None,
                    params: vec![format!("{GENERATED_BINDING_PREFIX}ignored").into()],
                    captures: vec![input.as_str().into(), transform.as_str().into()],
                    body: Box::new(worker_body),
                })),
            ),
        ];
        expressions.push(assign(
            &transformed_root,
            stdlib(
                "__singleCallbackResult",
                vec![LashExpr::Map {
                    items: Box::new(LashExpr::List(vec![LashExpr::Undefined])),
                    function: Box::new(variable(&worker)),
                }],
            ),
        ));
        expressions.push(LashExpr::If {
            condition: Box::new(LashExpr::Index {
                target: Box::new(variable(&cycle)),
                index: Box::new(LashExpr::Number(0.0)),
            }),
            then_block: Box::new(LashExpr::Throw(Box::new(LashExpr::BuiltinCall {
                name: "__typescript_heap_new".into(),
                args: vec![
                    LashExpr::String("TypeError".into()),
                    LashExpr::String("Converting circular structure to JSON".into()),
                ],
            }))),
            else_block: Box::new(LashExpr::Undefined),
        });
        let stringify_args = vec![
            variable(&transformed_root),
            variable(&property_replacer_name),
            variable(&space_name),
        ];
        expressions.push(stdlib("JSON.stringify", stringify_args));
        let guest_traversal = LashExpr::Block(expressions);
        if has_function_replacer {
            prefix.push(guest_traversal);
        } else {
            prefix.push(LashExpr::If {
                condition: Box::new(binary(
                    stdlib("__jsonContainerKind", vec![variable(&input)]),
                    JavaScriptBinaryOp::StrictEqual,
                    LashExpr::String("opaque".into()),
                )),
                then_block: Box::new(stdlib(
                    "JSON.stringify",
                    vec![
                        variable(&input),
                        variable(&property_replacer_name),
                        variable(&space_name),
                    ],
                )),
                else_block: Box::new(guest_traversal),
            });
        }
        Ok(LashExpr::Block(prefix))
    }
}

fn variable(name: &str) -> LashExpr {
    LashExpr::Variable(name.into())
}

fn field(name: &str, field: &str) -> LashExpr {
    LashExpr::Field {
        target: Box::new(variable(name)),
        field: field.into(),
    }
}

fn assign(name: &str, value: LashExpr) -> LashExpr {
    LashExpr::Assign {
        target: AssignTarget::variable(name.into()),
        expr: Box::new(value),
    }
}

fn append(name: &str, value: LashExpr) -> LashExpr {
    LashExpr::Assign {
        target: AssignTarget {
            root: name.into(),
            steps: vec![AssignPathStep::Index(field(name, "length"))],
        },
        expr: Box::new(value),
    }
}

fn binary(left: LashExpr, op: JavaScriptBinaryOp, right: LashExpr) -> LashExpr {
    LashExpr::JavaScriptBinary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn add(left: LashExpr, right: LashExpr) -> LashExpr {
    binary(left, JavaScriptBinaryOp::Add, right)
}

fn sub(left: LashExpr, right: LashExpr) -> LashExpr {
    binary(left, JavaScriptBinaryOp::Subtract, right)
}

fn stdlib(method: &str, mut args: Vec<LashExpr>) -> LashExpr {
    args.insert(0, LashExpr::String(method.into()));
    LashExpr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args,
    }
}

pub(super) fn reject_json_parse_reviver() -> Diagnostic {
    Diagnostic::new(
        DiagnosticCode::MethodUnsupported,
        "Unsupported: JSON.parse reviver callbacks. Parse first, then walk the returned value explicitly in deterministic TypeScript.",
        None,
    )
}
