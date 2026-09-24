//! Array callbacks lowered through the existing durable, effect-rejecting VM
//! callback frame. A one-element outer `Map` provides that frame; the generated
//! guest function implements the ECMA sequential loop without adding a new VM
//! continuation shape.

use lashlang::{
    AssignPathStep, AssignTarget, Expr as LashExpr, FunctionExpr, JavaScriptBinaryOp, MethodKey,
    StructuralRole,
};

use super::{GENERATED_BINDING_PREFIX, Lowerer};
use crate::adapter::Expr;
use crate::{Diagnostic, DiagnosticCode};

impl Lowerer {
    pub(super) fn lower_string_replace_callback(
        &mut self,
        receiver_expr: &Expr,
        needle_expr: &Expr,
        callback_expr: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        let receiver = self.temporary("replace_receiver");
        let needle = self.temporary("replace_needle");
        let callback = self.temporary("replace_callback");
        let worker = self.temporary("replace_worker");
        let index = self.temporary("replace_index");
        let matched = self.temporary("replace_match");
        let body = LashExpr::Block(vec![
            assign(
                &index,
                stdlib("indexOf", vec![variable(&receiver), variable(&needle)]),
            ),
            LashExpr::If {
                condition: Box::new(binary(
                    variable(&index),
                    JavaScriptBinaryOp::Less,
                    LashExpr::Number(0.0),
                )),
                then_block: Box::new(LashExpr::Return(Box::new(variable(&receiver)))),
                else_block: Box::new(LashExpr::Undefined),
            },
            assign(
                &matched,
                stdlib(
                    "slice",
                    vec![
                        variable(&receiver),
                        variable(&index),
                        add(variable(&index), field(&needle, "length")),
                    ],
                ),
            ),
            add(
                add(
                    stdlib(
                        "slice",
                        vec![variable(&receiver), LashExpr::Number(0.0), variable(&index)],
                    ),
                    LashExpr::Call {
                        function: Box::new(variable(&callback)),
                        args: vec![variable(&matched), variable(&index), variable(&receiver)],
                    },
                ),
                stdlib(
                    "slice",
                    vec![
                        variable(&receiver),
                        add(variable(&index), field(&needle, "length")),
                    ],
                ),
            ),
        ]);
        let guard = may_be_plain_object(receiver_expr);
        let search = self.temporary("replace_search");
        let mut expressions = vec![
            assign(&receiver, self.lower_expr(receiver_expr)?),
            assign(&search, self.lower_expr(needle_expr)?),
            assign(&callback, self.lower_expr(callback_expr)?),
        ];
        // The built-in converts the pattern after both arguments are
        // evaluated.
        let builtin = LashExpr::Block(vec![
            assign(
                &needle,
                add(LashExpr::String(String::new().into()), variable(&search)),
            ),
            assign(
                &worker,
                LashExpr::Function(Box::new(FunctionExpr {
                    name: None,
                    js_name: None,
                    receiver: None,
                    params: vec![format!("{GENERATED_BINDING_PREFIX}ignored").into()],
                    captures: vec![
                        receiver.as_str().into(),
                        needle.as_str().into(),
                        callback.as_str().into(),
                    ],
                    body: Box::new(body),
                })),
            ),
            stdlib(
                "__singleCallbackResult",
                vec![LashExpr::Map {
                    items: Box::new(LashExpr::List(vec![LashExpr::Undefined])),
                    function: Box::new(variable(&worker)),
                }],
            ),
        ]);
        // A plain object has no string `replace`: its own member is called
        // with the arguments as given, and the worker's generated string
        // calls never see it.
        expressions.push(if guard {
            LashExpr::If {
                condition: Box::new(stdlib(
                    "Lash.OwnMethod",
                    vec![variable(&receiver), LashExpr::String("replace".into())],
                )),
                then_block: Box::new(LashExpr::MethodCall {
                    receiver: Box::new(variable(&receiver)),
                    method: MethodKey::Field("replace".into()),
                    args: vec![variable(&search), variable(&callback)],
                }),
                else_block: Box::new(builtin),
            }
        } else {
            builtin
        });
        Ok(LashExpr::Block(expressions))
    }

