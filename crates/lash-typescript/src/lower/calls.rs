use super::*;

enum CalleeFamily<'a> {
    UnboundGlobal(&'a str),
    Member { object: &'a Expr, method: &'a str },
    AsyncHelper(&'a str),
    Dynamic,
}

#[derive(Clone, Copy)]
enum GlobalBuiltin {
    Coercion(CoercionBuiltin),
    NumberParser(NumberParser),
    NumberPredicate(NumberPredicate),
    Uri(UriBuiltin),
    RejectedDom(RejectedDomBuiltin),
    StructuredClone,
    ErrorConstructor(ErrorConstructor),
    RegExpConstructor,
    AgentPrimitive(AgentPrimitive),
}

impl GlobalBuiltin {
    fn classify(name: &str) -> Option<Self> {
        match name {
            "String" => Some(Self::Coercion(CoercionBuiltin::String)),
            "Number" => Some(Self::Coercion(CoercionBuiltin::Number)),
            "Boolean" => Some(Self::Coercion(CoercionBuiltin::Boolean)),
            "parseInt" => Some(Self::NumberParser(NumberParser::Int)),
            "parseFloat" => Some(Self::NumberParser(NumberParser::Float)),
            "isNaN" => Some(Self::NumberPredicate(NumberPredicate::NaN)),
            "isFinite" => Some(Self::NumberPredicate(NumberPredicate::Finite)),
            "encodeURIComponent" => Some(Self::Uri(UriBuiltin::EncodeComponent)),
            "decodeURIComponent" => Some(Self::Uri(UriBuiltin::DecodeComponent)),
            "encodeURI" => Some(Self::Uri(UriBuiltin::Encode)),
            "decodeURI" => Some(Self::Uri(UriBuiltin::Decode)),
            "btoa" => Some(Self::RejectedDom(RejectedDomBuiltin::Btoa)),
            "atob" => Some(Self::RejectedDom(RejectedDomBuiltin::Atob)),
            "structuredClone" => Some(Self::StructuredClone),
            "Error" => Some(Self::ErrorConstructor(ErrorConstructor::Error)),
            "TypeError" => Some(Self::ErrorConstructor(ErrorConstructor::Type)),
            "RangeError" => Some(Self::ErrorConstructor(ErrorConstructor::Range)),
            "SyntaxError" => Some(Self::ErrorConstructor(ErrorConstructor::Syntax)),
            "ReferenceError" => Some(Self::ErrorConstructor(ErrorConstructor::Reference)),
            "URIError" => Some(Self::ErrorConstructor(ErrorConstructor::Uri)),
            "EvalError" => Some(Self::ErrorConstructor(ErrorConstructor::Eval)),
            "AggregateError" => Some(Self::ErrorConstructor(ErrorConstructor::Aggregate)),
            "RegExp" => Some(Self::RegExpConstructor),
            "finish" => Some(Self::AgentPrimitive(AgentPrimitive::Finish)),
            "print" => Some(Self::AgentPrimitive(AgentPrimitive::Print)),
            "sleep" => Some(Self::AgentPrimitive(AgentPrimitive::Sleep)),
            "waitSignal" => Some(Self::AgentPrimitive(AgentPrimitive::WaitSignal)),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum CoercionBuiltin {
    String,
    Number,
    Boolean,
}

#[derive(Clone, Copy)]
enum NumberParser {
    Int,
    Float,
}

impl NumberParser {
    fn name(self) -> &'static str {
        match self {
            Self::Int => "parseInt",
            Self::Float => "parseFloat",
        }
    }
}

#[derive(Clone, Copy)]
enum NumberPredicate {
    NaN,
    Finite,
}

impl NumberPredicate {
    fn name(self) -> &'static str {
        match self {
            Self::NaN => "isNaN",
            Self::Finite => "isFinite",
        }
    }
}

#[derive(Clone, Copy)]
enum UriBuiltin {
    EncodeComponent,
    DecodeComponent,
    Encode,
    Decode,
}

impl UriBuiltin {
    fn name(self) -> &'static str {
        match self {
            Self::EncodeComponent => "encodeURIComponent",
            Self::DecodeComponent => "decodeURIComponent",
            Self::Encode => "encodeURI",
            Self::Decode => "decodeURI",
        }
    }

    fn intrinsic(self) -> &'static str {
        match self {
            Self::EncodeComponent => "__typescript_encode_uri_component",
            Self::DecodeComponent => "__typescript_decode_uri_component",
            Self::Encode => "__typescript_encode_uri",
            Self::Decode => "__typescript_decode_uri",
        }
    }

    fn rejects_lone_surrogate(self) -> bool {
        matches!(self, Self::EncodeComponent | Self::Encode)
    }
}

#[derive(Clone, Copy)]
enum RejectedDomBuiltin {
    Btoa,
    Atob,
}

impl RejectedDomBuiltin {
    fn name(self) -> &'static str {
        match self {
            Self::Btoa => "btoa",
            Self::Atob => "atob",
        }
    }
}

