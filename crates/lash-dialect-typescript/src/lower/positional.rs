//! Packed arguments are needed when the body or an enclosing-arrow default
//! reads `arguments`. Conservatively include nested ordinary functions too.
use crate::adapter::{
    self as ast, ArrayElement, AssignTarget, CallArg, Expr, MemberProperty, ObjectProperty,
    OptionalOperation, Pattern, PropertyKey, Stmt,
};

pub(super) fn eligible(function: &ast::Function) -> bool {
    !function.is_arrow
        && callback_eligible(function)
        && function.params.iter().all(|p| match p {
            Pattern::Ident(..) => true,
            Pattern::Assign { target, .. } => matches!(target.as_ref(), Pattern::Ident(..)),
            _ => false,
        })
}

/// A callback can share the surrounding arrow receiver, but cannot observe
/// an argument list or bind a rest parameter.
pub(super) fn callback_eligible(function: &ast::Function) -> bool {
    !function.is_async
        && !function
            .params
            .iter()
            .any(|p| matches!(p, Pattern::Rest(_)))
        && !function_reads_arguments(function)
}

fn function_reads_arguments(function: &ast::Function) -> bool {
    function.params.iter().any(pattern_reads_arguments)
        || match &function.body {
            ast::FunctionBody::Block(body) => body.iter().any(statement_reads_arguments),
            ast::FunctionBody::Expression(expr) => reads_arguments(expr),
        }
}

pub(super) fn statement_reads_arguments(statement: &Stmt) -> bool {
    match statement {
        Stmt::Spanned(_, inner) => statement_reads_arguments(inner),
        Stmt::Empty | Stmt::TypeAlias { .. } | Stmt::Break | Stmt::Continue => false,
        Stmt::Function { function, .. } => function_reads_arguments(function),
        Stmt::Expr(expr) | Stmt::Throw(expr) => reads_arguments(expr),
        Stmt::Block(body) => body.iter().any(statement_reads_arguments),
        Stmt::Var { declarations, .. } => declarations.iter().any(|declaration| {
            pattern_reads_arguments(&declaration.pattern)
                || declaration.init.as_ref().is_some_and(reads_arguments)
        }),
        Stmt::Enum { members, .. } => members.iter().any(|member| reads_arguments(&member.value)),
        Stmt::Return(value) => value.as_ref().is_some_and(reads_arguments),
        Stmt::If {
            test,
            consequent,
            alternate,
        } => {
            reads_arguments(test)
                || statement_reads_arguments(consequent)
                || alternate
                    .as_ref()
                    .is_some_and(|alternate| statement_reads_arguments(alternate))
        }
        Stmt::While { test, body } | Stmt::DoWhile { body, test, .. } => {
            reads_arguments(test) || statement_reads_arguments(body)
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            init.as_ref()
                .is_some_and(|init| statement_reads_arguments(init))
                || test.as_ref().is_some_and(reads_arguments)
                || update.as_ref().is_some_and(reads_arguments)
                || statement_reads_arguments(body)
        }
        Stmt::ForOf {
            pattern,
            iterable: subject,
            body,
            ..
        }
        | Stmt::ForIn {
            pattern,
            object: subject,
            body,
            ..
        } => {
            pattern_reads_arguments(pattern)
                || reads_arguments(subject)
                || statement_reads_arguments(body)
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            reads_arguments(discriminant)
                || cases.iter().any(|case| {
                    case.test.as_ref().is_some_and(reads_arguments)
                        || case.consequent.iter().any(statement_reads_arguments)
                })
        }
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            body.iter().any(statement_reads_arguments)
                || catch.as_ref().is_some_and(|catch| {
                    catch.binding.as_ref().is_some_and(pattern_reads_arguments)
                        || catch.body.iter().any(statement_reads_arguments)
                })
                || finally
                    .as_ref()
                    .is_some_and(|finally| finally.iter().any(statement_reads_arguments))
        }
    }
}

fn pattern_reads_arguments(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Ident(name, _) => name == "arguments",
        Pattern::Rest(inner) => pattern_reads_arguments(inner),
        Pattern::Member { object, property } => {
            reads_arguments(object) || property_reads_arguments(property)
        }
        Pattern::Assign { target, default } => {
            pattern_reads_arguments(target) || reads_arguments(default)
        }
        Pattern::Array { elements, rest } => {
            elements.iter().flatten().any(pattern_reads_arguments)
                || rest
                    .as_ref()
                    .is_some_and(|rest| pattern_reads_arguments(rest))
        }
        Pattern::Object { properties, rest } => {
            properties.iter().any(|property| {
                key_reads_arguments(&property.key) || pattern_reads_arguments(&property.value)
            }) || rest
                .as_ref()
                .is_some_and(|rest| pattern_reads_arguments(rest))
        }
    }
}

fn property_reads_arguments(property: &MemberProperty) -> bool {
    match property {
        MemberProperty::Field(_) => false,
        MemberProperty::Index(index) => reads_arguments(index),
    }
}

fn key_reads_arguments(key: &PropertyKey) -> bool {
    match key {
        PropertyKey::Static(_) => false,
        PropertyKey::Computed(key) => reads_arguments(key),
    }
}