    pub(super) fn lower_group_by(
        &mut self,
        owner: &str,
        source: &Expr,
        callback: &Expr,
    ) -> Result<LashExpr, Diagnostic> {
        let source_name = self.temporary("group_source");
        let callback_name = self.temporary("group_callback");
        let output = self.temporary("group_output");
        let worker = self.temporary("group_worker");
        let index = self.temporary("group_index");
        let key = self.temporary("group_key");
        let group = self.temporary("group_values");
        // groupBy takes an iterable only — never an array-like — and checks
        // the callback is callable, both as guest TypeErrors.
        let source_value = stdlib(
            "Lash.GroupBySource",
            vec![self.lower_iterable_sink(source)?],
        );
        let output_value = if owner == "Map" {
            LashExpr::BuiltinCall {
                name: "__typescript_heap_new".into(),
                args: vec![LashExpr::String("Map".into())],
            }
        } else {
            LashExpr::Record(Vec::new())
        };
        let item = LashExpr::Index {
            target: Box::new(variable(&source_name)),
            index: Box::new(variable(&index)),
        };
        let callback_call = LashExpr::Call {
            function: Box::new(variable(&callback_name)),
            args: vec![item.clone(), variable(&index)],
        };
        let group_step = if owner == "Map" {
            LashExpr::Block(vec![
                assign(&key, callback_call),
                LashExpr::If {
                    condition: Box::new(stdlib("has", vec![variable(&output), variable(&key)])),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(stdlib(
                        "set",
                        vec![
                            variable(&output),
                            variable(&key),
                            LashExpr::List(Vec::new()),
                        ],
                    )),
                },
                assign(
                    &group,
                    stdlib("get", vec![variable(&output), variable(&key)]),
                ),
                append(&group, item),
            ])
        } else {
            LashExpr::Block(vec![
                // GroupBy's ToPropertyKey: the string hint, and ECMA's own
                // answer for an object without hooks.
                assign(&key, stdlib("Lash.ToPropertyKey", vec![callback_call])),
                LashExpr::If {
                    condition: Box::new(stdlib(
                        "Object.hasOwn",
                        vec![variable(&output), variable(&key)],
                    )),
                    then_block: Box::new(LashExpr::Undefined),
                    else_block: Box::new(LashExpr::Assign {
                        target: AssignTarget {
                            root: output.as_str().into(),
                            steps: vec![AssignPathStep::Index(variable(&key))],
                        },
                        expr: Box::new(LashExpr::List(Vec::new())),
                    }),
                },
                assign(
                    &group,
                    LashExpr::Index {
                        target: Box::new(variable(&output)),
                        index: Box::new(variable(&key)),
                    },
                ),
                append(&group, item),
            ])
        };
        let body = LashExpr::Block(vec![
            assign(&index, LashExpr::Number(0.0)),
            LashExpr::While {
                condition: Box::new(binary(
                    variable(&index),
                    JavaScriptBinaryOp::Less,
                    field(&source_name, "length"),
                )),
                body: Box::new(LashExpr::Block(vec![
                    group_step,
                    assign(&index, add(variable(&index), LashExpr::Number(1.0))),
                ])),
            },
            LashExpr::Undefined,
        ]);
        Ok(LashExpr::Block(vec![
            assign(&source_name, source_value),
            assign(&callback_name, self.lower_expr(callback)?),
            // GroupBy refuses a non-callable callback before it calls one.
            assign(&callback_name, require_callable(variable(&callback_name))),
            assign(&output, output_value),
            assign(
                &worker,
                LashExpr::Function(Box::new(FunctionExpr {
                    name: None,
                    js_name: None,
                    receiver: None,
                    params: vec![format!("{GENERATED_BINDING_PREFIX}ignored").into()],
                    captures: vec![
                        source_name.as_str().into(),
                        callback_name.as_str().into(),
                        output.as_str().into(),
                    ],
                    body: Box::new(body),
                })),
            ),
            LashExpr::Map {
                items: Box::new(LashExpr::List(vec![LashExpr::Undefined])),
                function: Box::new(variable(&worker)),
            },
            variable(&output),
        ]))
    }

    pub(super) fn lower_array_callback_method(
        &mut self,
        method: &str,
        object: &Expr,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        let guard = may_be_plain_object(object);
        let receiver = self.lower_expr(object)?;
        self.lower_array_callback_with_receiver(method, receiver, args, guard)
    }

    pub(super) fn lower_array_from_mapping(
        &mut self,
        receiver: LashExpr,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        self.lower_array_callback_with_receiver("arrayFromMap", receiver, args, false)
    }

