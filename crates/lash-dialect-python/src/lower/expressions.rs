//! Expressions.

use lash_kernel_doc::{Expr, Float, Integer, Literal, MapEntry, Member, Name, Place, Rhs, Stmt};
use ruff_python_ast::{self as ast, Expr as PyExpr};
use ruff_text_size::{Ranged, TextRange};

use super::statements::Body;
use super::{Buf, Lowerer, Lowering, Operand};
use crate::diagnostics::{self, Code};
use crate::scope::Ty;

/// What a comprehension collects.
enum Collect<'a> {
    List(&'a PyExpr),
    Set(&'a PyExpr),
    Dict(&'a PyExpr, &'a PyExpr),
}

impl Lowerer<'_> {
    pub(crate) fn expr(&mut self, expr: &PyExpr) -> Lowering<Operand> {
        self.nested(expr.range(), |this| this.expr_inner(expr))
    }

    fn expr_inner(&mut self, expr: &PyExpr) -> Lowering<Operand> {
        match expr {
            PyExpr::NoneLiteral(_) => Ok(Operand::none()),
            PyExpr::BooleanLiteral(literal) => {
                Ok(Operand::literal(Literal::Bool(literal.value), Ty::Bool))
            }
            PyExpr::NumberLiteral(literal) => number(&literal.value, literal.range),
            PyExpr::StringLiteral(literal) => Ok(Operand::text(literal.value.to_str())),
            PyExpr::BytesLiteral(literal) => Err(diagnostics::refusal(
                Code::LiteralUnsupported,
                "bytes literals are not in the dialect",
                literal.range,
                "use a str",
            )),
            PyExpr::EllipsisLiteral(literal) => Err(diagnostics::refusal(
                Code::LiteralUnsupported,
                "`...` is not a value in the dialect",
                literal.range,
                "use None",
            )),
            PyExpr::FString(fstring) => self.fstring(fstring),
            PyExpr::TString(tstring) => Err(diagnostics::refusal(
                Code::SyntaxUnsupported,
                "template strings are not in the dialect",
                tstring.range,
                "use an f-string",
            )),
            PyExpr::Name(name) => self.read(name.id.as_str(), name.range),
            PyExpr::BinOp(operation) => {
                if operation.op == ast::Operator::Mod
                    && matches!(
                        operation.left.as_ref(),
                        PyExpr::StringLiteral(_) | PyExpr::FString(_)
                    )
                {
                    return Err(diagnostics::refusal(
                        Code::OperatorUnsupported,
                        "`%` formatting of a str is not in the dialect",
                        operation.range,
                        "use an f-string",
                    ));
                }
                let operands = self.operands(&[&operation.left, &operation.right])?;
                self.binary(
                    operation.op,
                    operands[0].clone(),
                    operands[1].clone(),
                    operation.range,
                )
            }
            PyExpr::UnaryOp(operation) => self.unary(operation),
            PyExpr::BoolOp(operation) => self.bool_op(operation),
            PyExpr::Compare(compare) => self.compare(compare),
            PyExpr::If(conditional) => {
                let result = self.temp();
                self.emit(Stmt::Let {
                    name: result.clone(),
                    value: Rhs::Expr(Expr::Literal(Literal::Absent)),
                });
                let test = self.expr(&conditional.test)?;
                let condition = self.truth(test)?;
                let mut types = Vec::new();
                let mut branch = |this: &mut Self, value: &PyExpr| {
                    this.block(|this| {
                        let value = this.expr(value)?;
                        types.push(value.ty);
                        this.store(Place::Variable(result.clone()), value);
                        Ok(())
                    })
                };
                let then_block = branch(self, &conditional.body)?;
                let else_block = branch(self, &conditional.orelse)?;
                self.emit_if(condition, then_block, else_block);
                let ty = if types[0] == types[1] {
                    types[0]
                } else {
                    Ty::Unknown
                };
                Ok(Operand::temp(result, ty))
            }
            PyExpr::Lambda(lambda) => {
                let defaults = match lambda.parameters.as_deref() {
                    Some(parameters) => self.defaults(parameters)?,
                    None => Vec::new(),
                };
                let (params, body) = self.function_body(
                    lambda.parameters.as_deref(),
                    Body::Value(&lambda.body),
                    false,
                    &defaults,
                    "<lambda>",
                )?;
                Ok(self.emit_closure(params, body))
            }
            PyExpr::Dict(dict) => {
                let mut exprs = Vec::with_capacity(dict.items.len() * 2);
                for item in &dict.items {
                    let Some(key) = &item.key else {
                        return Err(diagnostics::refusal(
                            Code::StarUnsupported,
                            "`**` in a dict display is not in the dialect",
                            item.value.range(),
                            "build the dict, then call `update`",
                        ));
                    };
                    exprs.push(key);
                    exprs.push(&item.value);
                }
                let operands = self.operands(&exprs)?;
                let entries = operands
                    .chunks(2)
                    .map(|pair| MapEntry {
                        key: pair[0].expr.clone(),
                        value: pair[1].expr.clone(),
                    })
                    .collect();
                // A display makes one object; it is bound where it is
                // written so that the object is made once, there.
                Ok(self.let_rhs(Rhs::Expr(Expr::Map(entries)), Ty::Dict))
            }
            PyExpr::Set(set) => {
                let items = self.elements(&set.elts)?;
                Ok(self.let_rhs(Rhs::Expr(Expr::Set(items)), Ty::Set))
            }
            PyExpr::List(list) => {
                let items = self.elements(&list.elts)?;
                Ok(self.let_rhs(Rhs::Expr(Expr::List(items)), Ty::List))
            }
            PyExpr::Tuple(tuple) => {
                let items = self.elements(&tuple.elts)?;
                Ok(Operand::inline(Expr::Tuple(items), Ty::Tuple))
            }
            PyExpr::ListComp(comprehension) => {
                self.comprehension(Collect::List(&comprehension.elt), &comprehension.generators)
            }
            // A generator expression is evaluated where it stands, as a
            // list (`deviations.md`).
            PyExpr::Generator(comprehension) => {
                self.comprehension(Collect::List(&comprehension.elt), &comprehension.generators)
            }
            PyExpr::SetComp(comprehension) => {
                self.comprehension(Collect::Set(&comprehension.elt), &comprehension.generators)
            }
            PyExpr::DictComp(comprehension) => match &comprehension.key {
                Some(key) => self.comprehension(
                    Collect::Dict(key, &comprehension.value),
                    &comprehension.generators,
                ),
                None => Err(diagnostics::refusal(
                    Code::StarUnsupported,
                    "`**` in a dict comprehension is not in the dialect",
                    comprehension.range,
                    "write the key and the value",
                )),
            },
            PyExpr::Await(await_expr) => self.await_expr(await_expr),
            PyExpr::Call(call) => self.call(call, false),
            PyExpr::Attribute(attribute) => self.attribute(attribute),
            PyExpr::Subscript(subscript) => match subscript.slice.as_ref() {
                PyExpr::Slice(slice) => {
                    // A bound that is not written is None.
                    let operands = self.optional_operands(&[
                        Some(&subscript.value),
                        slice.lower.as_deref(),
                        slice.upper.as_deref(),
                        slice.step.as_deref(),
                    ])?;
                    self.invoke("py.slice", &operands, Ty::Unknown)
                }
                index => {
                    let operands = self.operands(&[&subscript.value, index])?;
                    self.invoke("py.getitem", &operands, Ty::Unknown)
                }
            },
            PyExpr::Slice(slice) => Err(diagnostics::refusal(
                Code::SyntaxUnsupported,
                "a slice stands only inside a subscript",
                slice.range,
                "write `xs[start:stop]`",
            )),
            PyExpr::Starred(starred) => Err(diagnostics::refusal(
                Code::StarUnsupported,
                "`*` unpacking is not in the dialect here",
                starred.range,
                "concatenate with `+`",
            )),
            PyExpr::Named(named) => Err(diagnostics::refusal(
                Code::SyntaxUnsupported,
                "an assignment expression (`:=`) is not in the dialect",
                named.range,
                "assign on a line of its own",
            )),
            PyExpr::Yield(node) => Err(generator(node.range)),
            PyExpr::YieldFrom(node) => Err(generator(node.range)),
            PyExpr::IpyEscapeCommand(node) => Err(diagnostics::diagnostic(
                Code::Syntax,
                "an IPython escape is not Python",
                node.range,
            )),
        }
    }

    /// The members of a display, in order.
    fn elements(&mut self, elts: &[PyExpr]) -> Lowering<Vec<Expr>> {
        let exprs: Vec<&PyExpr> = elts.iter().collect();
        Ok(self
            .operands(&exprs)?
            .into_iter()
            .map(|operand| operand.expr)
            .collect())
    }

    /// `left op right`, on operands already lowered.
    pub(super) fn binary(
        &mut self,
        op: ast::Operator,
        left: Operand,
        right: Operand,
        range: TextRange,
    ) -> Lowering<Operand> {
        use ast::Operator;
        let numbers = left.ty.is_number() && right.ty.is_number();
        let number = if left.ty == Ty::Int && right.ty == Ty::Int {
            Ty::Int
        } else if numbers {
            Ty::Float
        } else {
            Ty::Unknown
        };
        // Two operands the source types as numbers add, subtract and
        // multiply as the kernel does (tier 1).
        let direct = match op {
            Operator::Add if numbers => Some(("add", number)),
            Operator::Sub if numbers => Some(("sub", number)),
            Operator::Mult if numbers => Some(("mul", number)),
            Operator::Add if left.ty == Ty::Str && right.ty == Ty::Str => {
                Some(("text.concat", Ty::Str))
            }
            _ => None,
        };
        if let Some((function, ty)) = direct {
            let call = self.native(function, vec![left.expr, right.expr])?;
            return Ok(Operand::inline(call, ty));
        }
        let (helper, ty) = match op {
            Operator::Add => ("py.add", Ty::Unknown),
            Operator::Sub => ("py.sub", Ty::Unknown),
            Operator::Mult => ("py.mul", Ty::Unknown),
            Operator::Div => ("py.truediv", Ty::Float),
            Operator::FloorDiv => ("py.floordiv", number),
            Operator::Mod => ("py.mod", number),
            Operator::Pow => ("py.pow", Ty::Unknown),
            Operator::BitOr | Operator::BitAnd | Operator::BitXor => {
                let symbol = Operand::text(op.as_str());
                return self.invoke("py.set_op", &[symbol, left, right], Ty::Set);
            }
            Operator::MatMult | Operator::LShift | Operator::RShift => {
                return Err(diagnostics::refusal(
                    Code::OperatorUnsupported,
                    format!("the operator `{}` is not in the dialect", op.as_str()),
                    range,
                    "use `*`, `//` and `**` with a power of two for shifts",
                ));
            }
        };
        self.invoke(helper, &[left, right], ty)
    }

    fn unary(&mut self, operation: &ast::ExprUnaryOp) -> Lowering<Operand> {
        match operation.op {
            ast::UnaryOp::Not => {
                let operand = self.expr(&operation.operand)?;
                let truth = self.truth(operand)?;
                Ok(Operand::inline(self.not(truth)?, Ty::Bool))
            }
            ast::UnaryOp::USub => {
                // A negative number literal is a literal.
                if let PyExpr::NumberLiteral(literal) = operation.operand.as_ref() {
                    let operand = number(&literal.value, literal.range)?;
                    return Ok(match operand.expr {
                        Expr::Literal(Literal::Int(value)) => Operand::literal(
                            Literal::Int(Integer::new(-value.into_bigint())),
                            Ty::Int,
                        ),
                        Expr::Literal(Literal::Float(value)) => {
                            Operand::literal(Literal::Float(Float::new(-value.get())), Ty::Float)
                        }
                        _ => operand,
                    });
                }
                let operand = self.expr(&operation.operand)?;
                if operand.ty.is_number() {
                    let ty = operand.ty;
                    return Ok(Operand::inline(self.native("neg", vec![operand.expr])?, ty));
                }
                self.invoke("py.neg", &[operand], Ty::Unknown)
            }
            ast::UnaryOp::UAdd => {
                let operand = self.expr(&operation.operand)?;
                if operand.ty.is_number() {
                    return Ok(operand);
                }
                self.invoke("py.pos", &[operand], Ty::Unknown)
            }
            ast::UnaryOp::Invert => Err(diagnostics::refusal(
                Code::OperatorUnsupported,
                "the operator `~` is not in the dialect",
                operation.range,
                "write `-x - 1`",
            )),
        }
    }

    /// `a and b` and `a or b`: the operand that decides, not its truth.
    fn bool_op(&mut self, operation: &ast::ExprBoolOp) -> Lowering<Operand> {
        let Some((first, rest)) = operation.values.split_first() else {
            return Ok(Operand::none());
        };
        let first = self.expr(first)?;
        let mut all_bool = first.ty == Ty::Bool;
        let ty = first.ty;
        let result = self.temp();
        self.emit(Stmt::Let {
            name: result.clone(),
            value: Rhs::Expr(first.expr),
        });
        let is_and = operation.op == ast::BoolOp::And;
        self.bool_rest(rest, &result, ty, is_and, &mut all_bool)?;
        let ty = if all_bool { Ty::Bool } else { Ty::Unknown };
        Ok(Operand::temp(result, ty))
    }

    fn bool_rest(
        &mut self,
        rest: &[PyExpr],
        result: &Name,
        ty: Ty,
        is_and: bool,
        all_bool: &mut bool,
    ) -> Lowering<()> {
        let Some((next, rest)) = rest.split_first() else {
            return Ok(());
        };
        let mut condition = self.truth(Operand::temp(result.clone(), ty))?;
        if !is_and {
            condition = self.not(condition)?;
        }
        let then_block = self.block(|this| {
            let value = this.expr(next)?;
            let ty = value.ty;
            *all_bool &= ty == Ty::Bool;
            this.store(Place::Variable(result.clone()), value);
            this.bool_rest(rest, result, ty, is_and, all_bool)
        })?;
        self.emit_if(condition, then_block, Buf::default());
        Ok(())
    }

    /// One comparison, on operands already lowered.
    fn comparison(&mut self, op: ast::CmpOp, left: Operand, right: Operand) -> Lowering<Operand> {
        use ast::CmpOp;
        let (function, negated) = match op {
            CmpOp::Eq => ("eq", false),
            CmpOp::NotEq => ("eq", true),
            CmpOp::Lt => ("lt", false),
            CmpOp::LtE => ("le", false),
            CmpOp::Gt => ("gt", false),
            CmpOp::GtE => ("ge", false),
            CmpOp::Is => ("same", false),
            CmpOp::IsNot => ("same", true),
            CmpOp::In | CmpOp::NotIn => {
                let found = self.invoke("py.contains", &[right, left], Ty::Bool)?;
                if op == CmpOp::In {
                    return Ok(found);
                }
                return Ok(Operand::inline(self.not(found.expr)?, Ty::Bool));
            }
        };
        let mut call = self.native(function, vec![left.expr, right.expr])?;
        if negated {
            call = self.not(call)?;
        }
        Ok(Operand::inline(call, Ty::Bool))
    }

    fn compare(&mut self, compare: &ast::ExprCompare) -> Lowering<Operand> {
        let (Some(first_op), Some(first)) = (compare.ops.first(), compare.comparators.first())
        else {
            return self.expr(&compare.left);
        };
        let operands = self.operands(&[&compare.left, first])?;
        if compare.ops.len() == 1 {
            return self.comparison(*first_op, operands[0].clone(), operands[1].clone());
        }
        // `a < b < c`: `b` is evaluated once, and `c` only if `a < b`.
        let middle = self.pin(operands[1].clone());
        let holds = self.comparison(*first_op, operands[0].clone(), middle.clone())?;
        let result = self.temp();
        self.emit(Stmt::Let {
            name: result.clone(),
            value: Rhs::Expr(holds.expr),
        });
        self.compare_rest(
            middle,
            &compare.ops[1..],
            &compare.comparators[1..],
            &result,
        )?;
        Ok(Operand::temp(result, Ty::Bool))
    }

    fn compare_rest(
        &mut self,
        left: Operand,
        ops: &[ast::CmpOp],
        comparators: &[PyExpr],
        result: &Name,
    ) -> Lowering<()> {
        let (Some(op), Some(right)) = (ops.first(), comparators.first()) else {
            return Ok(());
        };
        let then_block = self.block(|this| {
            let mut right = this.expr(right)?;
            if ops.len() > 1 {
                right = this.pin(right);
            }
            let holds = this.comparison(*op, left, right.clone())?;
            this.store(Place::Variable(result.clone()), holds);
            this.compare_rest(right, &ops[1..], &comparators[1..], result)
        })?;
        self.emit_if(Expr::Variable(result.clone()), then_block, Buf::default());
        Ok(())
    }

    fn attribute(&mut self, attribute: &ast::ExprAttribute) -> Lowering<Operand> {
        let name = attribute.attr.id.as_str();
        // `type(x).__name__`.
        if name == "__name__"
            && let PyExpr::Call(call) = attribute.value.as_ref()
            && let PyExpr::Name(function) = call.func.as_ref()
            && function.id.as_str() == "type"
            && self.variable("type").is_none()
            && call.arguments.keywords.is_empty()
            && let [argument] = &*call.arguments.args
        {
            let value = self.expr(argument)?;
            return self.invoke("py.type", &[value], Ty::Str);
        }
        let helper = match name {
            "args" => Some("py.exc.args"),
            "__cause__" => Some("py.exc.cause"),
            _ => None,
        };
        match helper {
            Some(helper) if !self.is_asyncio(&attribute.value) => {
                let value = self.expr(&attribute.value)?;
                self.invoke(helper, &[value], Ty::Unknown)
            }
            _ => Err(diagnostics::refusal(
                Code::AttributeUnsupported,
                format!("the attribute `{name}` is not in the dialect"),
                attribute.range,
                "values have methods, and an exception has `args` and `__cause__`; keep named fields in a dict",
            )),
        }
    }

    fn comprehension(
        &mut self,
        collect: Collect<'_>,
        generators: &[ast::Comprehension],
    ) -> Lowering<Operand> {
        let (empty, ty) = match collect {
            Collect::List(_) => (Expr::List(Vec::new()), Ty::List),
            Collect::Set(_) => (Expr::Set(Vec::new()), Ty::Set),
            Collect::Dict(..) => (Expr::Map(Vec::new()), Ty::Dict),
        };
        let out = self.let_rhs(Rhs::Expr(empty), ty);
        let Expr::Variable(target) = out.expr.clone() else {
            return Ok(out);
        };
        let renames = self.renames.len();
        let result = self.generators(generators, &collect, &target);
        self.renames.truncate(renames);
        result.map(|()| out)
    }

    fn generators(
        &mut self,
        generators: &[ast::Comprehension],
        collect: &Collect<'_>,
        out: &Name,
    ) -> Lowering<()> {
        let Some((generator, rest)) = generators.split_first() else {
            return self.collect(collect, out);
        };
        if generator.is_async {
            return Err(diagnostics::refusal(
                Code::AsyncUnsupported,
                "`async for` in a comprehension is not in the dialect",
                generator.range,
                "await inside an ordinary `for` loop",
            ));
        }
        // The iterable is evaluated before the comprehension's own
        // variables exist.
        let iterable = self.expr(&generator.iter)?;
        let walked = self.invoke("py.iter", &[iterable], Ty::Unknown)?;
        let mut names = Vec::new();
        target_names(&generator.target, &mut names);
        for name in names {
            let renamed = self.fresh(&format!("{name}_"));
            self.emit(Stmt::Let {
                name: renamed.clone(),
                value: Rhs::Expr(Expr::Literal(Literal::Absent)),
            });
            self.renames.push((name, renamed));
        }
        let binding = self.temp();
        let body = self.block(|this| {
            this.assign(
                &generator.target,
                Operand::temp(binding.clone(), Ty::Unknown),
            )?;
            this.conditions(&generator.ifs, rest, collect, out)
        })?;
        self.emit_for(binding, walked.expr, body);
        Ok(())
    }

    fn conditions(
        &mut self,
        conditions: &[PyExpr],
        generators: &[ast::Comprehension],
        collect: &Collect<'_>,
        out: &Name,
    ) -> Lowering<()> {
        let Some((condition, rest)) = conditions.split_first() else {
            return self.generators(generators, collect, out);
        };
        let test = self.expr(condition)?;
        let holds = self.truth(test)?;
        let then_block = self.block(|this| this.conditions(rest, generators, collect, out))?;
        self.emit_if(holds, then_block, Buf::default());
        Ok(())
    }

    /// Adds one element to what a comprehension builds.
    fn collect(&mut self, collect: &Collect<'_>, out: &Name) -> Lowering<()> {
        let target = Expr::Variable(out.clone());
        match collect {
            Collect::List(element) => {
                let value = self.expr(element)?;
                let end = self.native("list.len", vec![target.clone()])?;
                self.store(Place::Member(Member::Index { target, index: end }), value);
            }
            Collect::Set(element) => {
                let value = self.expr(element)?;
                self.emit(Stmt::Assign {
                    place: Place::Member(Member::Index {
                        target,
                        index: value.expr,
                    }),
                    value: Rhs::Expr(Expr::Literal(Literal::Bool(true))),
                });
            }
            Collect::Dict(key, value) => {
                let operands = self.operands(&[key, value])?;
                let key = self.pin(operands[0].clone());
                self.emit(Stmt::Assign {
                    place: Place::Member(Member::Index {
                        target,
                        index: key.expr,
                    }),
                    value: Rhs::Expr(operands[1].expr.clone()),
                });
            }
        }
        Ok(())
    }

    pub(super) fn tuple_of(operands: &[Operand]) -> Operand {
        Operand::inline(
            Expr::Tuple(
                operands
                    .iter()
                    .map(|operand| operand.expr.clone())
                    .collect(),
            ),
            Ty::Tuple,
        )
    }
}

