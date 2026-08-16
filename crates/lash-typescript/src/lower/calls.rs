use super::*;

impl Lowerer {
    pub(super) fn lower_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        if args
            .iter()
            .any(|argument| matches!(argument, CallArg::Spread(_)))
        {
            let callee = self.lower_expr(callee)?;
            return self.lower_dynamic_call_value(callee, args);
        }
        let plain_args = args
            .iter()
            .map(|argument| match argument {
                CallArg::Value(value) => value.clone(),
                CallArg::Spread(_) => unreachable!(),
            })
            .collect::<Vec<_>>();
        let args = plain_args.as_slice();
        if let Expr::Ident(name) = callee
            && !self.has_binding(name)
        {
            if matches!(name.as_str(), "String" | "Number" | "Boolean") {
                let value = args
                    .first()
                    .map(|value| self.lower_expr(value))
                    .transpose()?;
                return Ok(match (name.as_str(), value) {
                    ("String", None) => LashExpr::String("".into()),
                    ("String", Some(value)) => js_add(LashExpr::String("".into()), value),
                    ("Number", None) => LashExpr::Number(0.0),
                    ("Number", Some(value)) => js_unary(JavaScriptUnaryOp::Plus, value),
                    ("Boolean", None) => LashExpr::Bool(false),
                    ("Boolean", Some(value)) => js_unary(
                        JavaScriptUnaryOp::Not,
                        js_unary(JavaScriptUnaryOp::Not, value),
                    ),
                    _ => unreachable!(),
                });
            }
            if matches!(name.as_str(), "parseInt" | "parseFloat") {
                let expected = if name == "parseInt" { 1..=2 } else { 1..=1 };
                if !expected.contains(&args.len()) {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!(
                            "{name} expects {} argument(s)",
                            if name == "parseInt" {
                                "one or two"
                            } else {
                                "one"
                            }
                        ),
                        None,
                    ));
                }
                let mut values = vec![LashExpr::String(
                    if name == "parseInt" {
                        "Number.parseInt"
                    } else {
                        "Number.parseFloat"
                    }
                    .into(),
                )];
                values.extend(
                    args.iter()
                        .map(|value| self.lower_expr(value))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: values,
                });
            }
            if matches!(name.as_str(), "isNaN" | "isFinite") {
                let [value] = args else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("{name} expects one argument"),
                        None,
                    ));
                };
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String(
                            if name == "isNaN" {
                                "Number.isNaN"
                            } else {
                                "Number.isFinite"
                            }
                            .into(),
                        ),
                        js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
                    ],
                });
            }
            if matches!(
                name.as_str(),
                "encodeURIComponent" | "decodeURIComponent" | "encodeURI" | "decodeURI"
            ) {
                let [value] = args else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        format!("{name} expects exactly one argument"),
                        None,
                    ));
                };
                if matches!(name.as_str(), "encodeURIComponent" | "encodeURI")
                    && matches!(value, Expr::LoneSurrogateString)
                {
                    return Ok(LashExpr::Throw(Box::new(LashExpr::BuiltinCall {
                        name: "__typescript_heap_new".into(),
                        args: vec![
                            LashExpr::String("URIError".into()),
                            LashExpr::String("URI malformed".into()),
                        ],
                    })));
                }
                let intrinsic = match name.as_str() {
                    "encodeURIComponent" => "__typescript_encode_uri_component",
                    "decodeURIComponent" => "__typescript_decode_uri_component",
                    "encodeURI" => "__typescript_encode_uri",
                    "decodeURI" => "__typescript_decode_uri",
                    _ => unreachable!(),
                };
                return Ok(LashExpr::BuiltinCall {
                    name: intrinsic.into(),
                    args: vec![self.lower_expr(value)?],
                });
            }
            if matches!(name.as_str(), "btoa" | "atob") {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!(
                        "Unsupported: {name}. Use a deterministic host tool until the runtime can preserve Node's DOMException identity."
                    ),
                    None,
                ));
            }
            if name == "structuredClone" {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: structuredClone. For JSON-shaped data use JSON.parse(JSON.stringify(value)).",
                    None,
                ));
            }
            if matches!(
                name.as_str(),
                "Error"
                    | "TypeError"
                    | "RangeError"
                    | "SyntaxError"
                    | "ReferenceError"
                    | "URIError"
                    | "EvalError"
                    | "AggregateError"
            ) {
                let args = args.iter().cloned().map(CallArg::Value).collect::<Vec<_>>();
                return self.lower_constructor(name, &args);
            }
            return match (name.as_str(), args) {
                ("finish", [_]) if self.process_depth > 0 => Err(Diagnostic::with_repair(
                    DiagnosticCode::UnsupportedExpression,
                    "finish is cell-only",
                    "return from defineProcess.run so enclosing finally blocks execute",
                    None,
                )),
                ("finish", [value]) => Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?))),
                ("print", [value]) => Ok(LashExpr::Print(Box::new(self.lower_expr(value)?))),
                ("wake", [value]) => Ok(LashExpr::Wake(Box::new(self.lower_expr(value)?))),
                ("wake", [run, Expr::String(signal), payload]) => Ok(LashExpr::SignalRun {
                    run: Box::new(self.lower_expr(run)?),
                    name: signal.as_str().into(),
                    payload: Box::new(self.lower_expr(payload)?),
                }),
                ("sleep", [milliseconds]) if self.await_depth > 0 => {
                    Ok(LashExpr::SleepFor(Box::new(self.lower_expr(milliseconds)?)))
                }
                ("waitSignal", [Expr::String(name)]) if self.await_depth > 0 => {
                    Ok(LashExpr::WaitSignal {
                        name: name.as_str().into(),
                    })
                }
                ("start", [Expr::Ident(target)]) => self.lower_start(target, &[]),
                ("start", [Expr::Ident(target), Expr::Object(entries)]) => {
                    self.lower_start(target, entries)
                }
                ("registerTrigger", [config]) if self.await_depth > 0 => {
                    Ok(LashExpr::ReceiverCall {
                        receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                            vec!["triggers".into()],
                        ))),
                        operation: "register".into(),
                        args: vec![self.lower_expr(config)?],
                    })
                }
                ("defineProcess", _) => Err(Diagnostic::new(
                    DiagnosticCode::ProcessDefinitionNotTopLevel,
                    "defineProcess must initialize a top-level binding",
                    None,
                )),
                ("sleep" | "waitSignal" | "registerTrigger", _) if self.await_depth == 0 => {
                    Err(Diagnostic::new(
                        DiagnosticCode::AwaitRequired,
                        format!("agent primitive `{name}` requires await"),
                        None,
                    ))
                }
                (
                    "finish" | "print" | "wake" | "sleep" | "waitSignal" | "start"
                    | "registerTrigger",
                    _,
                ) => Err(Diagnostic::new(
                    DiagnosticCode::UnsupportedExpression,
                    format!("invalid arguments for agent primitive `{name}`"),
                    None,
                )),
                _ => Ok(LashExpr::Call {
                    function: Box::new(self.lower_expr(callee)?),
                    args: args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<_, _>>()?,
                }),
            };
        }
        if let Expr::Member {
            object,
            property: MemberProperty::Field(method),
        } = callee
        {
            if matches!(object.as_ref(), Expr::Ident(name) if name == "crypto")
                && method == "randomUUID"
                && !self.has_binding("crypto")
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: crypto.randomUUID. Use a journaled host tool that returns an identifier.",
                    None,
                ));
            }
            if matches!(method.as_str(), "then" | "catch" | "finally") {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: Promise chaining with .then/.catch/.finally. Use direct await and try/catch/finally.",
                    None,
                ));
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Promise")
                && !self.has_binding("Promise")
            {
                match method.as_str() {
                    "race" | "any" => {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MethodUnsupported,
                            format!(
                                "Unsupported: Promise.{method} requires durable partial-settlement ordering (FIG-1416). Use Promise.all/Promise.allSettled, or await durable sleep for timeout patterns."
                            ),
                            None,
                        ));
                    }
                    "resolve" | "reject" => {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MethodUnsupported,
                            format!(
                                "Unsupported: Promise.{method}. Await values directly and use throw/try-catch for failures."
                            ),
                            None,
                        ));
                    }
                    "all" | "allSettled" if self.await_depth == 0 => {
                        return Err(Diagnostic::new(
                            DiagnosticCode::AwaitRequired,
                            format!("Promise.{method} must be awaited directly"),
                            None,
                        ));
                    }
                    _ => {}
                }
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "JSON")
                && !self.has_binding("JSON")
            {
                if method == "parse" && args.len() > 1 {
                    return Err(reject_json_parse_reviver());
                }
                if method == "stringify" {
                    if args.len() > 3 {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MethodUnsupported,
                            "JSON.stringify expects value, optional replacer, and optional space",
                            None,
                        ));
                    }
                    if args.is_empty() {
                        return Ok(LashExpr::BuiltinCall {
                            name: "__typescript_stdlib".into(),
                            args: vec![
                                LashExpr::String("JSON.stringify".into()),
                                LashExpr::Undefined,
                            ],
                        });
                    }
                    let value = &args[0];
                    let replacer = args.get(1);
                    let function_replacer = replacer.filter(|replacer| {
                        !matches!(replacer, Expr::Null | Expr::Undefined | Expr::Array(_))
                    });
                    let property_replacer =
                        function_replacer.is_none().then_some(replacer).flatten();
                    return self.lower_json_stringify(
                        value,
                        function_replacer,
                        property_replacer,
                        args.get(2),
                    );
                }
            }
            if let Some(replacement) = match method.as_str() {
                "getFullYear" => Some("getUTCFullYear"),
                "getMonth" => Some("getUTCMonth"),
                "getDate" => Some("getUTCDate"),
                "getDay" => Some("getUTCDay"),
                "getHours" => Some("getUTCHours"),
                "getMinutes" => Some("getUTCMinutes"),
                "getSeconds" => Some("getUTCSeconds"),
                "getMilliseconds" => Some("getUTCMilliseconds"),
                _ => None,
            } {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!(
                        "Unsupported: Date.{method} is host-timezone dependent. Use d.{replacement}()."
                    ),
                    None,
                ));
            }
            if matches!(
                method.as_str(),
                "setUTCFullYear"
                    | "setUTCMonth"
                    | "setUTCDate"
                    | "setUTCHours"
                    | "setUTCMinutes"
                    | "setUTCSeconds"
                    | "setUTCMilliseconds"
            ) {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!(
                        "Unsupported: Date.{method}; durable Date values are immutable. Use new Date(d.getTime() + n)."
                    ),
                    None,
                ));
            }
            if matches!(
                method.as_str(),
                "toDateString"
                    | "toTimeString"
                    | "toUTCString"
                    | "toGMTString"
                    | "toLocaleDateString"
                    | "toLocaleTimeString"
            ) {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!(
                        "Unsupported: Date.{method} is timezone/locale dependent. Use Date.toISOString()."
                    ),
                    None,
                ));
            }
            if method == "localeCompare" {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: localeCompare/Intl ordering is host-dependent. Use (a < b ? -1 : a > b ? 1 : 0).",
                    None,
                ));
            }
            if method == "normalize" {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: String.normalize depends on Unicode normalization data outside the pinned v1 VM. Normalize text in a deterministic host tool before the cell.",
                    None,
                ));
            }
            if method == "toLocaleString" {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: toLocaleString/Intl formatting is locale-dependent. For Date use d.toISOString(); for numbers use toFixed(digits); otherwise build the deterministic string explicitly.",
                    None,
                ));
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "console")
                && matches!(method.as_str(), "log" | "warn" | "error" | "info" | "debug")
            {
                if !self.has_binding("console") {
                    let mut lowered = args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter();
                    let joined = lowered.next().map_or_else(
                        || LashExpr::String("".into()),
                        |first| js_add(LashExpr::String("".into()), first),
                    );
                    let joined = lowered.fold(joined, |joined, value| {
                        js_add(js_add(joined, LashExpr::String(" ".into())), value)
                    });
                    return Ok(LashExpr::Print(Box::new(joined)));
                }
                if self.has_binding("console") {
                    return Ok(LashExpr::Call {
                        function: Box::new(self.lower_expr(callee)?),
                        args: args
                            .iter()
                            .map(|arg| self.lower_expr(arg))
                            .collect::<Result<_, _>>()?,
                    });
                }
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Date")
                && method == "now"
                && args.is_empty()
                && !self.has_binding("Date")
            {
                return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                    "now",
                ))));
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Math")
                && method == "random"
                && args.is_empty()
                && !self.has_binding("Math")
            {
                return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                    "random",
                ))));
            }
            let receiver_is_module_authority = module_path(object)
                .and_then(|path| path.first().cloned())
                .is_some_and(|root| !self.has_binding(&root));
            if !receiver_is_module_authority
                && let Some(lowered) = self.lower_regexp_method(object, method, args)?
            {
                return Ok(lowered);
            }
            if matches!(method.as_str(), "entries" | "keys" | "values")
                && static_stdlib_owner(object).is_none()
                && self.iterable_sink_depth > 0
            {
                let exotic = match object.as_ref() {
                    Expr::New { constructor, .. }
                        if matches!(constructor.as_str(), "Map" | "Set" | "URLSearchParams") =>
                    {
                        true
                    }
                    Expr::Member {
                        property: MemberProperty::Field(field),
                        ..
                    } if field == "searchParams" => true,
                    Expr::Ident(name) => self
                        .binding(name)
                        .ok()
                        .and_then(|binding| self.iterable_kinds.get(&binding.internal))
                        .is_some(),
                    _ => false,
                };
                let receiver = self.temporary("iterator_receiver");
                let receiver_value = self.lower_expr(object)?;
                let variable = || LashExpr::Variable(receiver.as_str().into());
                if exotic {
                    return Ok(LashExpr::Block(vec![
                        LashExpr::Assign {
                            target: AssignTarget::variable(receiver.as_str().into()),
                            expr: Box::new(receiver_value),
                        },
                        LashExpr::BuiltinCall {
                            name: "__typescript_stdlib".into(),
                            args: vec![LashExpr::String(method.as_str().into()), variable()],
                        },
                    ]));
                }
                let array = match method.as_str() {
                    "values" => LashExpr::BuiltinCall {
                        name: "__typescript_stdlib".into(),
                        args: vec![
                            LashExpr::String("Lash.ArrayFromIterable".into()),
                            variable(),
                        ],
                    },
                    "entries" => {
                        let pair = self.temporary("array_entry");
                        let at = |index| LashExpr::Index {
                            target: Box::new(LashExpr::Variable(pair.as_str().into())),
                            index: Box::new(LashExpr::Number(index)),
                        };
                        LashExpr::Map {
                            items: Box::new(LashExpr::BuiltinCall {
                                name: "__typescript_stdlib".into(),
                                args: vec![LashExpr::String("__enumerate".into()), variable()],
                            }),
                            function: Box::new(LashExpr::Function(Box::new(FunctionExpr {
                                name: None,
                                params: vec![pair.as_str().into()],
                                captures: Vec::new(),
                                body: Box::new(LashExpr::List(vec![at(1.0), at(0.0)])),
                            }))),
                        }
                    }
                    "keys" => {
                        let key = self.temporary("array_key");
                        LashExpr::Map {
                            items: Box::new(LashExpr::BuiltinCall {
                                name: "__typescript_stdlib".into(),
                                args: vec![LashExpr::String("Object.keys".into()), variable()],
                            }),
                            function: Box::new(LashExpr::Function(Box::new(FunctionExpr {
                                name: None,
                                params: vec![key.as_str().into()],
                                captures: Vec::new(),
                                body: Box::new(LashExpr::JavaScriptUnary {
                                    op: JavaScriptUnaryOp::Plus,
                                    expr: Box::new(LashExpr::Variable(key.as_str().into())),
                                }),
                            }))),
                        }
                    }
                    _ => unreachable!(),
                };
                return Ok(LashExpr::Block(vec![
                    LashExpr::Assign {
                        target: AssignTarget::variable(receiver.as_str().into()),
                        expr: Box::new(receiver_value),
                    },
                    array,
                ]));
            }
            if matches!(method.as_str(), "entries" | "keys" | "values")
                && static_stdlib_owner(object).is_none()
                && self.iterable_sink_depth == 0
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    "Unsupported: iterator methods may only be consumed directly by for-of / spread / Array.from / new Map|Set / Object.fromEntries — wrap: [...expr]",
                    None,
                ));
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Array")
                && method == "from"
                && !self.has_binding("Array")
            {
                let (value, mapping_args) = match args {
                    [value] => (value, &[][..]),
                    [value, callback] => (value, std::slice::from_ref(callback)),
                    [value, _callback, _this_arg] => {
                        (value, args.get(1..).expect("mapping arguments exist"))
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            DiagnosticCode::MethodUnsupported,
                            "Array.from expects a source and optional mapping callback",
                            None,
                        ));
                    }
                };
                let array = LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String("Lash.ArrayFromIterable".into()),
                        self.lower_iterable_sink(value)?,
                    ],
                };
                return if mapping_args.is_empty() {
                    Ok(array)
                } else {
                    self.lower_array_from_mapping(array, mapping_args)
                };
            }
            if matches!(object.as_ref(), Expr::Ident(name) if name == "Object")
                && method == "fromEntries"
                && !self.has_binding("Object")
            {
                let [value] = args else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        "Object.fromEntries expects one iterable",
                        None,
                    ));
                };
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String("Object.fromEntries".into()),
                        self.lower_iterable_sink(value)?,
                    ],
                });
            }
            if method == "hasOwnProperty" {
                let [key] = args else {
                    return Err(Diagnostic::with_repair(
                        DiagnosticCode::UnsupportedExpression,
                        "hasOwnProperty expects exactly one key",
                        "use Object.hasOwn(object, key)",
                        None,
                    ));
                };
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![
                        LashExpr::String("Object.hasOwn".into()),
                        self.lower_expr(object)?,
                        self.lower_expr(key)?,
                    ],
                });
            }
            if method == "replace"
                && let [needle, callback @ Expr::Function(_)] = args
            {
                return self.lower_string_replace_callback(object, needle, callback);
            }
            if matches!(object.as_ref(), Expr::Ident(owner) if matches!(owner.as_str(), "Object" | "Map"))
                && method == "groupBy"
                && !self.has_binding(match object.as_ref() {
                    Expr::Ident(owner) => owner,
                    _ => unreachable!(),
                })
            {
                let [source, callback] = args else {
                    return Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        "groupBy expects an iterable and one callback",
                        None,
                    ));
                };
                let Expr::Ident(owner) = object.as_ref() else {
                    unreachable!()
                };
                return self.lower_group_by(owner, source, callback);
            }
            if let Some(static_owner) = static_stdlib_owner(object)
                && !self.has_binding(static_owner)
                && is_static_stdlib_method(static_owner, method)
            {
                let mut builtin_args =
                    vec![LashExpr::String(format!("{static_owner}.{method}").into())];
                builtin_args.extend(
                    args.iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: builtin_args,
                });
            }
            // A literal receiver that cannot carry this method is decided here,
            // before any per-method lowering. `map` used to be routed ahead of
            // this check and so skipped it, leaving `"ab".map(f)` to fail at run
            // time with a shaping error instead of being named — one receiver
            // shape short of the classification claim.
            if is_instance_stdlib_method(method)
                && has_literal_stdlib_receiver(object)
                && !literal_supports_instance_method(object, method)
            {
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!("method `{method}` is unavailable on this literal receiver"),
                    None,
                ));
            }
            // Callback methods stay entirely inside the VM. The synchronous
            // family shares the effect-rejecting callback frame; async `map`
            // retains the durable sequential async-map path.
            if method == "map" && matches!(args, [Expr::Function(function)] if function.is_async) {
                return self.lower_array_map(object, args);
            }
            let receiver_is_callback_exotic = method == "forEach"
                && match object.as_ref() {
                    Expr::New { constructor, .. } => {
                        matches!(constructor.as_str(), "Map" | "Set" | "URLSearchParams")
                    }
                    Expr::Ident(name) => self
                        .binding(name)
                        .ok()
                        .and_then(|binding| self.iterable_kinds.get(&binding.internal))
                        .is_some_and(|kind| {
                            matches!(kind.as_str(), "Map" | "Set" | "URLSearchParams")
                        }),
                    _ => false,
                };
            if !receiver_is_callback_exotic
                && matches!(
                    method.as_str(),
                    "map"
                        | "filter"
                        | "reduce"
                        | "reduceRight"
                        | "find"
                        | "findIndex"
                        | "findLast"
                        | "findLastIndex"
                        | "some"
                        | "every"
                        | "forEach"
                        | "flatMap"
                )
                || !receiver_is_callback_exotic
                    && matches!(method.as_str(), "sort" | "toSorted")
                    && args
                        .first()
                        .is_some_and(|argument| !matches!(argument, Expr::Undefined))
            {
                return self.lower_array_callback_method(method, object, args);
            }
            if is_instance_stdlib_method(method) {
                let mut builtin_args = vec![
                    LashExpr::String(method.as_str().into()),
                    self.lower_expr(object)?,
                ];
                builtin_args.extend(
                    args.iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<Vec<_>, _>>()?,
                );
                return Ok(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: builtin_args,
                });
            }

            if method.starts_with(|character: char| character.is_ascii_uppercase())
                && module_path(object)
                    .and_then(|path| path.first().cloned())
                    .is_some_and(|root| !self.has_binding(&root))
            {
                return Ok(LashExpr::ReceiverCall {
                    receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                        module_path(object)
                            .expect("constructor path checked above")
                            .into_iter()
                            .map(Into::into)
                            .collect(),
                    ))),
                    operation: method.as_str().into(),
                    args: args
                        .iter()
                        .map(|arg| self.lower_expr(arg))
                        .collect::<Result<_, _>>()?,
                });
            }

            // Classify by method name before the tool-call branch. A receiver
            // that is not a module authority — a chained call, a local binding,
            // a computed member, a literal — can never dispatch a tool, so an
            // unadvertised method there is a missing method and must say so.
            // Falling through reported it as a tool call needing `await`, and
            // under `await` it lowered and failed at the host untyped.
            let receiver_is_module_authority = module_path(object)
                .and_then(|path| path.first().cloned())
                .is_some_and(|root| !self.has_binding(&root));
            let ecma_owner = match object.as_ref() {
                Expr::Ident(owner)
                    if is_ecma_global_namespace(owner) && !self.has_binding(owner) =>
                {
                    Some(owner.as_str())
                }
                _ => None,
            };
            if ecma_owner.is_some()
                || has_literal_stdlib_receiver(object)
                || (!receiver_is_module_authority && !is_instance_stdlib_method(method))
            {
                let name = match ecma_owner {
                    Some(owner) => format!("{owner}.{method}"),
                    None => method.to_string(),
                };
                return Err(Diagnostic::new(
                    DiagnosticCode::MethodUnsupported,
                    format!("method `{name}` is not in the TypeScript runtime surface"),
                    None,
                ));
            }

            if self.await_depth == 0 {
                return Err(Diagnostic::new(
                    DiagnosticCode::AwaitRequired,
                    format!(
                        "tool call `{method}` must appear under await or Promise.all/allSettled"
                    ),
                    None,
                ));
            }
            let receiver = if let Some(path) = module_path(object)
                && path
                    .first()
                    .is_some_and(|root| !self.has_binding(root.as_str()))
            {
                LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                    path.into_iter().map(Into::into).collect(),
                ))
            } else {
                self.lower_expr(object)?
            };
            return Ok(LashExpr::ReceiverCall {
                receiver: Box::new(receiver),
                operation: method.as_str().into(),
                args: args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<Result<_, _>>()?,
            });
        }
        if let Expr::Ident(name) = callee
            && let Some(binding) = self
                .scopes
                .iter()
                .rev()
                .find_map(|scope| scope.bindings.get(name))
            && self.async_bindings.contains(&binding.internal)
            && self.await_depth == 0
        {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitRequired,
                format!("async helper `{name}` must be awaited directly"),
                None,
            ));
        }
        Ok(LashExpr::Call {
            function: Box::new(self.lower_expr(callee)?),
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }
    fn lower_start(
        &mut self,
        target: &str,
        entries: &[ObjectProperty],
    ) -> Result<LashExpr, Diagnostic> {
        let Some(process) = self.process_bindings.get(target).cloned() else {
            return Err(Diagnostic::new(
                DiagnosticCode::ProcessTargetStaticRequired,
                format!("`{target}` is not a top-level defineProcess binding"),
                None,
            ));
        };
        Ok(LashExpr::StartProcess(ProcessStartExpr {
            process: process.into(),
            args: entries
                .iter()
                .map(|property| match property {
                    ObjectProperty::KeyValue(PropertyKey::Static(name), value) => {
                        Ok((name.as_str().into(), self.lower_expr(value)?))
                    }
                    _ => Err(Diagnostic::new(
                        DiagnosticCode::UnsupportedExpression,
                        "start arguments require static properties without spread",
                        None,
                    )),
                })
                .collect::<Result<_, Diagnostic>>()?,
        }))
    }
}