    fn lower_array_callback_with_receiver(
        &mut self,
        method: &str,
        receiver_value: LashExpr,
        args: &[Expr],
        own_method_guard: bool,
    ) -> Result<LashExpr, Diagnostic> {
        let Some(callback) = args.first() else {
            return Err(callback_arity(method, "a callback"));
        };
        let initial = match method {
            "reduce" | "reduceRight" => args.get(1),
            _ => None,
        };

        let receiver = self.temporary("callback_receiver");
        let callback_name = self.temporary("callback_function");
        let worker = self.temporary("callback_worker");
        let mut setup = vec![
            assign(&receiver, receiver_value),
            assign(&callback_name, self.lower_expr(callback)?),
        ];
        // The call's arguments as evaluated, in order: a plain object's own
        // method receives exactly these.
        let mut arguments = vec![callback_name.clone()];
        let initial_name = initial.map(|_| self.temporary("callback_initial"));
        if let (Some(value), Some(name)) = (initial, initial_name.as_deref()) {
            // Arguments are evaluated left-to-right: for reduce the initial
            // value precedes any excess arguments.
            setup.push(assign(name, self.lower_expr(value)?));
            arguments.push(name.to_string());
        }
        // The predicate family and `Array.from`'s mapper take a `thisArg`: the
        // callback's receiver on every call (ECMA-262 `Call(callbackfn,
        // thisArg, ...)`); without one the receiver is `undefined`.
        let this_arg = if takes_this_arg(method)
            && let Some(value) = args.get(1)
        {
            let name = self.temporary("callback_this");
            setup.push(assign(&name, self.lower_expr(value)?));
            arguments.push(name.clone());
            Some(name)
        } else {
            None
        };
        // ECMAScript evaluates excess arguments before the call and then
        // ignores them.
        let consumed = if matches!(method, "reduce" | "reduceRight") && initial.is_some()
            || this_arg.is_some()
        {
            2
        } else {
            1
        };
        for argument in args.iter().skip(consumed) {
            let ignored = self.temporary("callback_ignored_argument");
            setup.push(assign(&ignored, self.lower_expr(argument)?));
            arguments.push(ignored);
        }
        // A plain object has no array methods: `o.map(f)` calls `o`'s own
        // `map`. Which one runs is decided at run time, after the arguments,
        // and the built-in's generated code never sees a plain object.
        let own_method = own_method_guard.then(|| {
            stdlib(
                "Lash.OwnMethod",
                vec![variable(&receiver), LashExpr::String(method.into())],
            )
        });
        let own_call = || LashExpr::MethodCall {
            receiver: Box::new(variable(&receiver)),
            method: MethodKey::Field(method.into()),
            args: arguments.iter().map(|name| variable(name)).collect(),
        };
        if method == "toSorted" {
            let copy = stdlib("slice", vec![variable(&receiver)]);
            setup.push(assign(
                &receiver,
                match &own_method {
                    Some(own) => LashExpr::If {
                        condition: Box::new(own.clone()),
                        then_block: Box::new(variable(&receiver)),
                        else_block: Box::new(copy),
                    },
                    None => copy,
                },
            ));
        }
        let initial = initial_name.as_deref().map(variable);
        let body = if matches!(method, "sort" | "toSorted") {
            callback_body(
                method,
                &receiver,
                &callback_name,
                this_arg.as_deref(),
                initial,
                self,
            )?
        } else {
            // Every callback-taking method checks IsCallable after its
            // arguments are evaluated and before it visits an element, so a
            // non-callable callback throws a TypeError even on an empty
            // receiver. `Array.from` reads an `undefined` mapper as no mapping.
            // The check runs first in the driver, so the role's setup keeps
            // its receiver-and-callback shape.
            let callee = self.temporary("callback_callee");
            let check = if method == "arrayFromMap" {
                let identity = self.temporary("callback_identity");
                LashExpr::If {
                    condition: Box::new(binary(
                        variable(&callback_name),
                        JavaScriptBinaryOp::StrictEqual,
                        LashExpr::Undefined,
                    )),
                    then_block: Box::new(LashExpr::Function(Box::new(FunctionExpr {
                        name: None,
                        js_name: None,
                        receiver: None,
                        params: vec![identity.as_str().into()],
                        captures: Vec::new(),
                        body: Box::new(variable(&identity)),
                    }))),
                    else_block: Box::new(require_callable(variable(&callback_name))),
                }
            } else {
                require_callable(variable(&callback_name))
            };
            LashExpr::Block(vec![
                assign(&callee, check),
                callback_body(
                    method,
                    &receiver,
                    &callee,
                    this_arg.as_deref(),
                    initial,
                    self,
                )?,
            ])
        };
        let mut captures = vec![receiver.as_str().into(), callback_name.as_str().into()];
        if let Some(name) = initial_name {
            captures.push(name.into());
        }
        if let Some(name) = &this_arg {
            captures.push(name.as_str().into());
        }
        setup.push(assign(
            &worker,
            LashExpr::Function(Box::new(FunctionExpr {
                name: None,
                js_name: None,
                receiver: None,
                params: vec![format!("{GENERATED_BINDING_PREFIX}ignored").into()],
                captures,
                body: Box::new(body),
            })),
        ));
        let driven = LashExpr::Map {
            items: Box::new(LashExpr::List(vec![LashExpr::Undefined])),
            function: Box::new(variable(&worker)),
        };
        let builtin = if matches!(method, "sort" | "toSorted") {
            LashExpr::Block(vec![driven, variable(&receiver)])
        } else if method == "forEach" {
            // A receiver not written as a collection (an alias, a field, a
            // call's result) is an array or a collection only at run time. A
            // `Map`, `Set` or `URLSearchParams` iterates by its own `forEach`,
            // with live cursors; walking it as an array visited nothing.
            LashExpr::If {
                condition: Box::new(stdlib("Array.isArray", vec![variable(&receiver)])),
                then_block: Box::new(stdlib("__singleCallbackResult", vec![driven])),
                else_block: Box::new(stdlib(
                    "forEach",
                    vec![
                        variable(&receiver),
                        match &this_arg {
                            Some(this_arg) => bound_callback(&callback_name, this_arg, self),
                            None => variable(&callback_name),
                        },
                    ],
                )),
            }
        } else {
            stdlib("__singleCallbackResult", vec![driven])
        };
        setup.push(match own_method {
            Some(own) => LashExpr::If {
                condition: Box::new(own),
                then_block: Box::new(own_call()),
                else_block: Box::new(builtin),
            },
            None => builtin,
        });
        Ok(LashExpr::Role {
            role: StructuralRole::CollectionTransform {
                operation: method.into(),
            },
            expr: Box::new(LashExpr::Block(setup)),
        })
    }
}