/// The names an assignment target binds.
fn target_names(target: &PyExpr, out: &mut Vec<String>) {
    match target {
        PyExpr::Name(name) => out.push(name.id.as_str().to_string()),
        PyExpr::Tuple(tuple) => tuple.elts.iter().for_each(|elt| target_names(elt, out)),
        PyExpr::List(list) => list.elts.iter().for_each(|elt| target_names(elt, out)),
        _ => {}
    }
}

fn number(value: &ast::Number, range: TextRange) -> Lowering<Operand> {
    match value {
        ast::Number::Int(value) => {
            let integer = match value.as_u64() {
                Some(small) => Some(Integer::new(small)),
                None => Integer::parse(&value.to_string()).ok(),
            };
            match integer {
                Some(integer) => Ok(Operand::literal(Literal::Int(integer), Ty::Int)),
                None => Err(diagnostics::refusal(
                    Code::LiteralUnsupported,
                    "an integer this large is written in decimal here",
                    range,
                    "write the number in base ten",
                )),
            }
        }
        ast::Number::Float(value) => Ok(Operand::literal(
            Literal::Float(Float::new(*value)),
            Ty::Float,
        )),
        ast::Number::Complex { .. } => Err(diagnostics::refusal(
            Code::LiteralUnsupported,
            "complex numbers are not in the dialect",
            range,
            "keep the real and imaginary parts as two floats",
        )),
    }
}

fn generator(range: TextRange) -> lash_kernel_dialect::Diagnostic {
    diagnostics::refusal(
        Code::GeneratorUnsupported,
        "generators are not in the dialect",
        range,
        "build and return a list",
    )
}