#[derive(Clone, Copy)]
enum ErrorConstructor {
    Error,
    Type,
    Range,
    Syntax,
    Reference,
    Uri,
    Eval,
    Aggregate,
}

impl ErrorConstructor {
    fn name(self) -> &'static str {
        match self {
            Self::Error => "Error",
            Self::Type => "TypeError",
            Self::Range => "RangeError",
            Self::Syntax => "SyntaxError",
            Self::Reference => "ReferenceError",
            Self::Uri => "URIError",
            Self::Eval => "EvalError",
            Self::Aggregate => "AggregateError",
        }
    }
}

#[derive(Clone, Copy)]
enum AgentPrimitive {
    Finish,
    Print,
    Sleep,
    WaitSignal,
}

impl AgentPrimitive {
    fn name(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::Print => "print",
            Self::Sleep => "sleep",
            Self::WaitSignal => "waitSignal",
        }
    }
}

/// Whether an unbound global `name` is a builtin the dialect lowers itself.
pub(super) fn is_global_builtin(name: &str) -> bool {
    GlobalBuiltin::classify(name).is_some()
}

fn normalize_call_args(args: &[CallArg]) -> Option<Vec<Expr>> {
    args.iter()
        .map(|argument| match argument {
            CallArg::Value(value) => Some(value.clone()),
            CallArg::Spread(_) => None,
        })
        .collect()
}