/// Whether a member call's receiver, as written, may be a plain object at run
/// time: anything but a primitive or array literal.
pub(super) fn may_be_plain_object(object: &Expr) -> bool {
    !matches!(
        object,
        Expr::String(_) | Expr::Number(_) | Expr::Bool(_) | Expr::Array(_)
    )
}

/// Whether `method`'s callback takes a `thisArg` after it.
fn takes_this_arg(method: &str) -> bool {
    matches!(
        method,
        "map"
            | "arrayFromMap"
            | "filter"
            | "flatMap"
            | "forEach"
            | "some"
            | "every"
            | "find"
            | "findIndex"
            | "findLast"
            | "findLastIndex"
    )
}

/// `(value, key, collection) => callback.call(thisArg, value, key,
/// collection)`: a collection's own `forEach` calls it, so the callback runs
/// with the `thisArg` receiver.
fn bound_callback(callback: &str, this_arg: &str, lowerer: &mut Lowerer) -> LashExpr {
    let params = ["value", "key", "collection"].map(|label| lowerer.temporary(label));
    LashExpr::Function(Box::new(FunctionExpr {
        name: None,
        js_name: None,
        receiver: None,
        params: params.iter().map(|param| param.as_str().into()).collect(),
        captures: vec![callback.into(), this_arg.into()],
        body: Box::new(LashExpr::ThisCall {
            this: Box::new(variable(this_arg)),
            function: Box::new(variable(callback)),
            args: params.iter().map(|param| variable(param)).collect(),
        }),
    }))
}

