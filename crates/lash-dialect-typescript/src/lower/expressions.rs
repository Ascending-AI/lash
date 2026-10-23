//! Expressions: every one lowers to an operand, after the statements that
//! compute it.

use lash_kernel_doc::{Expr, Literal, Member, Place, RecordEntry, Rhs, Stmt};

use lash_vocabulary::Name;

use super::calls::Key;
use super::patterns::Mode;
use super::{Buf, Lowerer, Lowering, Operand, Ty, unknown_binding};
use crate::adapter::{self as ast, AssignOp, BinaryOp, LogicalOp, UnaryOp};
use crate::types::{self, Direct};
use crate::{Diagnostic, DiagnosticCode, SourceSpan};

/// The helper that carries a binary operator's JavaScript meaning.
fn binary_helper(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "ts.add",
        BinaryOp::Subtract => "ts.sub",
        BinaryOp::Multiply => "ts.mul",
        BinaryOp::Divide => "ts.div",
        BinaryOp::Remainder => "ts.rem",
        BinaryOp::Exponent => "ts.pow",
        BinaryOp::BitAnd => "ts.bit_and",
        BinaryOp::BitOr => "ts.bit_or",
        BinaryOp::BitXor => "ts.bit_xor",
        BinaryOp::ShiftLeft => "ts.shl",
        BinaryOp::ShiftRight => "ts.shr",
        BinaryOp::ShiftRightUnsigned => "ts.ushr",
        BinaryOp::StrictEqual => "ts.strict_equals",
        BinaryOp::StrictNotEqual => "ts.strict_not_equals",
        BinaryOp::LooseEqual => "ts.loose_equals",
        BinaryOp::LooseNotEqual => "ts.loose_not_equals",
        BinaryOp::Less => "ts.lt",
        BinaryOp::LessEqual => "ts.le",
        BinaryOp::Greater => "ts.gt",
        BinaryOp::GreaterEqual => "ts.ge",
        BinaryOp::In => "ts.in",
        BinaryOp::InstanceOf => unreachable!("`instanceof` is resolved by its class"),
    }
}

/// Each cache belongs to one dynamic loop entry, never to a function or
/// source spelling. Only already-live, singly-defined roots can enter it.
#[derive(Debug, Default)]
pub(super) struct LoopPaths {
    roots: std::collections::HashMap<String, Name>,
    cached: std::collections::HashMap<(Name, Vec<String>), (Name, Name, Ty)>,
    declarations: Buf,
    filling: bool,
    function: usize,
}

fn member_path(expr: &ast::Expr, fields: &mut Vec<String>) -> Option<String> {
    match expr {
        ast::Expr::Ident(name, _) => Some(name.clone()),
        ast::Expr::Member {
            object, property, ..
        } => {
            let root = member_path(object, fields)?;
            let field = match property {
                ast::MemberProperty::Field(field) => field,
                ast::MemberProperty::Index(key) => match key.as_ref() {
                    ast::Expr::String(field) => field,
                    _ => return None,
                },
            };
            fields.push(field.clone());
            Some(root)
        }
        _ => None,
    }
}