impl Lowerer {
    /// `receiver.method(args)` where `receiver` is an optional chain's
    /// current value, held in a generated slot: the slot is bound, for the
    /// call alone, under its own name, and the call lowers as any member call.
    pub(super) fn lower_chain_method_call(
        &mut self,
        receiver: &LashExpr,
        method: &str,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        let LashExpr::Variable(slot) = receiver else {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "an optional chain's current value is always a generated slot",
                None,
            ));
        };
        let slot = slot.to_string();
        let owner_function = self.current_function();
        let id = self.declare_in_ledger(BindingKind::Const, (owner_function, slot.clone()));
        self.scopes.push(Scope::default());
        #[expect(clippy::unwrap_used, reason = "the scope was pushed on the line above")]
        self.scopes.last_mut().unwrap().bindings.insert(
            slot.clone(),
            Binding {
                id,
                internal: slot.clone(),
                kind: BindingKind::Const,
                initialized: true,
                owner_function,
                role: BindingRole::Plain,
            },
        );
        let callee = Expr::Member {
            object: Box::new(Expr::Ident(slot, None)),
            property: MemberProperty::Field(method.to_string()),
            span: self.current_span.unwrap_or(SourceSpan { start: 0, end: 0 }),
        };
        let lowered = self.lower_call(&callee, args);
        self.scopes.pop();
        lowered
    }

    pub(super) fn lower_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        let Some(args) = normalize_call_args(args) else {
            if let Some(applied) = self.lower_builtin_spread_call(callee, args)? {
                return Ok(applied);
            }
            if let Expr::Member {
                object, property, ..
            } = callee
                && !matches!(object.as_ref(), Expr::Ident(owner, _)
                    if (owner == "globalThis" || is_ecma_global_namespace(owner))
                        && !self.has_binding(owner))
            {
                return self.lower_method_spread_call(object, property, args);
            }
            let callee = self.lower_expr(callee)?;
            return self.lower_dynamic_call_value(callee, args);
        };
        match self.classify_callee(callee) {
            CalleeFamily::UnboundGlobal(name) => {
                // A boundary dropped the name for holding a function: a
                // call of it is refused by name before the built-in folds
                // can answer.
                self.refuse_expired_global_read(name)?;
                // A bare `Date(...)` call ignores its arguments and answers
                // the current date-time string; like `Date.now()` it reads
                // the journaled clock.
                if name == "Date" {
                    return Ok(Self::stdlib_call(
                        "Lash.DateString",
                        vec![LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                            lashlang::LANGUAGE_RUNTIME_NOW_OPERATION,
                        )))],
                    ));
                }
                let Some(builtin) = GlobalBuiltin::classify(name) else {
                    return self.lower_dynamic_call(callee, &args);
                };
                self.lower_global_builtin(builtin, &args)
            }
            CalleeFamily::Member { object, method } => {
                self.lower_member_call(callee, object, method, &args)
            }
            CalleeFamily::AsyncHelper(name) if self.position.await_depth == 0 => {
                Err(Diagnostic::new(
                    DiagnosticCode::AwaitRequired,
                    format!("async helper `{name}` must be awaited directly"),
                    None,
                ))
            }
            CalleeFamily::AsyncHelper(_) | CalleeFamily::Dynamic => {
                self.lower_dynamic_call(callee, &args)
            }
        }
    }

    fn classify_callee<'a>(&self, callee: &'a Expr) -> CalleeFamily<'a> {
        match callee {
            Expr::Ident(name, _) if !self.has_binding(name) => CalleeFamily::UnboundGlobal(name),
            Expr::Member {
                object,
                property: MemberProperty::Field(method),
                ..
            } => CalleeFamily::Member { object, method },
            Expr::Ident(name, _)
                if self
                    .binding(name)
                    .is_ok_and(|binding| binding.role == BindingRole::AsyncHelper) =>
            {
                CalleeFamily::AsyncHelper(name)
            }
            _ => CalleeFamily::Dynamic,
        }
    }

    fn lower_global_builtin(
        &mut self,
        builtin: GlobalBuiltin,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        match builtin {
            GlobalBuiltin::Coercion(builtin) => self.lower_coercion_builtin(builtin, args),
            GlobalBuiltin::NumberParser(parser) => self.lower_number_parser(parser, args),
            GlobalBuiltin::NumberPredicate(predicate) => {
                self.lower_number_predicate(predicate, args)
            }
            GlobalBuiltin::Uri(builtin) => self.lower_uri_builtin(builtin, args),
            GlobalBuiltin::RejectedDom(builtin) => Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                format!(
                    "Unsupported: {}. Use a deterministic host tool until the runtime can preserve Node's DOMException identity.",
                    builtin.name()
                ),
                None,
            )),
            GlobalBuiltin::StructuredClone => Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: structuredClone. For JSON-shaped data use JSON.parse(JSON.stringify(value)).",
                None,
            )),
            GlobalBuiltin::ErrorConstructor(constructor) => {
                let args = args.iter().cloned().map(CallArg::Value).collect::<Vec<_>>();
                self.lower_constructor(constructor.name(), &args)
            }
            GlobalBuiltin::RegExpConstructor => self.lower_regexp_call(args),
            GlobalBuiltin::AgentPrimitive(primitive) => self.lower_agent_primitive(primitive, args),
        }
    }

    fn lower_coercion_builtin(
        &mut self,
        builtin: CoercionBuiltin,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        let value = args
            .first()
            .map(|value| self.lower_expr(value))
            .transpose()?;
        Ok(match (builtin, value) {
            (CoercionBuiltin::String, None) => LashExpr::String("".into()),
            (CoercionBuiltin::String, Some(value)) => js_unary(JavaScriptUnaryOp::ToString, value),
            (CoercionBuiltin::Number, None) => LashExpr::Number(0.0),
            (CoercionBuiltin::Number, Some(value)) => js_unary(JavaScriptUnaryOp::Plus, value),
            (CoercionBuiltin::Boolean, None) => LashExpr::Bool(false),
            (CoercionBuiltin::Boolean, Some(value)) => js_unary(
                JavaScriptUnaryOp::Not,
                js_unary(JavaScriptUnaryOp::Not, value),
            ),
        })
    }

    fn lower_number_parser(
        &mut self,
        parser: NumberParser,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        let (expected, expected_description, intrinsic) = match parser {
            NumberParser::Int => (1..=2, "one or two", "Number.parseInt"),
            NumberParser::Float => (1..=1, "one", "Number.parseFloat"),
        };
        if !expected.contains(&args.len()) {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!(
                    "{} expects {expected_description} argument(s)",
                    parser.name()
                ),
                None,
            ));
        }
        let mut values = vec![LashExpr::String(intrinsic.into())];
        values.extend(
            args.iter()
                .map(|value| self.lower_expr(value))
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_stdlib".into(),
            args: values,
        })
    }

    fn lower_number_predicate(
        &mut self,
        predicate: NumberPredicate,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        let [value] = args else {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!("{} expects one argument", predicate.name()),
                None,
            ));
        };
        let intrinsic = match predicate {
            NumberPredicate::NaN => "Number.isNaN",
            NumberPredicate::Finite => "Number.isFinite",
        };
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_stdlib".into(),
            args: vec![
                LashExpr::String(intrinsic.into()),
                js_unary(JavaScriptUnaryOp::Plus, self.lower_expr(value)?),
            ],
        })
    }

    fn lower_uri_builtin(
        &mut self,
        builtin: UriBuiltin,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        let [value] = args else {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!("{} expects exactly one argument", builtin.name()),
                None,
            ));
        };
        if builtin.rejects_lone_surrogate() && matches!(value, Expr::LoneSurrogateString) {
            return Ok(LashExpr::Throw(Box::new(LashExpr::BuiltinCall {
                name: "__typescript_heap_new".into(),
                args: vec![
                    LashExpr::String("URIError".into()),
                    LashExpr::String("URI malformed".into()),
                ],
            })));
        }
        Ok(LashExpr::BuiltinCall {
            name: builtin.intrinsic().into(),
            args: vec![self.lower_expr(value)?],
        })
    }

    /// `RegExp(pattern, flags)` called as a function: ECMA-262's `RegExp`
    /// with NewTarget `undefined` — a RegExp `pattern` and `undefined`
    /// `flags` return `pattern` itself, anything else constructs as
    /// `new RegExp` would. The VM's `construct` operation applies both
    /// halves; the constructor's own argument checks stay on `new`.
    fn lower_regexp_call(&mut self, args: &[Expr]) -> Result<LashExpr, Diagnostic> {
        if args.len() > 2 {
            return Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "RegExp expects at most two arguments".to_string(),
                None,
            ));
        }
        for (index, argument) in args.iter().enumerate() {
            if matches!(
                argument,
                Expr::Null
                    | Expr::Bool(_)
                    | Expr::Number(_)
                    | Expr::Array(_)
                    | Expr::Object(_)
                    | Expr::Function(_)
            ) {
                let label = if index == 0 { "pattern" } else { "flags" };
                return Err(Diagnostic::refusal(
                    DiagnosticCode::MethodUnsupported,
                    format!("RegExp {label} must be a string, a RegExp, or undefined"),
                    None,
                )
                .with_hint("pass an explicit string"));
            }
        }
        let mut values = vec![LashExpr::String("construct".into())];
        values.extend(
            args.iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<Vec<_>, _>>()?,
        );
        Ok(LashExpr::BuiltinCall {
            name: "__typescript_regexp".into(),
            args: values,
        })
    }

    fn lower_agent_primitive(
        &mut self,
        primitive: AgentPrimitive,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        match (primitive, args) {
            (AgentPrimitive::Finish, [_]) if self.process_depth > 0 => Err(Diagnostic::refusal(
                DiagnosticCode::UnsupportedExpression,
                "finish is cell-only",
                None,
            )
            .with_hint("return from the process body so enclosing finally blocks execute")),
            (AgentPrimitive::Finish, [value]) => {
                Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?)))
            }
            (AgentPrimitive::Print, [value]) => {
                Ok(LashExpr::Print(Box::new(self.lower_expr(value)?)))
            }
            (AgentPrimitive::Sleep, [milliseconds]) if self.position.await_depth > 0 => {
                Ok(LashExpr::SleepFor(Box::new(self.lower_expr(milliseconds)?)))
            }
            // An unawaited `sleep(ms)` is a pending timer: one handle like a
            // pending tool call, settled by the aggregate that awaits it, whose
            // start point is that aggregate's admission (ADR 0099 §11).
            (AgentPrimitive::Sleep, [milliseconds]) => Ok(LashExpr::BuiltinCall {
                name: "__typescript_pending_timer".into(),
                args: vec![self.lower_expr(milliseconds)?],
            }),
            (AgentPrimitive::WaitSignal, [Expr::String(name)]) if self.position.await_depth > 0 => {
                Ok(LashExpr::WaitSignal {
                    name: name.as_str().into(),
                })
            }
            (AgentPrimitive::WaitSignal, _) if self.position.await_depth == 0 => {
                Err(Diagnostic::new(
                    DiagnosticCode::AwaitRequired,
                    format!("agent primitive `{}` requires await", primitive.name()),
                    None,
                ))
            }
            _ => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                format!(
                    "invalid arguments for agent primitive `{}`",
                    primitive.name()
                ),
                None,
            )),
        }
    }

    fn lower_dynamic_call(&mut self, callee: &Expr, args: &[Expr]) -> Result<LashExpr, Diagnostic> {
        if let Expr::Member {
            object,
            property: MemberProperty::Index(key),
            ..
        } = callee
            && !matches!(object.as_ref(), Expr::Ident(owner, _)
                if (owner == "globalThis" || is_known_runtime_global(owner))
                    && !self.has_binding(owner))
        {
            let receiver = self.lower_expr(object)?;
            let key = self.lower_expr(key)?;
            return self.lower_method_call(receiver, MethodKey::Index(Box::new(key)), args);
        }
        Ok(LashExpr::Call {
            function: Box::new(self.lower_expr(callee)?),
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Whether `object.method(..)` calls a method of the program's own
    /// objects: a name that is not a built-in prototype method, on a receiver
    /// that is neither a module authority, an ECMA namespace nor a primitive
    /// literal.
    pub(super) fn is_own_method_call(&self, object: &Expr, method: &str) -> bool {
        let module_root = module_path(object).and_then(|path| path.first().cloned());
        let module_authority = module_root.as_ref().is_some_and(|root| {
            !self.has_binding(root) && !is_ecma_global_namespace(root)
                || self.has_binding(root) && self.module_authority_roots.contains(root)
        });
        // A session global from an earlier cell cannot hold a method: a value
        // reaching a function does not survive its cell (ADR 0062 entry 17).
        // Such a call keeps the method diagnostic, which the executor refines
        // into the shadowed-module one when a module of that name exists.
        let session_global = module_root
            .as_deref()
            .is_some_and(|root| self.is_session_global(root));
        !module_authority
            && !session_global
            && !matches!(object, Expr::Ident(owner, _)
                if (owner == "globalThis" || is_ecma_global_namespace(owner))
                    && !self.has_binding(owner))
            && (matches!(object, Expr::Object(_)) || !has_literal_stdlib_receiver(object))
            && !is_instance_stdlib_method(method)
            && !is_ecma_prototype_method(method)
    }

    /// Whether `name` resolves to a session global an earlier cell bound: the
    /// immutable ambient scope beneath the program's root.
    fn is_session_global(&self, name: &str) -> bool {
        self.root_scope_depth > 1
            && self
                .scopes
                .iter()
                .rposition(|scope| scope.bindings.contains_key(name))
                .is_some_and(|index| {
                    index == 0 && self.scopes[0].bindings[name].kind == BindingKind::Const
                })
    }

    /// A member call: the callee is read from the receiver, and the call binds
    /// the receiver as the callee's `this` (ECMA-262 EvaluateCall).
    fn lower_method_call(
        &mut self,
        receiver: LashExpr,
        method: MethodKey,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        Ok(LashExpr::MethodCall {
            receiver: Box::new(receiver),
            method,
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }

    /// Lowers one tool-call argument, discovering an inline process body.
    ///
    /// An async arrow in a tool call's argument position is a process literal
    /// (FIG-2997): the linker decides from the slot's expected type whether it
    /// lifts to a hoisted declaration or is a type error naming the slot. A
    /// non-async arrow is an ordinary closure value and lowers as one; a
    /// dynamic call keeps that shape too, since its slots carry no contract to
    /// decide with.
    pub(super) fn lower_call_argument(&mut self, arg: &Expr) -> Result<LashExpr, Diagnostic> {
        if let Expr::Function(function) = arg
            && function.is_async
        {
            return self.lower_process_literal_arrow(function, None);
        }
        self.lower_expr(arg)
    }
    fn lower_member_call(
        &mut self,
        callee: &Expr,
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        // A boundary dropped an unbound name for holding a function: a
        // method call on it is refused by name before the built-in surface
        // fast paths can answer.
        if let Expr::Ident(name, _) = object {
            self.refuse_expired_global_read(name)?;
        }
        if matches!(object, Expr::Ident(name, _) if name == "crypto")
            && method == "randomUUID"
            && !self.has_binding("crypto")
            && !self.module_authority_roots.contains("crypto")
        {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: crypto.randomUUID. Use a journaled host tool that returns an identifier.",
                None,
            ));
        }
        if matches!(method, "then" | "catch" | "finally") {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: Promise chaining with .then/.catch/.finally. Use direct await and try/catch/finally.",
                None,
            ));
        }
        if matches!(object, Expr::Ident(name, _) if name == "Promise")
            && !self.has_binding("Promise")
        {
            match method {
                "resolve" | "reject" => {
                    return Err(Diagnostic::refusal(
                        DiagnosticCode::MethodUnsupported,
                        format!(
                            "Unsupported: Promise.{method}. Await values directly and use throw/try-catch for failures."
                        ),
                        None,
                    ));
                }
                "all" | "allSettled" | "race" | "any" if self.position.await_depth == 0 => {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AwaitRequired,
                        format!("Promise.{method} must be awaited directly"),
                        None,
                    ));
                }
                _ => {}
            }
        }
        if matches!(object, Expr::Ident(name, _) if name == "JSON") && !self.has_binding("JSON") {
            if method == "parse" {
                if args.len() > 2 {
                    return Err(Diagnostic::defect(
                        DiagnosticCode::MethodUnsupported,
                        "JSON.parse expects text and an optional reviver",
                        None,
                    )
                    .with_hint("call JSON.parse(text) or JSON.parse(text, reviver)"));
                }
                if let [text, reviver] = args {
                    return self.lower_json_parse(text, reviver);
                }
            }
            if method == "stringify" {
                if args.len() > 3 {
                    return Err(Diagnostic::defect(DiagnosticCode::MethodUnsupported, "JSON.stringify expects value, optional replacer, and optional space", None).with_hint("call JSON.stringify(value), JSON.stringify(value, replacer), or JSON.stringify(value, replacer, space)"));
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
                let function_replacer =
                    replacer.filter(|replacer| !matches!(replacer, Expr::Null | Expr::Array(_)));
                let property_replacer = function_replacer.is_none().then_some(replacer).flatten();
                return self.lower_json_stringify(
                    value,
                    function_replacer,
                    property_replacer,
                    args.get(2),
                );
            }
        }
        if let Some(replacement) = match method {
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
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                format!(
                    "Unsupported: Date.{method} is host-timezone dependent. Use d.{replacement}()."
                ),
                None,
            ));
        }
        if matches!(
            method,
            "setUTCFullYear"
                | "setUTCMonth"
                | "setUTCDate"
                | "setUTCHours"
                | "setUTCMinutes"
                | "setUTCSeconds"
                | "setUTCMilliseconds"
        ) {
            return Err(Diagnostic::new(
                DiagnosticCode::DateImmutable,
                format!(
                    "Unsupported: Date.{method}; durable Date values are immutable. Use new Date(d.getTime() + n)."
                ),
                None,
            ));
        }
        if matches!(
            method,
            "toDateString"
                | "toTimeString"
                | "toGMTString"
                | "toLocaleDateString"
                | "toLocaleTimeString"
        ) {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                format!(
                    "Unsupported: Date.{method} is timezone/locale dependent. Use Date.toISOString()."
                ),
                None,
            ));
        }
        if method == "localeCompare" {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: localeCompare/Intl ordering is host-dependent. Use (a < b ? -1 : a > b ? 1 : 0).",
                None,
            ));
        }
        if method == "normalize" {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: String.normalize depends on Unicode normalization data outside the pinned v1 VM. Normalize text in a deterministic host tool before the cell.",
                None,
            ));
        }
        if method == "toLocaleString" {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                "Unsupported: toLocaleString/Intl formatting is locale-dependent. For Date use d.toISOString(); for numbers use toFixed(digits); otherwise build the deterministic string explicitly.",
                None,
            ));
        }
        // A registered `console` module root takes authority over the
        // observation shim, the same way a cell-local `console` binding does:
        // without it the special case silently swallowed the host's binding
        // (FIG-1483). The ECMA-global special cases stay root-blind on purpose
        // — a host module named `Math` still loses to the stdlib surface,
        // which is why RLM registration refuses those roots outright.
        if matches!(object, Expr::Ident(name, _) if name == "console")
            && matches!(method, "log" | "warn" | "error" | "info" | "debug")
            && !self.module_authority_roots.contains("console")
        {
            if !self.has_binding("console") {
                // The arguments reach the substrate untouched. Joining them
                // here with `+` would coerce every object to `"[object Object]"`
                // before the observation was written, which is the one thing the
                // inspect step must not do; `__consoleObservationText` owns the
                // rendering instead (FIG-2767).
                let arguments = args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok(LashExpr::Print(Box::new(console_observation_text(
                    arguments,
                ))));
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
        if matches!(object, Expr::Ident(name, _) if name == "Date")
            && method == "now"
            && args.is_empty()
            && !self.has_binding("Date")
        {
            return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                lashlang::LANGUAGE_RUNTIME_NOW_OPERATION,
            ))));
        }
        if matches!(object, Expr::Ident(name, _) if name == "Math")
            && method == "random"
            && args.is_empty()
            && !self.has_binding("Math")
        {
            return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION,
            ))));
        }
        let module_root = module_path(object).and_then(|path| path.first().cloned());
        // The reserved value names are never module roots: `NaN.toString(2)`
        // is a Number.prototype call and `arguments.hasOwnProperty(k)` reads
        // the arguments object, not a tool call on a module of that name.
        let receiver_is_module_authority = module_root.as_ref().is_some_and(|root| {
            !matches!(
                root.as_str(),
                "undefined" | "NaN" | "Infinity" | "arguments"
            ) && !self.has_binding(root)
                && !is_ecma_global_namespace(root)
        });
        let receiver_shadows_module_authority = module_root
            .as_deref()
            .filter(|root| self.has_binding(root) && self.module_authority_roots.contains(*root));
        if !receiver_is_module_authority
            && let Some(lowered) = self.lower_regexp_method(object, method, args)?
        {
            return Ok(lowered);
        }
        if !receiver_is_module_authority
            && matches!(method, "entries" | "keys" | "values")
            && static_stdlib_owner(object).is_none()
            && self.position.iterable_sink_depth > 0
        {
            let exotic = match object {
                Expr::New { constructor, .. }
                    if IterableKind::from_constructor(constructor).is_some() =>
                {
                    true
                }
                Expr::Member {
                    property: MemberProperty::Field(field),
                    ..
                } if field == "searchParams" => true,
                Expr::Ident(name, _) => self
                    .binding(name)
                    .is_ok_and(|binding| matches!(binding.role, BindingRole::ExoticIterable(_))),
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
                        args: vec![LashExpr::String(method.into()), variable()],
                    },
                ]));
            }
            let array = match method {
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
                            js_name: None,
                            receiver: None,
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
                            js_name: None,
                            receiver: None,
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
            // A receiver not written as a collection (an alias, a field, a
            // call's result) is an array or a collection only at run time,
            // so it takes the array's iteration only if it is one: a `Map`
            // read as an array answered its `keys()` as `[]` in silence.
            let iteration = LashExpr::If {
                condition: Box::new(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![LashExpr::String("Array.isArray".into()), variable()],
                }),
                then_block: Box::new(array),
                else_block: Box::new(LashExpr::BuiltinCall {
                    name: "__typescript_stdlib".into(),
                    args: vec![LashExpr::String(method.into()), variable()],
                }),
            };
            return Ok(LashExpr::Block(vec![
                LashExpr::Assign {
                    target: AssignTarget::variable(receiver.as_str().into()),
                    expr: Box::new(receiver_value),
                },
                iteration,
            ]));
        }
        if !receiver_is_module_authority
            && matches!(method, "entries" | "keys" | "values")
            && static_stdlib_owner(object).is_none()
            && self.position.iterable_sink_depth == 0
        {
            return Err(Diagnostic::refusal(DiagnosticCode::MethodUnsupported, "Unsupported: iterator methods may only be consumed directly by for-of / spread / Array.from / new Map|Set / Object.fromEntries", None).with_hint("wrap it at the point of use: `[...expr]`"));
        }
        if matches!(object, Expr::Ident(name, _) if name == "Array")
            && method == "from"
            && !self.has_binding("Array")
        {
            let (value, mapping_args) = match args {
                [value] => (value, &[][..]),
                [value, callback] => (value, std::slice::from_ref(callback)),
                #[expect(
                    clippy::expect_used,
                    reason = "the slice pattern binds exactly three arguments, so the tail range is in bounds"
                )]
                [value, _callback, _this_arg] => {
                    (value, args.get(1..).expect("mapping arguments exist"))
                }
                _ => {
                    return Err(Diagnostic::defect(
                        DiagnosticCode::MethodUnsupported,
                        "Array.from expects a source and optional mapping callback",
                        None,
                    )
                    .with_hint("call Array.from(source) or Array.from(source, (item) => ...)"));
                }
            };
            // A mapper walks an array source live (the array iterator reads
            // each index and the length at every step); a plain copy has no
            // guest code to observe the difference.
            let source = if mapping_args.is_empty() {
                "Lash.ArrayFromIterable"
            } else {
                "Lash.ArrayIterationSource"
            };
            let array = LashExpr::BuiltinCall {
                name: "__typescript_stdlib".into(),
                args: vec![
                    LashExpr::String(source.into()),
                    self.lower_iterable_sink(value)?,
                ],
            };
            return if mapping_args.is_empty() {
                Ok(array)
            } else {
                self.lower_array_from_mapping(array, mapping_args)
            };
        }
        if matches!(object, Expr::Ident(name, _) if name == "Object")
            && method == "fromEntries"
            && !self.has_binding("Object")
        {
            let [value] = args else {
                return Err(Diagnostic::defect(
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
        if !receiver_is_module_authority && method == "hasOwnProperty" {
            let [key] = args else {
                return Err(Diagnostic::defect(
                    DiagnosticCode::UnsupportedExpression,
                    "hasOwnProperty expects exactly one key",
                    None,
                )
                .with_hint("use Object.hasOwn(object, key)"));
            };
            let own_check = super::array_callbacks::may_be_plain_object(object);
            let receiver = self.temporary("has_own_receiver");
            let key_value = self.temporary("has_own_key");
            let variable = |name: &str| LashExpr::Variable(name.into());
            let builtin = LashExpr::BuiltinCall {
                name: "__typescript_stdlib".into(),
                args: vec![
                    LashExpr::String("Object.hasOwn".into()),
                    variable(&receiver),
                    variable(&key_value),
                ],
            };
            // A plain object's own `hasOwnProperty` is its method.
            let call = if own_check {
                LashExpr::If {
                    condition: Box::new(LashExpr::BuiltinCall {
                        name: "__typescript_stdlib".into(),
                        args: vec![
                            LashExpr::String("Lash.OwnMethod".into()),
                            variable(&receiver),
                            LashExpr::String("hasOwnProperty".into()),
                        ],
                    }),
                    then_block: Box::new(LashExpr::MethodCall {
                        receiver: Box::new(variable(&receiver)),
                        method: MethodKey::Field("hasOwnProperty".into()),
                        args: vec![variable(&key_value)],
                    }),
                    else_block: Box::new(builtin),
                }
            } else {
                builtin
            };
            return Ok(LashExpr::Block(vec![
                LashExpr::Assign {
                    target: AssignTarget::variable(receiver.as_str().into()),
                    expr: Box::new(self.lower_expr(object)?),
                },
                LashExpr::Assign {
                    target: AssignTarget::variable(key_value.as_str().into()),
                    expr: Box::new(self.lower_expr(key)?),
                },
                call,
            ]));
        }
        if !receiver_is_module_authority
            && method == "replace"
            && let [needle, callback @ Expr::Function(_)] = args
        {
            return self.lower_string_replace_callback(object, needle, callback);
        }
        if matches!(object, Expr::Ident(owner, _) if matches!(owner.as_str(), "Object" | "Map"))
            && method == "groupBy"
            && !self.has_binding(match object {
                Expr::Ident(owner, _) => owner,
                _ => unreachable!(),
            })
        {
            let [source, callback] = args else {
                return Err(Diagnostic::defect(
                    DiagnosticCode::UnsupportedExpression,
                    "groupBy expects an iterable and one callback",
                    None,
                ));
            };
            let Expr::Ident(owner, _) = object else {
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
            && !matches!(object, Expr::Object(_))
            && !literal_supports_instance_method(object, method)
        {
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                format!("method `{method}` is unavailable on this literal receiver"),
                None,
            ));
        }
        // Callback methods stay entirely inside the VM. The synchronous
        // family shares the effect-rejecting callback frame; async `map`
        // retains the durable sequential async-map path.
        if !receiver_is_module_authority
            && method == "map"
            && matches!(args, [Expr::Function(function)] if function.is_async)
        {
            return self.lower_array_map(object, args);
        }
        // A collection's own `forEach` takes no receiver argument; with a
        // `thisArg` it goes through the array-callback lowering, whose
        // collection branch binds the receiver.
        let receiver_is_callback_exotic = method == "forEach"
            && args.len() < 2
            && match object {
                Expr::New { constructor, .. } => {
                    IterableKind::from_constructor(constructor).is_some()
                }
                Expr::Ident(name, _) => self
                    .binding(name)
                    .is_ok_and(|binding| matches!(binding.role, BindingRole::ExoticIterable(_))),
                _ => false,
            };
        if !receiver_is_module_authority
            && (!receiver_is_callback_exotic
                && matches!(
                    method,
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
                    && matches!(method, "sort" | "toSorted")
                    && args.first().is_some_and(
                        |argument| !matches!(argument, Expr::Ident(name, _) if name == "undefined"),
                    ))
        {
            return self.lower_array_callback_method(method, object, args);
        }
        if !receiver_is_module_authority && is_instance_stdlib_method(method) {
            let mut builtin_args = vec![LashExpr::String(method.into()), self.lower_expr(object)?];
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
            && receiver_is_module_authority
        {
            #[expect(
                clippy::expect_used,
                reason = "`receiver_is_module_authority` is true only when `module_path(object)` already resolved"
            )]
            return Ok(LashExpr::ReceiverCall {
                receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                    module_path(object)
                        .expect("constructor path checked above")
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                ))),
                operation: method.into(),
                args: args
                    .iter()
                    .map(|arg| self.lower_expr(arg))
                    .collect::<Result<_, _>>()?,
            });
        }

        // A method of the program's own objects: any name that is not a
        // built-in prototype method. A built-in name keeps the surface
        // classification below, so an unsupported built-in stays a named
        // refusal rather than a runtime TypeError.
        if self.is_own_method_call(object, method) {
            let receiver = self.lower_expr(object)?;
            return self.lower_method_call(receiver, MethodKey::Field(method.into()), args);
        }

        // Classify by method name before the tool-call branch. A receiver
        // that is not a module authority — a chained call, a local binding,
        // a computed member, a literal — can never dispatch a tool, so an
        // unadvertised method there is a missing method and must say so.
        // Falling through reported it as a tool call needing `await`, and
        // under `await` it lowered and failed at the host untyped.
        let ecma_owner = match object {
            Expr::Ident(owner, _)
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
            let message = if let Some(root) = receiver_shadows_module_authority {
                format!(
                    "local binding `{root}` shadows module `{root}`; rename the binding or call the module before binding"
                )
            } else {
                let name = match ecma_owner {
                    Some(owner) => format!("{owner}.{method}"),
                    None => method.to_string(),
                };
                format!("method `{name}` is not in the TypeScript runtime surface")
            };
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
                message,
                None,
            ));
        }

        #[expect(
            clippy::expect_used,
            reason = "`receiver_is_module_authority` is true only when `module_path(object)` already resolved"
        )]
        let receiver = if receiver_is_module_authority {
            LashExpr::ResourceRef(ResourceRefExpr::unresolved(
                module_path(object)
                    .expect("checked by receiver_is_module_authority")
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ))
        } else {
            self.lower_expr(object)?
        };
        // `registerTrigger` was the retired global spelling of
        // `triggers.register`, and `update`/`revive` take the same registration
        // record, so the `inputs` template is erased on all three remaining
        // paths. Retiring the event binding for one of them would strand the
        // other two.
        let lowered_args = if receiver_is_module_authority
            && matches!(object, Expr::Ident(root, _) if root == "triggers")
            && is_trigger_registration_operation(method)
            && let [config] = args
        {
            vec![self.lower_trigger_config(config)?]
        } else {
            args.iter()
                .map(|arg| self.lower_call_argument(arg))
                .collect::<Result<_, _>>()?
        };
        let call = LashExpr::ReceiverCall {
            receiver: Box::new(receiver),
            operation: method.into(),
            args: lowered_args,
        };
        Ok(if self.position.await_depth == 0 {
            LashExpr::BuiltinCall {
                name: "__typescript_pending_tool".into(),
                args: vec![call],
            }
        } else {
            call
        })
    }
}

/// Lowers a `console.*` argument list to the substrate call that renders it.
///
/// `console.log` is the inspect step the RLM prompt tells a cell to use, so its
/// text has to describe the value. Handing the raw arguments to
/// `__consoleObservationText` keeps the rendering in the one place that can see
/// the heap — plain objects and arrays become JSON, everything else keeps
/// JavaScript's coercion — instead of flattening them to `"[object Object]"`
/// here (FIG-2767).
///
/// The method name is spelled by hand on both sides of the seam; a drift makes
/// the substrate reject the call rather than answer it quietly.
fn console_observation_text(mut arguments: Vec<LashExpr>) -> LashExpr {
    arguments.insert(0, LashExpr::String("__consoleObservationText".into()));
    LashExpr::BuiltinCall {
        name: "__typescript_stdlib".into(),
        args: arguments,
    }
}
