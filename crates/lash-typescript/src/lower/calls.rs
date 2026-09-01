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
            "finish" => Some(Self::AgentPrimitive(AgentPrimitive::Finish)),
            "print" => Some(Self::AgentPrimitive(AgentPrimitive::Print)),
            "wake" => Some(Self::AgentPrimitive(AgentPrimitive::Wake)),
            "sleep" => Some(Self::AgentPrimitive(AgentPrimitive::Sleep)),
            "waitSignal" => Some(Self::AgentPrimitive(AgentPrimitive::WaitSignal)),
            "start" => Some(Self::AgentPrimitive(AgentPrimitive::Start)),
            "registerTrigger" => Some(Self::AgentPrimitive(AgentPrimitive::RegisterTrigger)),
            "defineProcess" => Some(Self::AgentPrimitive(AgentPrimitive::DefineProcess)),
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
    Wake,
    Sleep,
    WaitSignal,
    Start,
    RegisterTrigger,
    DefineProcess,
}

impl AgentPrimitive {
    fn name(self) -> &'static str {
        match self {
            Self::Finish => "finish",
            Self::Print => "print",
            Self::Wake => "wake",
            Self::Sleep => "sleep",
            Self::WaitSignal => "waitSignal",
            Self::Start => "start",
            Self::RegisterTrigger => "registerTrigger",
            Self::DefineProcess => "defineProcess",
        }
    }
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
    pub(super) fn lower_call(
        &mut self,
        callee: &Expr,
        args: &[CallArg],
    ) -> Result<LashExpr, Diagnostic> {
        let Some(args) = normalize_call_args(args) else {
            let callee = self.lower_expr(callee)?;
            return self.lower_dynamic_call_value(callee, args);
        };
        match self.classify_callee(callee) {
            CalleeFamily::UnboundGlobal(name) => {
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
            Expr::Ident(name) if !self.has_binding(name) => CalleeFamily::UnboundGlobal(name),
            Expr::Member {
                object,
                property: MemberProperty::Field(method),
            } => CalleeFamily::Member { object, method },
            Expr::Ident(name)
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
            (CoercionBuiltin::String, Some(value)) => js_add(LashExpr::String("".into()), value),
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
            .with_hint("return from defineProcess.run so enclosing finally blocks execute")),
            (AgentPrimitive::Finish, [value]) => {
                Ok(LashExpr::Finish(Box::new(self.lower_expr(value)?)))
            }
            (AgentPrimitive::Print, [value]) => {
                Ok(LashExpr::Print(Box::new(self.lower_expr(value)?)))
            }
            (AgentPrimitive::Wake, [value]) => {
                Ok(LashExpr::Wake(Box::new(self.lower_expr(value)?)))
            }
            (AgentPrimitive::Wake, [run, Expr::String(signal), payload]) => {
                Ok(LashExpr::SignalRun {
                    run: Box::new(self.lower_expr(run)?),
                    name: signal.as_str().into(),
                    payload: Box::new(self.lower_expr(payload)?),
                })
            }
            (AgentPrimitive::Sleep, [milliseconds]) if self.position.await_depth > 0 => {
                Ok(LashExpr::SleepFor(Box::new(self.lower_expr(milliseconds)?)))
            }
            (AgentPrimitive::WaitSignal, [Expr::String(name)]) if self.position.await_depth > 0 => {
                Ok(LashExpr::WaitSignal {
                    name: name.as_str().into(),
                })
            }
            (AgentPrimitive::Start, [Expr::Ident(target)]) => self.lower_start(target, &[]),
            (AgentPrimitive::Start, [Expr::Ident(target), Expr::Object(entries)]) => {
                self.lower_start(target, entries)
            }
            (AgentPrimitive::RegisterTrigger, [config]) if self.position.await_depth > 0 => {
                Ok(LashExpr::ReceiverCall {
                    receiver: Box::new(LashExpr::ResourceRef(ResourceRefExpr::unresolved(vec![
                        "triggers".into(),
                    ]))),
                    operation: "register".into(),
                    args: vec![self.lower_expr(config)?],
                })
            }
            (AgentPrimitive::DefineProcess, _) => Err(Diagnostic::new(
                DiagnosticCode::ProcessDefinitionNotTopLevel,
                "defineProcess must initialize a top-level binding",
                None,
            )),
            (
                AgentPrimitive::Sleep
                | AgentPrimitive::WaitSignal
                | AgentPrimitive::RegisterTrigger,
                _,
            ) if self.position.await_depth == 0 => Err(Diagnostic::new(
                DiagnosticCode::AwaitRequired,
                format!("agent primitive `{}` requires await", primitive.name()),
                None,
            )),
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
        Ok(LashExpr::Call {
            function: Box::new(self.lower_expr(callee)?),
            args: args
                .iter()
                .map(|arg| self.lower_expr(arg))
                .collect::<Result<_, _>>()?,
        })
    }
    fn lower_member_call(
        &mut self,
        callee: &Expr,
        object: &Expr,
        method: &str,
        args: &[Expr],
    ) -> Result<LashExpr, Diagnostic> {
        if matches!(object, Expr::Ident(name) if name == "crypto")
            && method == "randomUUID"
            && !self.has_binding("crypto")
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
        if matches!(object, Expr::Ident(name) if name == "Promise") && !self.has_binding("Promise")
        {
            match method {
                "race" | "any" => {
                    return Err(Diagnostic::refusal(
                        DiagnosticCode::MethodUnsupported,
                        format!(
                            "Unsupported: Promise.{method} requires durable partial-settlement ordering (FIG-1416). Use Promise.all/Promise.allSettled, or await durable sleep for timeout patterns."
                        ),
                        None,
                    ));
                }
                "resolve" | "reject" => {
                    return Err(Diagnostic::refusal(
                        DiagnosticCode::MethodUnsupported,
                        format!(
                            "Unsupported: Promise.{method}. Await values directly and use throw/try-catch for failures."
                        ),
                        None,
                    ));
                }
                "all" | "allSettled" if self.position.await_depth == 0 => {
                    return Err(Diagnostic::new(
                        DiagnosticCode::AwaitRequired,
                        format!("Promise.{method} must be awaited directly"),
                        None,
                    ));
                }
                _ => {}
            }
        }
        if matches!(object, Expr::Ident(name) if name == "JSON") && !self.has_binding("JSON") {
            if method == "parse" && args.len() > 1 {
                return Err(reject_json_parse_reviver());
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
                let function_replacer = replacer.filter(|replacer| {
                    !matches!(replacer, Expr::Null | Expr::Undefined | Expr::Array(_))
                });
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
            return Err(Diagnostic::refusal(
                DiagnosticCode::MethodUnsupported,
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
                | "toUTCString"
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
        if matches!(object, Expr::Ident(name) if name == "console")
            && matches!(method, "log" | "warn" | "error" | "info" | "debug")
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
        if matches!(object, Expr::Ident(name) if name == "Date")
            && method == "now"
            && args.is_empty()
            && !self.has_binding("Date")
        {
            return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                "now",
            ))));
        }
        if matches!(object, Expr::Ident(name) if name == "Math")
            && method == "random"
            && args.is_empty()
            && !self.has_binding("Math")
        {
            return Ok(LashExpr::ResultUnwrap(Box::new(journaled_runtime_call(
                "random",
            ))));
        }
        let module_root = module_path(object).and_then(|path| path.first().cloned());
        let receiver_is_module_authority = module_root
            .as_ref()
            .is_some_and(|root| !self.has_binding(root) && !is_ecma_global_namespace(root));
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
                Expr::Ident(name) => self
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
        if !receiver_is_module_authority
            && matches!(method, "entries" | "keys" | "values")
            && static_stdlib_owner(object).is_none()
            && self.position.iterable_sink_depth == 0
        {
            return Err(Diagnostic::refusal(DiagnosticCode::MethodUnsupported, "Unsupported: iterator methods may only be consumed directly by for-of / spread / Array.from / new Map|Set / Object.fromEntries", None).with_hint("wrap it at the point of use: `[...expr]`"));
        }
        if matches!(object, Expr::Ident(name) if name == "Array")
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
                    return Err(Diagnostic::defect(
                        DiagnosticCode::MethodUnsupported,
                        "Array.from expects a source and optional mapping callback",
                        None,
                    )
                    .with_hint("call Array.from(source) or Array.from(source, (item) => ...)"));
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
        if matches!(object, Expr::Ident(name) if name == "Object")
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
            return Ok(LashExpr::BuiltinCall {
                name: "__typescript_stdlib".into(),
                args: vec![
                    LashExpr::String("Object.hasOwn".into()),
                    self.lower_expr(object)?,
                    self.lower_expr(key)?,
                ],
            });
        }
        if !receiver_is_module_authority
            && method == "replace"
            && let [needle, callback @ Expr::Function(_)] = args
        {
            return self.lower_string_replace_callback(object, needle, callback);
        }
        if matches!(object, Expr::Ident(owner) if matches!(owner.as_str(), "Object" | "Map"))
            && method == "groupBy"
            && !self.has_binding(match object {
                Expr::Ident(owner) => owner,
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
            let Expr::Ident(owner) = object else {
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
        let receiver_is_callback_exotic = method == "forEach"
            && match object {
                Expr::New { constructor, .. } => {
                    IterableKind::from_constructor(constructor).is_some()
                }
                Expr::Ident(name) => self
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
                    && args
                        .first()
                        .is_some_and(|argument| !matches!(argument, Expr::Undefined)))
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

        // Classify by method name before the tool-call branch. A receiver
        // that is not a module authority — a chained call, a local binding,
        // a computed member, a literal — can never dispatch a tool, so an
        // unadvertised method there is a missing method and must say so.
        // Falling through reported it as a tool call needing `await`, and
        // under `await` it lowered and failed at the host untyped.
        let ecma_owner = match object {
            Expr::Ident(owner) if is_ecma_global_namespace(owner) && !self.has_binding(owner) => {
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

        if self.position.await_depth == 0 {
            return Err(Diagnostic::new(
                DiagnosticCode::AwaitRequired,
                format!("tool call `{method}` must appear under await or Promise.all/allSettled"),
                None,
            ));
        }
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
        Ok(LashExpr::ReceiverCall {
            receiver: Box::new(receiver),
            operation: method.into(),
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
        // `start` resolves its target through the scope stack like every other
        // read, so a nearer binding of the same name — a parameter, a block
        // local — is what the author wrote, and it is not the process.
        let process = match self.binding(target).map(|binding| &binding.role) {
            Ok(BindingRole::ProcessDefinition(process)) => process.clone(),
            _ => {
                return Err(Diagnostic::new(
                    DiagnosticCode::ProcessTargetStaticRequired,
                    format!("`{target}` is not a top-level defineProcess binding"),
                    None,
                ));
            }
        };
        Ok(LashExpr::StartProcess(ProcessStartExpr {
            process: process.into(),
            args: entries
                .iter()
                .map(|property| match property {
                    ObjectProperty::KeyValue(PropertyKey::Static(name), value) => {
                        Ok((name.as_str().into(), self.lower_expr(value)?))
                    }
                    _ => Err(Diagnostic::refusal(
                        DiagnosticCode::UnsupportedExpression,
                        "start arguments require static properties without spread",
                        None,
                    )),
                })
                .collect::<Result<_, Diagnostic>>()?,
        }))
    }
}
