//! Native syntax with generated-document semantics and direct exact inverses.
use super::{Reader, array, call_arguments, invalid, string};
use crate::{Diagnostic, generated_operators::Operator};
use lash_kernel_doc as k;
use swc_common::Spanned;
use swc_ecma_ast as s;

pub(super) fn path(expr: &s::Expr) -> Option<String> {
    match expr {
        s::Expr::Ident(ident) => Some(ident.sym.to_string()),
        s::Expr::Member(member) => {
            let s::MemberProp::Ident(field) = &member.prop else {
                return None;
            };
            Some(format!("{}.{}", path(&member.obj)?, field.sym))
        }
        _ => None,
    }
}
fn call(expr: &s::Expr) -> Result<(&s::Expr, Vec<&s::Expr>), Diagnostic> {
    let s::Expr::Call(call) = expr else {
        return Err(invalid("expected a native call", Some(expr.span())));
    };
    let s::Callee::Expr(callee) = &call.callee else {
        return Err(invalid("expected a native callee", Some(expr.span())));
    };
    Ok((callee, call_arguments(call)?))
}

pub(super) enum NativeAction {
    Nested(k::ActExpr),
    Statement(k::Action),
}
impl NativeAction {
    pub(super) fn expression(self) -> Result<k::Expr, Diagnostic> {
        match self {
            Self::Nested(act) => Ok(k::Expr::Act(Box::new(act))),
            Self::Statement(_) => Err(invalid("yield and cancel cannot nest", None)),
        }
    }
    pub(super) fn rhs(self) -> k::Rhs {
        match self {
            Self::Nested(act) => match act.as_action() {
                Some(action) => k::Rhs::Action(action),
                None => k::Rhs::Expr(k::Expr::Act(Box::new(act))),
            },
            Self::Statement(action) => k::Rhs::Action(action),
        }
    }
    fn statement(self) -> Result<k::Stmt, Diagnostic> {
        match self.rhs() {
            k::Rhs::Action(action) => Ok(k::Stmt::Do { action }),
            _ => Err(invalid("a discarded nested action needs a binding", None)),
        }
    }
}
impl Reader {
    pub(super) fn native_action(
        &mut self,
        expr: &s::Expr,
    ) -> Option<Result<NativeAction, Diagnostic>> {
        let (expr, annotation) = if let s::Expr::TsAs(assertion) = expr {
            (&*assertion.expr, Some(&*assertion.type_ann))
        } else {
            (expr, None)
        };
        if let s::Expr::Await(awaited) = expr {
            if super::expressions::async_scope(&awaited.arg) {
                return None;
            }
            return Some(self.awaited_action(&awaited.arg, annotation));
        }
        if annotation.is_none()
            && let Some(action) = self.authored_action(expr)
        {
            return Some(action.map(|action| {
                let k::Action::Call { callee, args } = action else {
                    unreachable!()
                };
                NativeAction::Nested(k::ActExpr {
                    kind: k::ActKind::Call(callee),
                    args: args
                        .into_iter()
                        .map(|atom| match atom {
                            k::Atom::Variable(name) => k::Expr::Variable(name),
                            k::Atom::Literal(value) => k::Expr::Literal(value),
                        })
                        .collect(),
                })
            }));
        }
        if matches!(expr, s::Expr::Call(call) if matches!(&call.callee, s::Callee::Expr(callee) if path(callee).as_deref() == Some("spawn")))
        {
            return Some((|| {
                if annotation.is_some() {
                    return Err(invalid(
                        "only effects have result annotations",
                        Some(expr.span()),
                    ));
                }
                let (_, args) = call(expr)?;
                let [callee, args] = args.as_slice() else {
                    return Err(invalid(
                        "spawn names a callee and argument array",
                        Some(expr.span()),
                    ));
                };
                Ok(NativeAction::Nested(k::ActExpr {
                    kind: k::ActKind::Spawn(self.native_callee(callee)?),
                    args: array(args)?
                        .into_iter()
                        .map(|arg| self.expression(arg))
                        .collect::<Result<_, _>>()?,
                }))
            })());
        }
        None
    }
    /// A library action spelled as an author writes it: `console.log(...args)`
    /// or `target.name(...args)` (generated_operators::Authored). None when
    /// the expression is not one; the base document must hold the helper once.
    fn authored_action(&mut self, expr: &s::Expr) -> Option<Result<k::Action, Diagnostic>> {
        let s::Expr::Call(call) = expr else {
            return None;
        };
        let s::Callee::Expr(callee) = &call.callee else {
            return None;
        };
        let s::Expr::Member(member) = callee.as_ref() else {
            return None;
        };
        let s::MemberProp::Ident(name) = &member.prop else {
            return None;
        };
        let s::Expr::Ident(target) = member.obj.as_ref() else {
            return None;
        };
        let [argument] = call.args.as_slice() else {
            return None;
        };
        argument.spread?;
        let (label, console) = if target.sym == "console" && name.sym == "log" {
            ("ts.console.log", true)
        } else {
            ("ts.call_member", false)
        };
        let function = self.operators.unique(label)?;
        Some((|| {
            let args = self.atom(&argument.expr)?;
            Ok(k::Action::Call {
                callee: k::Callee::Library(function),
                args: if console {
                    vec![k::Atom::Literal(k::Literal::Absent), args]
                } else {
                    vec![
                        k::Atom::Variable(self.name(target)?),
                        k::Atom::Literal(k::Literal::Text(name.sym.to_string())),
                        args,
                    ]
                },
            })
        })())
    }
    fn native_callee(&self, expr: &s::Expr) -> Result<k::Callee, Diagnostic> {
        match expr {
            s::Expr::Ident(ident) if self.helpers.contains_key(ident.sym.as_ref()) => {
                Ok(k::Callee::Library(self.id(expr)?))
            }
            s::Expr::Ident(ident) => Ok(k::Callee::Declared(self.name(ident)?)),
            s::Expr::Paren(paren) => {
                let s::Expr::Ident(ident) = &*paren.expr else {
                    return Err(invalid(
                        "value callee is one parenthesized binding",
                        Some(expr.span()),
                    ));
                };
                Ok(k::Callee::Value(self.name(ident)?))
            }
            _ => Err(invalid(
                "expected declared, value or helper callee",
                Some(expr.span()),
            )),
        }
    }
    fn awaited_action(
        &mut self,
        expr: &s::Expr,
        annotation: Option<&s::TsType>,
    ) -> Result<NativeAction, Diagnostic> {
        if let s::Expr::Await(inner) = expr {
            let NativeAction::Nested(mut act) = self.awaited_action(&inner.arg, annotation)? else {
                return Err(invalid(
                    "only a tool call or sleep is awaited twice",
                    Some(expr.span()),
                ));
            };
            match &mut act.kind {
                k::ActKind::Perform { then_yield, .. } | k::ActKind::Sleep { then_yield }
                    if !*then_yield =>
                {
                    *then_yield = true
                }
                _ => {
                    return Err(invalid(
                        "only a tool call or sleep is awaited twice",
                        Some(expr.span()),
                    ));
                }
            }
            return Ok(NativeAction::Nested(act));
        }
        if !matches!(expr, s::Expr::Call(_)) {
            if annotation.is_some() {
                return Err(invalid(
                    "only effects have result annotations",
                    Some(expr.span()),
                ));
            }
            let policy = self.operators.unique("ts.await").ok_or_else(|| {
                invalid(
                    "`await` of a value needs the base's one `ts.await`",
                    Some(expr.span()),
                )
            })?;
            return Ok(NativeAction::Nested(k::ActExpr {
                kind: k::ActKind::Await(policy),
                args: vec![self.expression(expr)?],
            }));
        }
        let (callee, args) = call(expr)?;
        let spelling = path(callee);
        if spelling.as_deref() == Some("k.await") {
            let [policy, value] = args.as_slice() else {
                return Err(invalid("wrong native action arity", Some(expr.span())));
            };
            if annotation.is_some() {
                return Err(invalid(
                    "only effects have result annotations",
                    Some(expr.span()),
                ));
            }
            return Ok(NativeAction::Nested(k::ActExpr {
                kind: k::ActKind::Await(self.id(policy)?),
                args: vec![self.expression(value)?],
            }));
        }
        let arguments = args
            .iter()
            .map(|arg| self.expression(arg))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(effect) = spelling
            .as_deref()
            .and_then(|name| name.strip_prefix("tools."))
        {
            let effect = k::EffectName::new(effect)
                .map_err(|e| invalid(e.to_string(), Some(expr.span())))?;
            let result = match annotation {
                Some(annotation) => decode_type(annotation, self.limits)?,
                None => k::Type::Any,
            };
            return Ok(NativeAction::Nested(k::ActExpr {
                kind: k::ActKind::Perform {
                    effect,
                    result,
                    then_yield: false,
                },
                args: arguments,
            }));
        }
        if annotation.is_some() {
            return Err(invalid(
                "only effects have result annotations",
                Some(expr.span()),
            ));
        }
        let kind = match (spelling.as_deref(), arguments.as_slice()) {
            (Some("sleep"), [_]) => k::ActKind::Sleep { then_yield: false },
            (Some("join"), [_]) => k::ActKind::Join,
            (Some("cancel"), [_]) => {
                return Ok(NativeAction::Statement(k::Action::Cancel {
                    task: self.atom(args[0])?,
                }));
            }
            (Some("yieldTask"), []) => return Ok(NativeAction::Statement(k::Action::Yield)),
            (
                Some(
                    name @ ("Promise.all" | "Promise.allSettled" | "Promise.race" | "Promise.any"),
                ),
                [_],
            ) => k::ActKind::JoinMany(match name {
                "Promise.all" => k::JoinMode::All,
                "Promise.allSettled" => k::JoinMode::AllSettled,
                "Promise.race" => k::JoinMode::Race,
                _ => k::JoinMode::Any,
            }),
            (
                Some(
                    "sleep" | "join" | "cancel" | "yieldTask" | "Promise.all"
                    | "Promise.allSettled" | "Promise.race" | "Promise.any",
                ),
                _,
            ) => return Err(invalid("wrong native action arity", Some(expr.span()))),
            _ => k::ActKind::Call(self.native_callee(callee)?),
        };
        Ok(NativeAction::Nested(k::ActExpr {
            kind,
            args: arguments,
        }))
    }
    pub(super) fn native_statement(
        &mut self,
        expr: &s::Expr,
    ) -> Option<Result<k::Stmt, Diagnostic>> {
        if let s::Expr::Unary(unary) = expr
            && unary.op == s::UnaryOp::Delete
        {
            return Some((|| {
                let s::Expr::Member(member) = &*unary.arg else {
                    return Err(invalid("delete needs a field or index", Some(expr.span())));
                };
                Ok(k::Stmt::Remove {
                    member: self.native_member(member)?,
                })
            })());
        }
        if let Ok((callee, args)) = call(expr)
            && let Some(name @ ("finish" | "failure" | "console.log")) = path(callee).as_deref()
        {
            return Some((|| {
                let [value] = args.as_slice() else {
                    return Err(invalid(
                        "terminal/print needs one expression",
                        Some(expr.span()),
                    ));
                };
                let value = self.expression(value)?;
                Ok(match name {
                    "finish" => k::Stmt::Finish { value },
                    "failure" => k::Stmt::Fail { value },
                    _ => k::Stmt::Print { value },
                })
            })());
        }
        self.native_action(expr)
            .map(|action| action.and_then(NativeAction::statement))
    }
    pub(super) fn native_expression(
        &mut self,
        expr: &s::Expr,
    ) -> Option<Result<k::Expr, Diagnostic>> {
        match expr {
            s::Expr::Bin(binary) => {
                Some(self.operator(Operator::Binary(binary.op), &[&binary.left, &binary.right]))
            }
            s::Expr::Unary(unary)
                if unary.op == s::UnaryOp::Minus
                    && matches!(&*unary.arg, s::Expr::Ident(ident) if ident.sym == "Infinity") =>
            {
                Some(Ok(k::Expr::Literal(k::Literal::Float(k::Float::new(
                    f64::NEG_INFINITY,
                )))))
            }
            s::Expr::Unary(unary)
                if unary.op == s::UnaryOp::Minus
                    && matches!(&*unary.arg, s::Expr::Lit(s::Lit::BigInt(_))) =>
            {
                Some(big_integer(&unary.arg, true).map(k::Expr::Literal))
            }
            s::Expr::Unary(unary)
                if !(unary.op == s::UnaryOp::Minus
                    && matches!(&*unary.arg, s::Expr::Lit(s::Lit::Num(_)))) =>
            {
                Some(self.operator(Operator::Unary(unary.op), &[&unary.arg]))
            }
            s::Expr::Lit(s::Lit::BigInt(_)) => Some(big_integer(expr, false).map(k::Expr::Literal)),
            s::Expr::Ident(ident)
                if matches!(ident.sym.as_ref(), "undefined" | "NaN" | "Infinity") =>
            {
                Some(Ok(k::Expr::Literal(match ident.sym.as_ref() {
                    "undefined" => k::Literal::Absent,
                    "NaN" => k::Literal::Float(k::Float::new(f64::NAN)),
                    _ => k::Literal::Float(k::Float::new(f64::INFINITY)),
                })))
            }
            s::Expr::Member(member) if matches!(&*member.obj, s::Expr::Ident(ident) if ident.sym == "declared") => {
                Some((|| {
                    let s::MemberProp::Ident(ident) = &member.prop else {
                        return Err(invalid(
                            "declared reference uses an alias",
                            Some(expr.span()),
                        ));
                    };
                    let ident = s::Ident::new(ident.sym.clone(), ident.span, Default::default());
                    Ok(k::Expr::Literal(k::Literal::Function(self.name(&ident)?)))
                })())
            }
            s::Expr::TsConstAssertion(assertion) => Some((|| {
                Ok(k::Expr::Tuple(
                    array(&assertion.expr)?
                        .into_iter()
                        .map(|expr| self.expression(expr))
                        .collect::<Result<_, _>>()?,
                ))
            })()),
            s::Expr::New(new) => Some(self.native_collection(new)),
            s::Expr::Call(_)
                if call(expr).ok().is_some_and(|(callee, _)| {
                    matches!(
                        path(callee).as_deref(),
                        Some("Date.now" | "Math.random" | "projection.read" | "Object.fromEntries")
                    )
                }) =>
            {
                Some((|| {
                    let (callee, args) = call(expr)?;
                    match (path(callee).as_deref(), args.as_slice()) {
                        (Some("Date.now"), []) => Ok(k::Expr::Clock),
                        (Some("Math.random"), []) => Ok(k::Expr::Random),
                        (Some("Object.fromEntries"), [entries]) => Ok(k::Expr::Record(
                            array(entries)?
                                .into_iter()
                                .map(|entry| {
                                    let pair = array(entry)?;
                                    let [key, value] = pair.as_slice() else {
                                        return Err(invalid(
                                            "record entry has a text key and expression",
                                            Some(entry.span()),
                                        ));
                                    };
                                    Ok(k::RecordEntry {
                                        field: string(key)?,
                                        value: self.expression(value)?,
                                    })
                                })
                                .collect::<Result<_, _>>()?,
                        )),
                        (Some("projection.read"), [handle, request]) => {
                            Ok(k::Expr::Read(Box::new(k::ProjectionRead {
                                handle: self.expression(handle)?,
                                request: self.expression(request)?,
                            })))
                        }
                        _ => Err(invalid(
                            "wrong clock/random/projection arity",
                            Some(expr.span()),
                        )),
                    }
                })())
            }
            _ => None,
        }
    }
    fn operator(&mut self, operator: Operator, args: &[&s::Expr]) -> Result<k::Expr, Diagnostic> {
        let args = args
            .iter()
            .map(|arg| self.expression(arg))
            .collect::<Result<Vec<_>, _>>()?;
        let function = self
            .operators
            .id(operator, &args, &self.types)
            .ok_or_else(|| {
                invalid(
                    "operator has no unique full-identity binding in the base manifest",
                    None,
                )
            })?;
        Ok(k::Expr::Call { function, args })
    }
    fn native_collection(&mut self, new: &s::NewExpr) -> Result<k::Expr, Diagnostic> {
        if new.type_args.is_some() {
            return Err(invalid(
                "native collection has no type arguments",
                Some(new.span),
            ));
        }
        let name = path(&new.callee)
            .ok_or_else(|| invalid("collection constructor is named", Some(new.span)))?;
        let args = new
            .args
            .as_ref()
            .ok_or_else(|| invalid("collection constructor has one array", Some(new.span)))?;
        let [argument] = args.as_slice() else {
            return Err(invalid(
                "collection constructor has one array",
                Some(new.span),
            ));
        };
        if argument.spread.is_some() {
            return Err(invalid("collection has no spread", Some(new.span)));
        }
        let elements = array(&argument.expr)?;
        match name.as_str() {
            "Set" => Ok(k::Expr::Set(
                elements
                    .into_iter()
                    .map(|expr| self.expression(expr))
                    .collect::<Result<_, _>>()?,
            )),
            "Map" => Ok(k::Expr::Map(
                elements
                    .into_iter()
                    .map(|expr| {
                        let pair = array(expr)?;
                        let [key, value] = pair.as_slice() else {
                            return Err(invalid(
                                "map entry has two expressions",
                                Some(expr.span()),
                            ));
                        };
                        Ok(k::MapEntry {
                            key: self.expression(key)?,
                            value: self.expression(value)?,
                        })
                    })
                    .collect::<Result<_, _>>()?,
            )),
            "Uint8Array" => {
                let bytes: Vec<u8> = elements
                    .into_iter()
                    .map(|expr| {
                        let k::Expr::Literal(k::Literal::Int(value)) = self.expression(expr)?
                        else {
                            return Err(invalid(
                                "byte array needs integer literals",
                                Some(expr.span()),
                            ));
                        };
                        value
                            .to_string()
                            .parse::<u8>()
                            .map_err(|_| invalid("byte out of range", Some(expr.span())))
                    })
                    .collect::<Result<_, _>>()?;
                Ok(k::Expr::Literal(k::Literal::Bytes(k::Bytes::new(bytes))))
            }
            _ => Err(invalid("unknown native collection", Some(new.span))),
        }
    }
}
fn big_integer(expr: &s::Expr, negative: bool) -> Result<k::Literal, Diagnostic> {
    let s::Expr::Lit(s::Lit::BigInt(value)) = expr else {
        return Err(invalid("expected bigint literal", Some(expr.span())));
    };
    let raw = value
        .raw
        .as_ref()
        .and_then(|raw| raw.strip_suffix('n'))
        .ok_or_else(|| invalid("bigint needs canonical decimal token", Some(expr.span())))?;
    let text = if negative {
        format!("-{raw}")
    } else {
        raw.to_owned()
    };
    Ok(k::Literal::Int(
        k::Integer::parse(&text).map_err(|e| invalid(e.to_string(), Some(expr.span())))?,
    ))
}