impl Lowerer<'_> {
    pub(crate) fn lower_expr(&mut self, expr: &ast::Expr) -> Lowering<Operand> {
        let mut value = match self.cached_path(expr)? {
            Some(value) => value,
            None => self.lower_expr_inner(expr)?,
        };
        value.plain = self.facts.plain(expr);
        value.text_prefix = self.facts.text_prefix(expr);
        Ok(value)
    }

    /// Keep the first read at its source position: zero-trip loops and
    /// untaken branches must not evaluate a path. Cache state is ordinary
    /// kernel locals, so every charge slice/export can resume it exactly.
    pub(super) fn with_loop_paths<T>(
        &mut self,
        lower: impl FnOnce(&mut Self) -> Lowering<T>,
    ) -> Lowering<T> {
        let start = self.buf.len();
        let mut paths = LoopPaths {
            function: self.functions.len(),
            ..LoopPaths::default()
        };
        for scope in &self.scopes {
            for (name, binding) in &scope.bindings {
                paths.roots.remove(name);
                if binding.live
                    && binding.function + 1 == self.functions.len()
                    && self.facts.narrowable(name)
                {
                    paths.roots.insert(name.clone(), binding.kernel.clone());
                }
            }
        }
        self.paths.push(paths);
        let result = lower(self);
        let Some(paths) = self.paths.pop() else {
            unreachable!("the loop's path context remains on the stack while it lowers");
        };
        self.buf.splice(start, paths.declarations);
        result
    }

    fn cached_path(&mut self, expr: &ast::Expr) -> Lowering<Option<Operand>> {
        let Some(index) = self.paths.len().checked_sub(1) else {
            return Ok(None);
        };
        let paths = &self.paths[index];
        if paths.filling
            || paths.function != self.functions.len()
            || self.facts.plain(expr).is_none()
        {
            return Ok(None);
        }
        let mut fields = Vec::new();
        let Some(mut root) = member_path(expr, &mut fields) else {
            return Ok(None);
        };
        // Do not bypass resolution/TDZ of an alias whose declaration has
        // not run, even when its definition leads to a stable entry root.
        if self
            .binding_mut(&root)
            .is_none_or(|(_, binding)| !binding.live)
        {
            return Ok(None);
        }
        let paths = &self.paths[index];
        // One-hop paths seldom repay a branch and two cache bindings.
        if fields.len() < 2 {
            return Ok(None);
        }
        // An immutable local such as `const middle = tree.next.next`
        // may be declared inside the loop. Resolve its sole definition back
        // to a live entry root, without caching that local's own binding.
        for _ in 0..32 {
            if paths.roots.contains_key(&root) {
                break;
            }
            let Some(definition) = self.facts.path_definition(&root) else {
                return Ok(None);
            };
            let mut prefix = Vec::new();
            let Some(parent) = member_path(definition, &mut prefix) else {
                return Ok(None);
            };
            prefix.extend(fields);
            fields = prefix;
            root = parent;
        }
        let Some(kernel) = paths.roots.get(&root).cloned() else {
            return Ok(None);
        };
        if self
            .binding_mut(&root)
            .is_none_or(|(_, binding)| binding.kernel != kernel)
        {
            return Ok(None);
        }
        let key = (kernel, fields);
        let cached = self.paths[index].cached.get(&key).cloned();
        let (value, ready, ty) = match cached {
            Some(cached) => cached,
            None => {
                let value = self.temp();
                let ready = self.temp();
                let span = self.span;
                let paths = &mut self.paths[index];
                for (name, literal) in [
                    (value.clone(), Literal::Absent),
                    (ready.clone(), Literal::Bool(false)),
                ] {
                    paths.declarations.push(
                        Stmt::Let {
                            name,
                            value: Rhs::Expr(Expr::Literal(literal)),
                        },
                        super::Note {
                            span,
                            ..super::Note::default()
                        },
                    );
                }
                (value, ready, Ty::Unknown)
            }
        };
        self.paths[index].filling = true;
        let mut result_ty = ty;
        let fill = self.block(|this| {
            let result = this.lower_expr(expr)?;
            result_ty = result.ty.clone();
            this.store(Place::Variable(value.clone()), result);
            this.store(Place::Variable(ready.clone()), Operand::bool(true));
            Ok(())
        });
        let paths = &mut self.paths[index];
        paths.filling = false;
        paths
            .cached
            .insert(key, (value.clone(), ready.clone(), result_ty.clone()));
        self.emit_if(Expr::Variable(ready), Buf::default(), fill?);
        Ok(Some(Operand::variable(value, result_ty)))
    }

    fn lower_expr_inner(&mut self, expr: &ast::Expr) -> Lowering<Operand> {
        match expr {
            ast::Expr::Null => Ok(Operand::null()),
            ast::Expr::Bool(value) => Ok(Operand::bool(*value)),
            ast::Expr::Number(value) => Ok(Operand::number(*value)),
            ast::Expr::BigInt(value) => {
                Ok(self.inline(Expr::Literal(Literal::Int(value.clone())), Ty::BigInt))
            }
            ast::Expr::String(value) => Ok(Operand::text(value.clone())),
            ast::Expr::LoneSurrogateString => Err(Diagnostic::new(
                DiagnosticCode::LoneSurrogateLiteralUnsupported,
                "a string literal holds a lone surrogate, which a text cannot",
                self.span,
            )),
            ast::Expr::RegExp { pattern, flags } => {
                crate::regex::validate_literal(pattern, flags, self.span)?;
                let Some(constructor) = self.table.constructors.get("RegExp").copied() else {
                    return Err(Diagnostic::refusal(
                        DiagnosticCode::UnsupportedExpression,
                        "Unsupported: regular expression literals. The regular expression extension is not installed.",
                        self.span,
                    ));
                };
                let args = self.let_expr(
                    Expr::List(vec![
                        Operand::text(pattern.clone()).expr(),
                        Operand::text(flags.clone()).expr(),
                    ]),
                    Ty::Unknown,
                );
                self.invoke(constructor, &[Operand::undefined(), args], Ty::Unknown)
            }
            ast::Expr::Ident(name, span) => self.read(name, *span),
            ast::Expr::This => match self.this_binding() {
                Some(this) => Ok(Operand::variable(this, Ty::Unknown)),
                None => Ok(Operand::undefined()),
            },
            ast::Expr::TemplateObject { site, cooked, raw } => {
                if let Some(name) = self.templates.get(site) {
                    return Ok(Operand::variable(name.clone(), Ty::Unknown));
                }
                let entry = |name: &str, value| RecordEntry {
                    field: name.into(),
                    value,
                };
                let brand = || {
                    Expr::Tuple(vec![
                        Operand::text("typescript").expr(),
                        Operand::text("template").expr(),
                    ])
                };
                let raw = Expr::Record(vec![
                    entry("__lash_brand", brand()),
                    entry(
                        "items",
                        Expr::List(
                            raw.iter()
                                .map(|text| Operand::text(text.clone()).expr())
                                .collect(),
                        ),
                    ),
                ]);
                let value = Expr::Record(vec![
                    entry("__lash_brand", brand()),
                    entry(
                        "items",
                        Expr::List(
                            cooked
                                .iter()
                                .map(|text| {
                                    text.as_ref()
                                        .map_or_else(Operand::undefined, |text| {
                                            Operand::text(text.clone())
                                        })
                                        .expr()
                                })
                                .collect(),
                        ),
                    ),
                    entry("raw", raw),
                ]);
                let name = self.fresh("template");
                self.scopes[0]
                    .predeclare
                    .push((name.clone(), value, self.span));
                self.note_private(&name);
                self.templates.insert(*site, name.clone());
                Ok(Operand::variable(name, Ty::Unknown))
            }
            ast::Expr::Sequence(expressions) => {
                let mut result = Operand::undefined();
                for (index, expression) in expressions.iter().enumerate() {
                    result = self.lower_expr(expression)?;
                    if index + 1 != expressions.len() {
                        self.discard(result.clone());
                    }
                }
                Ok(result)
            }
            ast::Expr::DeleteChain { base, operations } => {
                self.lower_delete_chain(base, operations)
            }
            ast::Expr::Array(elements) => self.lower_array(elements),
            ast::Expr::Object(properties) => self.lower_object(properties),
            ast::Expr::Assign { target, op, value } => self.lower_assign(target, *op, value),
            ast::Expr::Member {
                object,
                property,
                span,
            } => self.lower_member(expr, object, property, *span),
            ast::Expr::Unary { op, value } => self.lower_unary(*op, value),
            ast::Expr::Binary {
                left,
                op,
                right,
                operand_spans,
            } => self.lower_binary(left, *op, right, *operand_spans),
            ast::Expr::Logical { left, op, right } => self.lower_logical(left, *op, right),
            ast::Expr::Conditional {
                test,
                consequent,
                alternate,
            } => self.lower_conditional(test, consequent, alternate),
            ast::Expr::Template {
                quasis,
                expressions,
            } => self.lower_template(quasis, expressions),
            ast::Expr::Function(function) => self.lower_function_expression(function),
            ast::Expr::Call { callee, args, span } => self.lower_call(callee, args, *span, false),
            ast::Expr::New { constructor, args } => self.lower_new(constructor, args),
            ast::Expr::OptionalChain { base, operations } => {
                self.lower_optional_chain(base, operations)
            }
            ast::Expr::Await { value, span } => self.lower_await(value, *span),
            ast::Expr::Update {
                target,
                delta,
                prefix,
            } => self.lower_update(target, *delta, *prefix),
            ast::Expr::As { value, ty } => {
                // The value itself, believed to be what the assertion says;
                // `as any` says nothing is known of it.
                let value = self.lower_expr(value)?;
                Ok(Operand {
                    expr: value.expr,
                    stable: value.stable,
                    ty: self.facts.believed(ty),
                    plain: value.plain,
                    text_prefix: value.text_prefix,
                })
            }
            ast::Expr::Delete { object, property } => {
                if let Some(refused) = self.reflection_on(object, "deleting a property of") {
                    return Err(refused);
                }
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                self.invoke("ts.delete", &[object, key.operand()], Ty::Bool)
            }
        }
    }

    /// A read of a name: a binding of the source, a session binding, or a
    /// global a built-in row answers for.
    pub(super) fn read(&mut self, name: &str, span: Option<SourceSpan>) -> Lowering<Operand> {
        let span = span.or(self.span);
        if let Some(kernel) = self.resolve(name, span)? {
            let stable = !self.facts.assigned(name)
                && self.binding_mut(name).is_some_and(|(_, binding)| {
                    binding.live
                        && binding.kernel == kernel
                        && matches!(
                            binding.kind,
                            super::BindingKind::Const | super::BindingKind::Local
                        )
                });
            return Ok(Operand {
                stable,
                ..Operand::variable(kernel, self.type_of_binding(name))
            });
        }
        if let Some(read) = self.read_package(&ast::Expr::Ident(name.to_owned(), span), span) {
            return read;
        }
        match name {
            "undefined" => return Ok(Operand::undefined()),
            "NaN" => return Ok(Operand::number(f64::NAN)),
            "Infinity" => return Ok(Operand::number(f64::INFINITY)),
            "arguments" => {
                return match self.arguments_binding() {
                    Some(args) => Ok(Operand::variable(args, Ty::Unknown)),
                    None => Err(Diagnostic::new(
                        DiagnosticCode::ArgumentsUnsupported,
                        "`arguments` outside a function is not in the TypeScript dialect",
                        span,
                    )),
                };
            }
            _ => {}
        }
        self.read_global(name, span).unwrap_or_else(|| {
            Err(if matches!(name, "Reflect" | "Symbol") {
                // The global the language gives reflection by.
                Diagnostic::refusal(
                    DiagnosticCode::ReflectionUnsupported,
                    format!("Unsupported: `{name}`, whose reflective protocol is outside the TypeScript dialect"),
                    span,
                )
            } else {
                unknown_binding(name, span)
            })
        })
    }

    fn lower_array(&mut self, elements: &[ast::ArrayElement]) -> Lowering<Operand> {
        if elements
            .iter()
            .all(|element| !matches!(element, ast::ArrayElement::Hole))
        {
            let items: Vec<_> = elements
                .iter()
                .map(|element| match element {
                    ast::ArrayElement::Spread(value) => (true, value),
                    ast::ArrayElement::Value(value) => (false, value),
                    ast::ArrayElement::Hole => unreachable!("handled below"),
                })
                .collect();
            return self.list(&items);
        }
        let mut parts = Vec::new();
        for element in elements {
            parts.push(self.aside(|this| match element {
                ast::ArrayElement::Hole => {
                    Ok(this.inline(Expr::List(vec![Expr::Tuple(Vec::new())]), Ty::Unknown))
                }
                ast::ArrayElement::Value(value) => {
                    let value = this.lower_expr(value)?;
                    Ok(this.inline(Expr::List(vec![value.expr()]), Ty::Unknown))
                }
                ast::ArrayElement::Spread(value) => {
                    let value = this.lower_expr(value)?;
                    this.invoke("ts.array.spread", &[value], Ty::Unknown)
                }
            })?);
        }
        self.spread_parts(parts)
    }

    /// A new list of values, some of them spread.
    pub(super) fn list(&mut self, items: &[(bool, &ast::Expr)]) -> Lowering<Operand> {
        if items.iter().all(|(spread, _)| !spread) {
            let exprs: Vec<_> = items.iter().map(|(_, expr)| *expr).collect();
            let operands = self.operands(&exprs)?;
            return Ok(self.inline(
                Expr::List(operands.iter().map(Operand::expr).collect()),
                Ty::Array,
            ));
        }
        let mut parts = Vec::new();
        for (spread, expr) in items {
            parts.push(self.aside(|this| {
                let value = this.lower_expr(expr)?;
                if *spread {
                    this.invoke("ts.array.spread", &[value], Ty::Unknown)
                } else {
                    Ok(this.inline(Expr::List(vec![value.expr()]), Ty::Unknown))
                }
            })?);
        }
        self.spread_parts(parts)
    }

    fn spread_parts(&mut self, parts: Vec<(Buf, Operand)>) -> Lowering<Operand> {
        let parts = self.ordered(parts);
        let parts = self.inline(
            Expr::List(parts.iter().map(Operand::expr).collect()),
            Ty::Unknown,
        );
        self.invoke("ts.spread", &[parts], Ty::Array)
    }

    fn lower_object(&mut self, properties: &[ast::ObjectProperty]) -> Lowering<Operand> {
        let mut names: Vec<&str> = Vec::new();
        let plain = properties.iter().all(|property| match property {
            ast::ObjectProperty::KeyValue(ast::PropertyKey::Static(name), _) => {
                let fresh = !names.contains(&name.as_str());
                names.push(name);
                fresh
            }
            _ => false,
        });
        if plain {
            let values: Vec<&ast::Expr> = properties
                .iter()
                .filter_map(|property| match property {
                    ast::ObjectProperty::KeyValue(_, value) => Some(value),
                    ast::ObjectProperty::Spread(_) => None,
                })
                .collect();
            let mut parts = Vec::with_capacity(values.len());
            for (name, value) in names.iter().zip(&values) {
                parts.push(self.aside(|this| this.named_expression(value, name))?);
            }
            let operands = self.ordered(parts);
            let entries = names
                .into_iter()
                .zip(operands)
                .map(|(field, value)| RecordEntry {
                    field: field.to_string(),
                    value: value.expr(),
                })
                .collect();
            return Ok(self.inline(Expr::Record(entries), Ty::Object));
        }
        // A computed key, a repeated key or a spread: the object is built
        // one property at a time, in source order.
        let object = self.let_expr(Expr::Record(Vec::new()), Ty::Object);
        for property in properties {
            match property {
                ast::ObjectProperty::KeyValue(key, value) => {
                    let key = match key {
                        ast::PropertyKey::Static(name) => Key::Static(name.clone()),
                        ast::PropertyKey::Computed(key) => {
                            let key = self.lower_expr(key)?;
                            {
                                let key = self.invoke("ts.to_property_key", &[key], Ty::Text)?;
                                Key::Computed(self.pin(key))
                            }
                        }
                    };
                    let value = match &key {
                        Key::Static(name) => self.named_expression(value, name)?,
                        Key::Computed(_) => self.lower_expr(value)?,
                    };
                    match key {
                        Key::Static(field) => self.store(
                            Place::Member(Member::Field {
                                target: object.expr(),
                                field,
                            }),
                            value,
                        ),
                        Key::Computed(_) => self.set_member(&object, &key, value)?,
                    }
                }
                ast::ObjectProperty::Spread(source) => {
                    let source = self.lower_expr(source)?;
                    let copied =
                        self.invoke("ts.assign", &[object.clone(), source], Ty::Unknown)?;
                    self.discard(copied);
                }
            }
        }
        Ok(object)
    }

    fn lower_unary(&mut self, op: UnaryOp, value: &ast::Expr) -> Lowering<Operand> {
        match (op, value) {
            (UnaryOp::Minus, ast::Expr::Number(number)) => return Ok(Operand::number(-number)),
            (UnaryOp::TypeOf, ast::Expr::Ident(name, _))
                if !self.is_bound(name)
                    && !crate::builtins::is_global(name)
                    && name != "arguments"
                    && name != "Reflect" =>
            {
                // `typeof` of a name nothing binds is "undefined", not an
                // error.
                return Ok(Operand::text("undefined"));
            }
            _ => {}
        }
        let value = self.lower_expr(value)?;
        match op {
            UnaryOp::Plus | UnaryOp::Minus if value.ty.is_number() => {
                let number = self.float(&value)?;
                let number = if op == UnaryOp::Minus {
                    self.native("num.neg", vec![number])?
                } else {
                    number
                };
                Ok(self.inline(number, Ty::Float))
            }
            UnaryOp::Not if value.ty == Ty::Bool => {
                let negated = self.native("bool.not", vec![value.expr()])?;
                Ok(self.inline(negated, Ty::Bool))
            }
            UnaryOp::Plus => self.invoke("ts.to_number", &[value], Ty::Float),
            UnaryOp::Minus => {
                let ty = types::unary_result(op, &value.ty);
                self.invoke("ts.neg", &[value], ty)
            }
            UnaryOp::Not => self.invoke("ts.not", &[value], Ty::Bool),
            UnaryOp::TypeOf => self.invoke("ts.typeof", &[value], Ty::Text),
            UnaryOp::BitNot => {
                let ty = types::unary_result(op, &value.ty);
                self.invoke("ts.bit_not", &[value], ty)
            }
            UnaryOp::Void => {
                self.discard(value);
                Ok(Operand::undefined())
            }
        }
    }

    fn lower_binary(
        &mut self,
        left: &ast::Expr,
        op: BinaryOp,
        right: &ast::Expr,
        operand_spans: Option<[SourceSpan; 2]>,
    ) -> Lowering<Operand> {
        if op == BinaryOp::InstanceOf
            && let Some(tested) = self.instance_of_package(left, right)
        {
            return tested;
        }
        if op == BinaryOp::InstanceOf {
            let test = match right {
                ast::Expr::Ident(class, _) if !self.is_bound(class) => {
                    self.table.instance_tests.get(class.as_str()).copied()
                }
                _ => None,
            };
            let Some(test) = test else {
                let Some([left_span, right_span]) = operand_spans else {
                    unreachable!("an instanceof has source operands");
                };
                let value = &self.source[left_span.start..left_span.end];
                let class = &self.source[right_span.start..right_span.end];
                return Err(Diagnostic::with_repair(
                    DiagnosticCode::InstanceOfUnsupported,
                    "`instanceof` needs a built-in class on its right",
                    format!(
                        "for `{value} instanceof {class}`, check `({value}).name` for an error name, or use `Array.isArray({value})` for an array"
                    ),
                    self.span,
                ));
            };
            let value = self.lower_expr(left)?;
            return self.invoke(test, &[value], Ty::Bool);
        }
        if op == BinaryOp::In
            && let Some(refused) = self.reflection_on(right, "`in` on")
        {
            return Err(refused);
        }
        let mut operands = self.operands(&[left, right])?.into_iter();
        let (Some(left), Some(right)) = (operands.next(), operands.next()) else {
            unreachable!("two operands were lowered");
        };
        self.binary_with_admission(op, left, right, operand_spans)
    }

    /// A binary operator over operands already evaluated.
    pub(super) fn binary(
        &mut self,
        op: BinaryOp,
        left: Operand,
        right: Operand,
    ) -> Lowering<Operand> {
        let ty = types::binary_result(op, &left.ty, &right.ty);
        if let Some(direct) = types::direct_binary(op, &left.ty, &right.ty) {
            // Source values are evaluated before numeric checks. A trusted
            // left annotation can fail its check, so hold a non-atomic right
            // value first, after reading the left, to preserve raise order.
            let (left, right) = if left.ty.is_number()
                && left.ty != Ty::Float
                && !matches!(right.expr, Expr::Literal(_) | Expr::Variable(_))
            {
                (self.pin(left), self.pin(right))
            } else {
                (left, right)
            };
            let value = match direct {
                Direct::Numeric(function) => {
                    let args = vec![self.float(&left)?, self.float(&right)?];
                    self.native(function, args)?
                }
                Direct::NumericSwapped(function) => {
                    let left = self.float(&left)?;
                    let left = self.let_expr(left, Ty::Float);
                    let right = self.float(&right)?;
                    let right = self.let_expr(right, Ty::Float);
                    self.native(function, vec![right.expr(), left.expr()])?
                }
                Direct::NumericEqual { negated } => {
                    let args = vec![self.float(&left)?, self.float(&right)?];
                    let equal = self.native("eq", args)?;
                    if negated {
                        self.native("bool.not", vec![equal])?
                    } else {
                        equal
                    }
                }
                Direct::Concat => self.native("text.concat", vec![left.expr(), right.expr()])?,
                Direct::ConcatFloat { float_left } => {
                    let (left, right) = if float_left {
                        (self.float_text(left)?, right)
                    } else {
                        (left, self.float_text(right)?)
                    };
                    self.native("text.concat", vec![left.expr(), right.expr()])?
                }
            };
            return Ok(self.inline(value, ty));
        }
        let helper = binary_helper(op);
        if op == BinaryOp::In {
            // `key in object`: the helper takes the object first.
            let left = self.pin(left);
            let right = self.pin(right);
            return self.invoke(helper, &[right, left], ty);
        }
        self.invoke(helper, &[left, right], ty)
    }

    /// `operand + delta` as a number, with the number the operand was: the
    /// two values `++` and `--` give.
    fn stepped(&mut self, current: Operand, delta: f64) -> Lowering<(Operand, Operand)> {
        let ty = if current.ty == Ty::BigInt {
            Ty::BigInt
        } else if current.ty.is_number() {
            Ty::Float
        } else {
            Ty::Unknown
        };
        let old = if current.ty.is_number() {
            let number = self.float(&current)?;
            self.let_expr(number, Ty::Float)
        } else {
            self.invoke("ts.to_numeric", &[current], ty.clone())?
        };
        let old = self.pin(old);
        let new = if old.ty == Ty::Float {
            self.binary(BinaryOp::Add, old.clone(), Operand::number(delta))?
        } else {
            self.invoke("ts.step", &[old.clone(), Operand::number(delta)], ty)?
        };
        Ok((old, new))
    }

    /// `left && right`, `left || right` and `left ?? right`: the right
    /// operand is evaluated only when the left one does not decide, and the
    /// result is an operand, not a bool.
    pub(super) fn short_circuit(
        &mut self,
        op: LogicalOp,
        left: Operand,
        right: impl FnOnce(&mut Self) -> Lowering<Operand>,
    ) -> Lowering<Operand> {
        self.expression_block(|this| {
            let result = this.temp();
            let left_ty = left.ty.clone();
            this.bind(result.clone(), left);
            let left = Operand::variable(result.clone(), left_ty);
            let test = match op {
                LogicalOp::And | LogicalOp::Or => this.condition(left.clone())?,
                LogicalOp::Nullish => this.nullish_condition(&left)?,
            };
            let right = this.value_block(right)?;
            if this.branch_expression(&right.0, right.1.expr()).is_none() {
                let ty = left.ty.join(&right.1.ty);
                let branch = this.store_branch(right, Place::Variable(result.clone()));
                match op {
                    LogicalOp::And | LogicalOp::Nullish => {
                        this.emit_if(test, branch, Buf::default())
                    }
                    LogicalOp::Or => this.emit_if(test, Buf::default(), branch),
                }
                return Ok(Operand::variable(result, ty));
            }
            let held = (Buf::default(), left);
            Ok(match op {
                LogicalOp::And | LogicalOp::Nullish => this.choose(test, right, held),
                LogicalOp::Or => this.choose(test, held, right),
            })
        })
    }

    /// The text of a float, which has no `toString` of its own to run.
    fn float_text(&mut self, value: Operand) -> Lowering<Operand> {
        self.invoke("ts.number.format_value", &[value], Ty::Text)
    }

    /// ToString of a value: a float's is spelled directly.
    fn text_of(&mut self, value: Operand) -> Lowering<Operand> {
        if value.ty == Ty::Float {
            self.float_text(value)
        } else {
            self.invoke("ts.to_string", &[value], Ty::Text)
        }
    }

    fn lower_template(
        &mut self,
        quasis: &[String],
        expressions: &[(SourceSpan, ast::Expr)],
    ) -> Lowering<Operand> {
        if expressions.is_empty() {
            return Ok(Operand::text(quasis.concat()));
        }
        let mut parts = Vec::new();
        for (index, quasi) in quasis.iter().enumerate() {
            if !quasi.is_empty() {
                parts.push((Buf::default(), Operand::text(quasi.clone())));
            }
            let Some((span, expression)) = expressions.get(index) else {
                continue;
            };
            let part = self.aside(|this| {
                let value = this.lower_expr(expression)?;
                this.check_implicit_text(&value.ty, Some(*span))?;
                // Each value is converted where the source evaluates it: a
                // conversion may run the value's own `toString`.
                match value.ty {
                    Ty::Text => Ok(value),
                    _ => this.text_of(value),
                }
            })?;
            parts.push(part);
        }
        let parts = self.ordered(parts);
        // Two parts are one concatenation, with no list to build. The
        // native checks both are texts, as the join does.
        if let [left, right] = parts.as_slice() {
            let joined = self.native("text.concat", vec![left.expr(), right.expr()])?;
            return Ok(self.inline(joined, Ty::Text));
        }
        // One native join, charged by the bytes it writes.
        let parts = self.inline(
            Expr::List(parts.iter().map(Operand::expr).collect()),
            Ty::Unknown,
        );
        let joined = self.native(
            "text.join",
            vec![parts.expr(), Operand::text(String::new()).expr()],
        )?;
        Ok(self.inline(joined, Ty::Text))
    }

    fn lower_assign(
        &mut self,
        target: &ast::AssignTarget,
        op: AssignOp,
        value: &ast::Expr,
    ) -> Lowering<Operand> {
        match target {
            ast::AssignTarget::Ident(name) => self.assign_variable(name, op, value, true),
            ast::AssignTarget::ParenIdent(name) => self.assign_variable(name, op, value, false),
            ast::AssignTarget::Member { object, property } => {
                if let Some(refused) = self.reflection_on(object, "assigning a property of") {
                    return Err(refused);
                }
                // The target's object and key are evaluated, and held,
                // before the right-hand side.
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = match self.lower_key(property)? {
                    Key::Computed(value) => Key::Computed(self.pin(value)),
                    key => key,
                };
                match op {
                    AssignOp::Assign => {
                        let value = self.lower_expr(value)?;
                        let value = self.pin(value);
                        self.set_member(&object, &key, value.clone())?;
                        Ok(value)
                    }
                    AssignOp::Binary(op) => {
                        let key = self.reference_key(&object, key)?;
                        let current = self.get_member(&object, &key)?;
                        let (buf, value) = self.aside(|this| this.lower_expr(value))?;
                        let current = if buf.is_empty() {
                            current
                        } else {
                            self.pin(current)
                        };
                        self.append(buf);
                        let result = self.binary(op, current, value)?;
                        let result = self.pin(result);
                        self.set_member(&object, &key, result.clone())?;
                        Ok(result)
                    }
                    AssignOp::Logical(op) => {
                        let key = self.reference_key(&object, key)?;
                        let current = self.get_member(&object, &key)?;
                        self.short_circuit(op, current, |this| {
                            let value = this.lower_expr(value)?;
                            let value = this.pin(value);
                            this.set_member(&object, &key, value.clone())?;
                            Ok(value)
                        })
                    }
                }
            }
            ast::AssignTarget::Pattern(pattern) => {
                let value = self.lower_expr(value)?;
                let value = self.pin(value);
                self.destructure(pattern, value.clone(), Mode::Assign)?;
                Ok(value)
            }
        }
    }

    fn assign_variable(
        &mut self,
        name: &str,
        op: AssignOp,
        value: &ast::Expr,
        named: bool,
    ) -> Lowering<Operand> {
        match op {
            AssignOp::Assign => {
                let value = if named {
                    self.named_expression(value, name)?
                } else {
                    self.lower_expr(value)?
                };
                let ty = value.ty.clone();
                let kernel = self.resolve_for_write(name, self.span)?;
                self.store(Place::Variable(kernel.clone()), value);
                Ok(Operand::variable(kernel, ty))
            }
            AssignOp::Binary(op) => {
                let current = self.read(name, None)?;
                let (buf, value) = self.aside(|this| this.lower_expr(value))?;
                let current = if buf.is_empty() {
                    current
                } else {
                    self.pin(current)
                };
                self.append(buf);
                let result = self.binary(op, current, value)?;
                let ty = result.ty.clone();
                let kernel = self.resolve_for_write(name, self.span)?;
                self.store(Place::Variable(kernel.clone()), result);
                Ok(Operand::variable(kernel, ty))
            }
            AssignOp::Logical(op) => {
                let current = self.read(name, None)?;
                let kernel = self.resolve_for_write(name, self.span)?;
                self.short_circuit(op, current, |this| {
                    let value = if named {
                        this.named_expression(value, name)?
                    } else {
                        this.lower_expr(value)?
                    };
                    let ty = value.ty.clone();
                    this.store(Place::Variable(kernel.clone()), value);
                    Ok(Operand::variable(kernel, ty))
                })
            }
        }
    }

    /// `++` and `--`: the operand converted to a number, one added or
    /// taken, the result written back. A postfix form gives the number
    /// before the change.
    fn lower_update(
        &mut self,
        target: &ast::AssignTarget,
        delta: f64,
        prefix: bool,
    ) -> Lowering<Operand> {
        match target {
            ast::AssignTarget::Ident(name) | ast::AssignTarget::ParenIdent(name) => {
                let current = self.read(name, None)?;
                let (old, new) = self.stepped(current, delta)?;
                let kernel = self.resolve_for_write(name, self.span)?;
                self.store(Place::Variable(kernel.clone()), new);
                Ok(if prefix {
                    Operand::variable(kernel, old.ty.clone())
                } else {
                    old
                })
            }
            ast::AssignTarget::Member { object, property } => {
                if let Some(refused) = self.reflection_on(object, "updating a property of") {
                    return Err(refused);
                }
                let object = self.lower_expr(object)?;
                let object = self.pin(object);
                let key = self.lower_key(property)?;
                let key = self.reference_key(&object, key)?;
                let current = self.get_member(&object, &key)?;
                let (old, new) = self.stepped(current, delta)?;
                let new = self.pin(new);
                self.set_member(&object, &key, new.clone())?;
                Ok(if prefix { new } else { old })
            }
            ast::AssignTarget::Pattern(_) => Err(Diagnostic::defect(
                DiagnosticCode::UnsupportedExpression,
                "`++` and `--` need a variable or a property",
                self.span,
            )),
        }
    }
}