fn target_reads_arguments(target: &AssignTarget) -> bool {
    match target {
        AssignTarget::Ident(_) | AssignTarget::ParenIdent(_) => false,
        AssignTarget::Member { object, property } => {
            reads_arguments(object) || property_reads_arguments(property)
        }
        AssignTarget::Pattern(pattern) => pattern_reads_arguments(pattern),
    }
}

fn args_read_arguments(args: &[CallArg]) -> bool {
    args.iter().any(|arg| match arg {
        CallArg::Value(value) | CallArg::Spread(value) => reads_arguments(value),
    })
}

fn reads_arguments(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => function_reads_arguments(function),
        Expr::Null
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::RegExp { .. }
        | Expr::This
        | Expr::LoneSurrogateString => false,
        Expr::Ident(name, _) => name == "arguments",
        Expr::Array(elements) => elements.iter().any(|element| match element {
            ArrayElement::Value(value) | ArrayElement::Spread(value) => reads_arguments(value),
            ArrayElement::Hole => false,
        }),
        Expr::Object(properties) => properties.iter().any(|property| match property {
            ObjectProperty::KeyValue(key, value) => {
                key_reads_arguments(key) || reads_arguments(value)
            }
            ObjectProperty::Spread(value) => reads_arguments(value),
        }),
        Expr::Assign { target, value, .. } => {
            target_reads_arguments(target) || reads_arguments(value)
        }
        Expr::Member {
            object, property, ..
        }
        | Expr::Delete { object, property } => {
            reads_arguments(object) || property_reads_arguments(property)
        }
        Expr::Unary { value, .. } | Expr::Await { value, .. } | Expr::As { value, .. } => {
            reads_arguments(value)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            reads_arguments(left) || reads_arguments(right)
        }
        Expr::Conditional {
            test,
            consequent,
            alternate,
        } => reads_arguments(test) || reads_arguments(consequent) || reads_arguments(alternate),
        Expr::Template { expressions, .. } => expressions.iter().any(reads_arguments),
        Expr::Call { callee, args, .. } => reads_arguments(callee) || args_read_arguments(args),
        Expr::New { args, .. } => args_read_arguments(args),
        Expr::OptionalChain { base, operations } => {
            reads_arguments(base)
                || operations.iter().any(|operation| match operation {
                    OptionalOperation::Member { property, .. } => {
                        property_reads_arguments(property)
                    }
                    OptionalOperation::Call { args, .. } => args_read_arguments(args),
                })
        }
        Expr::Update { target, .. } => target_reads_arguments(target),
    }
}

impl super::Lowerer<'_> {
    /// Only a proven list method and a proven function body can bypass the
    /// token's packed entry. Unknown or reassigned values use the normal call.
    pub(super) fn call_array_callback(
        &mut self,
        object: &super::Operand,
        method: &str,
        callback: &ast::Expr,
        arguments: &[&ast::Expr],
    ) -> super::Lowering<Option<super::Operand>> {
        use super::{Operand, Ty};
        use lash_kernel_doc::{ActKind, Callee, Expr as KExpr, Stmt as KStmt};
        use lash_vocabulary::Name;
        let Some((helper, arity)) = crate::builtins::array_callback(method) else {
            return Ok(None);
        };
        let callback = match callback {
            ast::Expr::Function(function)
                if function.name.is_none() && callback_eligible(function) =>
            {
                self.callback_closure(function, arity)?.0
            }
            ast::Expr::Ident(name, _) => {
                let Some(entry) = self
                    .binding_mut(name)
                    .and_then(|(_, binding)| binding.positional.clone())
                else {
                    return Ok(None);
                };
                // Read the binding now, before later argument effects. Its
                // stable private entry carries the identical function body.
                let value = self.lower_expr(callback)?;
                self.discard(value);
                if entry.arity >= arity {
                    Operand::variable(entry.name, Ty::Unknown)
                } else {
                    let params: Vec<Name> = (0..=arity).map(|_| self.fresh("operand")).collect();
                    let body = self.block(|lowerer| {
                        let value = lowerer.action(
                            ActKind::Call(Callee::Value(entry.name)),
                            params[..=entry.arity]
                                .iter()
                                .cloned()
                                .map(KExpr::Variable)
                                .collect(),
                            Ty::Unknown,
                        );
                        lowerer.emit(KStmt::Return {
                            value: value.expr(),
                        });
                        Ok(())
                    })?;
                    self.emit_closure(params, body, None)
                }
            }
            _ => return Ok(None),
        };
        let callback = self.pin(callback);
        let values = self.operands(&arguments[1..])?;
        let mut context = Operand::undefined();
        for (index, value) in values.into_iter().enumerate() {
            if index == 0 && method != "sort" {
                context = self.pin(value);
            } else {
                self.discard(value);
            }
        }
        let length = self.invoke("ts.array.length", std::slice::from_ref(object), Ty::Unknown)?;
        let mut passed = vec![object.clone(), length];
        if method != "sort" {
            passed.push(context);
        }
        passed.push(callback);
        Ok(Some(self.invoke(helper, &passed, Ty::Unknown)?))
    }
}