fn decode_type(ty: &s::TsType, limits: k::KernelDataLimits) -> Result<k::Type, Diagnostic> {
    let s::TsType::TsTypeRef(reference) = ty else {
        return Err(invalid(
            "result assertion needs KernelType metadata",
            Some(ty.span()),
        ));
    };
    let s::TsEntityName::Ident(name) = &reference.type_name else {
        return Err(invalid("result type is KernelType", Some(ty.span())));
    };
    if name.sym != "KernelType" {
        return Err(invalid("result type is KernelType", Some(ty.span())));
    }
    let params = reference
        .type_params
        .as_ref()
        .ok_or_else(|| invalid("result type needs complete metadata", Some(ty.span())))?;
    let [metadata] = params.params.as_slice() else {
        return Err(invalid(
            "result type needs one complete metadata literal",
            Some(ty.span()),
        ));
    };
    super::decode(&type_text(metadata)?, limits)
}
fn type_text(ty: &s::TsType) -> Result<String, Diagnostic> {
    Ok(match ty {
        s::TsType::TsLitType(literal) => match &literal.lit {
            s::TsLit::Str(text) => super::json_string(
                text.value
                    .as_str()
                    .ok_or_else(|| invalid("type text needs Unicode scalars", Some(ty.span())))?,
            )?,
            s::TsLit::Bool(value) => value.value.to_string(),
            s::TsLit::Number(value) => value
                .raw
                .as_ref()
                .ok_or_else(|| invalid("type number needs its token", Some(ty.span())))?
                .to_string(),
            _ => {
                return Err(invalid(
                    "unsupported type metadata literal",
                    Some(ty.span()),
                ));
            }
        },
        s::TsType::TsKeywordType(keyword)
            if keyword.kind == s::TsKeywordTypeKind::TsNullKeyword =>
        {
            "null".into()
        }
        s::TsType::TsTupleType(tuple) => {
            let mut values = vec![];
            for element in &tuple.elem_types {
                if element.label.is_some() {
                    return Err(invalid("metadata tuple has no labels", Some(ty.span())));
                }
                values.push(type_text(&element.ty)?);
            }
            format!("[{}]", values.join(","))
        }
        s::TsType::TsTypeLit(object) => {
            let mut fields = vec![];
            for member in &object.members {
                let s::TsTypeElement::TsPropertySignature(property) = member else {
                    return Err(invalid(
                        "type metadata needs explicit fields",
                        Some(ty.span()),
                    ));
                };
                if property.optional || property.readonly || property.computed {
                    return Err(invalid("metadata field has no modifiers", Some(ty.span())));
                }
                let key = match &*property.key {
                    s::Expr::Ident(ident) => ident.sym.to_string(),
                    expr => string(expr)?,
                };
                let value = property
                    .type_ann
                    .as_ref()
                    .ok_or_else(|| invalid("metadata field needs its value", Some(ty.span())))?;
                fields.push(format!(
                    "{}:{}",
                    super::json_string(&key)?,
                    type_text(&value.type_ann)?
                ));
            }
            format!("{{{}}}", fields.join(","))
        }
        _ => {
            return Err(invalid(
                "expected complete type metadata literal",
                Some(ty.span()),
            ));
        }
    })
}