fn callback_body(
    method: &str,
    receiver: &str,
    callback: &str,
    this_arg: Option<&str>,
    initial: Option<LashExpr>,
    lowerer: &mut Lowerer,
) -> Result<LashExpr, Diagnostic> {
    if matches!(method, "sort" | "toSorted") {
        return Ok(sort_body(receiver, callback, lowerer));
    }

    let index = lowerer.temporary("callback_index");
    let length = lowerer.temporary("callback_length");
    let output = lowerer.temporary("callback_output");
    let accumulator = lowerer.temporary("callback_accumulator");
    let initialized = lowerer.temporary("callback_initialized");
    let reverse = matches!(method, "reduceRight" | "findLast" | "findLastIndex");
    let start = if reverse {
        subtract(field(receiver, "length"), LashExpr::Number(1.0))
    } else {
        LashExpr::Number(0.0)
    };
    let condition = if reverse {
        binary(
            variable(&index),
            JavaScriptBinaryOp::GreaterEqual,
            LashExpr::Number(0.0),
        )
    } else {
        binary(
            variable(&index),
            JavaScriptBinaryOp::Less,
            variable(&length),
        )
    };
    let item = LashExpr::Index {
        target: Box::new(variable(receiver)),
        index: Box::new(variable(&index)),
    };
    let call = |arguments: Vec<LashExpr>| match this_arg {
        Some(this_arg) => LashExpr::ThisCall {
            this: Box::new(variable(this_arg)),
            function: Box::new(variable(callback)),
            args: arguments,
        },
        None => LashExpr::Call {
            function: Box::new(variable(callback)),
            args: arguments,
        },
    };
    let predicate = || {
        let mut arguments = vec![item.clone(), variable(&index)];
        if method != "arrayFromMap" {
            arguments.push(variable(receiver));
        }
        call(arguments)
    };

    let mut expressions = vec![
        assign(&length, field(receiver, "length")),
        assign(&index, start),
    ];
    match method {
        "map" | "arrayFromMap" | "filter" | "flatMap" => {
            expressions.push(assign(&output, LashExpr::List(Vec::new())))
        }
        "reduce" | "reduceRight" => {
            expressions.push(assign(
                &accumulator,
                initial.clone().unwrap_or(LashExpr::Undefined),
            ));
            expressions.push(assign(&initialized, LashExpr::Bool(initial.is_some())));
        }
        _ => {}
    }

    let step = assign(
        &index,
        if reverse {
            subtract(variable(&index), LashExpr::Number(1.0))
        } else {
            add(variable(&index), LashExpr::Number(1.0))
        },
    );
    let operation = match method {
        "map" | "arrayFromMap" => append(&output, predicate()),
        "filter" => LashExpr::If {
            condition: Box::new(predicate()),
            then_block: Box::new(append(&output, item.clone())),
            else_block: Box::new(LashExpr::Undefined),
        },
        "flatMap" => LashExpr::Assign {
            target: AssignTarget::variable(output.as_str().into()),
            expr: Box::new(stdlib(
                "__appendFlatMap",
                vec![variable(&output), predicate()],
            )),
        },
        "forEach" => predicate(),
        "some" => LashExpr::If {
            condition: Box::new(predicate()),
            then_block: Box::new(LashExpr::Return(Box::new(LashExpr::Bool(true)))),
            else_block: Box::new(LashExpr::Undefined),
        },
        "every" => LashExpr::If {
            condition: Box::new(predicate()),
            then_block: Box::new(LashExpr::Undefined),
            else_block: Box::new(LashExpr::Return(Box::new(LashExpr::Bool(false)))),
        },
        "find" | "findIndex" | "findLast" | "findLastIndex" => LashExpr::If {
            condition: Box::new(predicate()),
            then_block: Box::new(LashExpr::Return(Box::new(if method.ends_with("Index") {
                variable(&index)
            } else {
                item.clone()
            }))),
            else_block: Box::new(LashExpr::Undefined),
        },
        "reduce" | "reduceRight" => LashExpr::If {
            condition: Box::new(variable(&initialized)),
            then_block: Box::new(assign(
                &accumulator,
                call(vec![
                    variable(&accumulator),
                    item.clone(),
                    variable(&index),
                    variable(receiver),
                ]),
            )),
            else_block: Box::new(LashExpr::Block(vec![
                assign(&accumulator, item.clone()),
                assign(&initialized, LashExpr::Bool(true)),
            ])),
        },
        _ => unreachable!("callback method inventory is exhaustive"),
    };
    expressions.push(LashExpr::While {
        condition: Box::new(condition),
        body: Box::new(LashExpr::Block(vec![operation, step])),
    });
    expressions.push(match method {
        "map" | "arrayFromMap" | "filter" | "flatMap" => variable(&output),
        "forEach" => LashExpr::Undefined,
        "some" => LashExpr::Bool(false),
        "every" => LashExpr::Bool(true),
        "find" | "findLast" => LashExpr::Undefined,
        "findIndex" | "findLastIndex" => LashExpr::Number(-1.0),
        "reduce" | "reduceRight" => LashExpr::If {
            condition: Box::new(variable(&initialized)),
            then_block: Box::new(variable(&accumulator)),
            else_block: Box::new(stdlib("__reduceEmpty", Vec::new())),
        },
        _ => unreachable!(),
    });
    Ok(LashExpr::Block(expressions))
}

