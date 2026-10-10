//! Expressions: every one lowers to an operand, after the statements that
//! compute it.

use lash_kernel_doc::{Expr, Literal, Member, Place, RecordEntry};

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

impl Lowerer<'_> {
    pub(crate) fn lower_expr(&mut self, expr: &ast::Expr) -> Lowering<Operand> {
        match expr {
            ast::Expr::Null => Ok(Operand::null()),
            ast::Expr::Bool(value) => Ok(Operand::bool(*value)),
            ast::Expr::Number(value) => Ok(Operand::number(*value)),
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
                None => Err(Diagnostic::new(
                    DiagnosticCode::ThisUnsupported,
                    "`this` outside a function is not in the TypeScript dialect",
                    self.span,
                )),
            },
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
            ast::Expr::Logical { left, op, right } => {
                let shown = self.narrowing(left);
                let left = self.lower_expr(left)?;
                // The right operand runs only where the left one did not
                // decide.
                let shown = match op {
                    LogicalOp::And => shown.when_true,
                    LogicalOp::Or => shown.when_false,
                    LogicalOp::Nullish => Vec::new(),
                };
                self.short_circuit(*op, left, |this| {
                    this.narrowed(shown, |this| this.lower_expr(right))
                })
            }
            ast::Expr::Conditional {
                test,
                consequent,
                alternate,
            } => {
                let shown = self.narrowing(test);
                let test = self.lower_expr(test)?;
                let condition = self.condition(test)?;
                let result = self.let_expr(Expr::Literal(Literal::Absent), Ty::Unknown);
                let name = super::statements::variable_of(&result);
                let place = Place::Variable(name.clone());
                let mut ty = Ty::Never;
                let mut branch = |this: &mut Self, shown: Vec<(String, Ty)>, value: &ast::Expr| {
                    this.block(|this| {
                        let value = this.narrowed(shown, |this| this.lower_expr(value))?;
                        ty = ty.join(&value.ty);
                        this.store(place.clone(), value);
                        Ok(())
                    })
                };
                let then_block = branch(self, shown.when_true, consequent)?;
                let else_block = branch(self, shown.when_false, alternate)?;
                self.emit_if(condition, then_block, else_block);
                Ok(Operand::variable(name, ty))
            }
            ast::Expr::Template {
                quasis,
                expressions,
            } => self.lower_template(quasis, expressions),
            ast::Expr::Function(function) => self.lower_function_expression(function),
            ast::Expr::Call { callee, args, span } => self.lower_call(callee, args, *span),
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
                    atom: value.atom,
                    ty: self.facts.believed(ty),
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
            return Ok(Operand::variable(kernel, self.type_of_binding(name)));
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
            Err(if name == "Reflect" {
                // The global the language gives reflection by.
                Diagnostic::refusal(
                    DiagnosticCode::ReflectionUnsupported,
                    "Unsupported: `Reflect`, which is reflection",
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
            let part = match element {
                ast::ArrayElement::Hole => {
                    self.let_expr(Expr::List(vec![Expr::Tuple(Vec::new())]), Ty::Unknown)
                }
                ast::ArrayElement::Value(value) => {
                    let value = self.lower_expr(value)?;
                    self.let_expr(Expr::List(vec![value.expr()]), Ty::Unknown)
                }
                ast::ArrayElement::Spread(value) => {
                    let value = self.lower_expr(value)?;
                    self.invoke("ts.array.spread", &[value], Ty::Unknown)?
                }
            };
            parts.push(part.expr());
        }
        let parts = self.let_expr(Expr::List(parts), Ty::Unknown);
        self.invoke("ts.spread", &[parts], Ty::Unknown)
    }

    /// A new list of values, some of them spread.
    pub(super) fn list(&mut self, items: &[(bool, &ast::Expr)]) -> Lowering<Operand> {
        if items.iter().all(|(spread, _)| !spread) {
            let exprs: Vec<_> = items.iter().map(|(_, expr)| *expr).collect();
            let operands = self.operands(&exprs)?;
            return Ok(self.let_expr(
                Expr::List(operands.iter().map(Operand::expr).collect()),
                Ty::Unknown,
            ));
        }
        let mut parts = Vec::new();
        for (spread, expr) in items {
            let value = self.lower_expr(expr)?;
            let part = if *spread {
                self.invoke("ts.array.spread", &[value], Ty::Unknown)?
            } else {
                self.let_expr(Expr::List(vec![value.expr()]), Ty::Unknown)
            };
            parts.push(part.expr());
        }
        let parts = self.let_expr(Expr::List(parts), Ty::Unknown);
        self.invoke("ts.spread", &[parts], Ty::Unknown)
    }

    /// A plain object whose `at`-th property is a process: the process is
    /// lifted, and the others are evaluated in source order around it.
    fn lower_object_with_process(
        &mut self,
        names: &[&str],
        values: &[&ast::Expr],
        at: usize,
    ) -> Lowering<Operand> {
        let mut entries = Vec::with_capacity(names.len());
        for (index, (field, value)) in names.iter().zip(values).enumerate() {
            let value = match (self.process_arrow(value), self.saved_process(value)) {
                (Some(function), _) if index == at => self.lower_process(None, function)?,
                (None, Some(saved)) if index == at => self.lower_saved_process(&saved)?,
                _ => {
                    let value = self.lower_expr(value)?;
                    self.pin(value)
                }
            };
            entries.push(RecordEntry {
                field: (*field).to_string(),
                value: value.expr(),
            });
        }
        Ok(self.let_expr(Expr::Record(entries), Ty::Unknown))
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
            // A process written inline as an object's `definition`.
            if let Some(at) = properties.iter().position(|property| {
                matches!(
                    property,
                    ast::ObjectProperty::KeyValue(ast::PropertyKey::Static(name), value)
                        if name == super::process::DEFINITION_PROPERTY
                            && (self.process_arrow(value).is_some()
                                || self.saved_process(value).is_some())
                )
            }) {
                return self.lower_object_with_process(&names, &values, at);
            }
            let mut operands = Vec::with_capacity(values.len());
            for (index, (name, value)) in names.iter().zip(&values).enumerate() {
                let value = self.named_expression(value, name)?;
                let value = if values[index + 1..]
                    .iter()
                    .any(|expression| !super::walk::is_inert(expression))
                {
                    self.pin(value)
                } else {
                    value
                };
                operands.push(value);
            }
            let entries = names
                .into_iter()
                .zip(operands)
                .map(|(field, value)| RecordEntry {
                    field: field.to_string(),
                    value: value.expr(),
                })
                .collect();
            return Ok(self.let_expr(Expr::Record(entries), Ty::Unknown));
        }
        // A computed key, a repeated key or a spread: the object is built
        // one property at a time, in source order.
        let object = self.let_expr(Expr::Record(Vec::new()), Ty::Unknown);
        for property in properties {
            match property {
                ast::ObjectProperty::KeyValue(key, value) => {
                    let key = match key {
                        ast::PropertyKey::Static(name) => Key::Static(name.clone()),
                        ast::PropertyKey::Computed(key) => {
                            let key = self.lower_expr(key)?;
                            Key::Computed(self.invoke("ts.to_property_key", &[key], Ty::Text)?)
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
                Ok(self.let_expr(number, Ty::Float))
            }
            UnaryOp::Not if value.ty == Ty::Bool => {
                let negated = self.native("bool.not", vec![value.expr()])?;
                Ok(self.let_expr(negated, Ty::Bool))
            }
            UnaryOp::Plus => self.invoke("ts.to_number", &[value], Ty::Float),
            UnaryOp::Minus => self.invoke("ts.neg", &[value], Ty::Float),
            UnaryOp::Not => self.invoke("ts.not", &[value], Ty::Bool),
            UnaryOp::TypeOf => self.invoke("ts.typeof", &[value], Ty::Text),
            UnaryOp::BitNot => self.invoke("ts.bit_not", &[value], Ty::Float),
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
        self.binary(op, left, right)
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
            let value = match direct {
                Direct::Numeric(function) => {
                    let args = vec![self.float(&left)?, self.float(&right)?];
                    self.native(function, args)?
                }
                Direct::NumericSwapped(function) => {
                    let args = vec![self.float(&right)?, self.float(&left)?];
                    self.native(function, args)?
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
            };
            return Ok(self.let_expr(value, ty));
        }
        let helper = binary_helper(op);
        if op == BinaryOp::In {
            // `key in object`: the helper takes the object first.
            return self.invoke(helper, &[right, left], ty);
        }
        self.invoke(helper, &[left, right], ty)
    }

    /// `operand + delta` as a number, with the number the operand was: the
    /// two values `++` and `--` give.
    fn stepped(&mut self, current: Operand, delta: f64) -> Lowering<(Operand, Operand)> {
        let old = if current.ty.is_number() {
            let number = self.float(&current)?;
            self.let_expr(number, Ty::Float)
        } else {
            self.invoke("ts.to_number", &[current], Ty::Float)?
        };
        let new = self.binary(BinaryOp::Add, old.clone(), Operand::number(delta))?;
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
        let result = self.temp();
        self.bind(result.clone(), left.clone());
        let held = Operand::variable(result.clone(), left.ty.clone());
        let mut ty = left.ty;
        let test = match op {
            LogicalOp::And | LogicalOp::Or => self.condition(held)?,
            LogicalOp::Nullish => self.invoke("ts.is_nullish", &[held], Ty::Bool)?.expr(),
        };
        let branch = self.block(|this| {
            let value = right(this)?;
            ty = ty.join(&value.ty);
            this.store(Place::Variable(result.clone()), value);
            Ok(())
        })?;
        match op {
            LogicalOp::And | LogicalOp::Nullish => self.emit_if(test, branch, Buf::default()),
            LogicalOp::Or => self.emit_if(test, Buf::default(), branch),
        }
        Ok(Operand::variable(result, ty))
    }

    fn lower_template(
        &mut self,
        quasis: &[String],
        expressions: &[ast::Expr],
    ) -> Lowering<Operand> {
        if expressions.is_empty() {
            return Ok(Operand::text(quasis.concat()));
        }
        let mut parts = Vec::new();
        for (index, quasi) in quasis.iter().enumerate() {
            if !quasi.is_empty() {
                parts.push(Operand::text(quasi.clone()).expr());
            }
            let Some(expression) = expressions.get(index) else {
                continue;
            };
            let value = self.lower_expr(expression)?;
            // Each value is converted where the source evaluates it: a
            // conversion may run the value's own `toString`.
            let spelled = if value.ty == Ty::Text {
                self.pin(value)
            } else {
                self.invoke("ts.to_string", &[value], Ty::Text)?
            };
            parts.push(spelled.expr());
        }
        let parts = self.let_expr(Expr::List(parts), Ty::Unknown);
        self.invoke("ts.join", &[parts], Ty::Text)
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
                let key = self.lower_key(property)?;
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
                        let value = self.lower_expr(value)?;
                        let result = self.binary(op, current, value)?;
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
                let current = if super::walk::is_inert(value) {
                    current
                } else {
                    self.pin(current)
                };
                let value = self.lower_expr(value)?;
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
                    Operand::variable(kernel, Ty::Float)
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