fn sort_body(receiver: &str, callback: &str, lowerer: &mut Lowerer) -> LashExpr {
    let index = lowerer.temporary("sort_index");
    let cursor = lowerer.temporary("sort_cursor");
    let current = lowerer.temporary("sort_current");
    let previous = || LashExpr::Index {
        target: Box::new(variable(receiver)),
        index: Box::new(subtract(variable(&cursor), LashExpr::Number(1.0))),
    };
    let compare = LashExpr::Call {
        function: Box::new(variable(callback)),
        args: vec![previous(), variable(&current)],
    };
    let shift = LashExpr::Assign {
        target: AssignTarget {
            root: receiver.into(),
            steps: vec![AssignPathStep::Index(variable(&cursor))],
        },
        expr: Box::new(previous()),
    };
    let insert = LashExpr::Assign {
        target: AssignTarget {
            root: receiver.into(),
            steps: vec![AssignPathStep::Index(variable(&cursor))],
        },
        expr: Box::new(variable(&current)),
    };
    LashExpr::Block(vec![
        assign(&index, LashExpr::Number(1.0)),
        LashExpr::While {
            condition: Box::new(binary(
                variable(&index),
                JavaScriptBinaryOp::Less,
                field(receiver, "length"),
            )),
            body: Box::new(LashExpr::Block(vec![
                assign(
                    &current,
                    LashExpr::Index {
                        target: Box::new(variable(receiver)),
                        index: Box::new(variable(&index)),
                    },
                ),
                assign(&cursor, variable(&index)),
                LashExpr::While {
                    condition: Box::new(binary(
                        binary(
                            variable(&cursor),
                            JavaScriptBinaryOp::Greater,
                            LashExpr::Number(0.0),
                        ),
                        JavaScriptBinaryOp::StrictEqual,
                        LashExpr::Bool(true),
                    )),
                    body: Box::new(LashExpr::If {
                        // Array sort places `undefined` after every defined
                        // value without invoking compareFn for either case.
                        condition: Box::new(binary(
                            variable(&current),
                            JavaScriptBinaryOp::StrictEqual,
                            LashExpr::Undefined,
                        )),
                        then_block: Box::new(LashExpr::Break),
                        else_block: Box::new(LashExpr::If {
                            condition: Box::new(binary(
                                previous(),
                                JavaScriptBinaryOp::StrictEqual,
                                LashExpr::Undefined,
                            )),
                            then_block: Box::new(LashExpr::Block(vec![
                                shift.clone(),
                                assign(&cursor, subtract(variable(&cursor), LashExpr::Number(1.0))),
                            ])),
                            else_block: Box::new(LashExpr::If {
                                condition: Box::new(binary(
                                    compare,
                                    JavaScriptBinaryOp::Greater,
                                    LashExpr::Number(0.0),
                                )),
                                then_block: Box::new(LashExpr::Block(vec![
                                    shift,
                                    assign(
                                        &cursor,
                                        subtract(variable(&cursor), LashExpr::Number(1.0)),
                                    ),
                                ])),
                                else_block: Box::new(LashExpr::Break),
                            }),
                        }),
                    }),
                },
                insert,
                assign(&index, add(variable(&index), LashExpr::Number(1.0))),
            ])),
        },
        variable(receiver),
    ])
}

fn callback_arity(method: &str, expected: &str) -> Diagnostic {
    // Arity, not availability: the generic "use a listed method" advice would
    // send the model looking for a different method than the correct one.
    Diagnostic::defect(
        DiagnosticCode::MethodUnsupported,
        format!("Array.{method} expects {expected}"),
        None,
    )
    .with_hint(format!("call `{method}` with {expected}"))
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

fn subtract(left: LashExpr, right: LashExpr) -> LashExpr {
    binary(left, JavaScriptBinaryOp::Subtract, right)
}

/// IsCallable, throwing ECMA's TypeError when the value is not a function.
fn require_callable(value: LashExpr) -> LashExpr {
    stdlib("Lash.RequireCallable", vec![value])
}

fn stdlib(method: &str, mut args: Vec<LashExpr>) -> LashExpr {
    args.insert(0, LashExpr::String(method.into()));
    LashExpr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args,
    }
}
