//! Rebuilding an [`Expr`] tree bottom-up: the [`ExprFolder`] a pass
//! implements, and the child-by-child rebuild every folder falls back to.

use super::*;

pub trait ExprFolder {
    fn fold_expr(&mut self, expr: Expr) -> Expr {
        fold_expr_children(self, expr)
    }
}

pub fn fold_expr_children<F>(folder: &mut F, expr: Expr) -> Expr
where
    F: ExprFolder + ?Sized,
{
    match expr {
        Expr::Block(expressions) => Expr::Block(
            expressions
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::LabelAnnotated { label, expr } => Expr::LabelAnnotated {
            label,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Tuple(items) => Expr::Tuple(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::List(items) => Expr::List(
            items
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        ),
        Expr::ListComprehension { element, clauses } => Expr::ListComprehension {
            element: Box::new(folder.fold_expr(*element)),
            clauses: clauses
                .into_iter()
                .map(|clause| fold_list_comprehension_clause(folder, clause))
                .collect(),
        },
        Expr::Record(entries) => Expr::Record(
            entries
                .into_iter()
                .map(|(name, value)| (name, folder.fold_expr(value)))
                .collect(),
        ),
        Expr::Assign { target, expr } => Expr::Assign {
            target: fold_assign_target(folder, target),
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::If {
            condition,
            then_block,
            else_block,
        } => Expr::If {
            condition: Box::new(folder.fold_expr(*condition)),
            then_block: Box::new(folder.fold_expr(*then_block)),
            else_block: Box::new(folder.fold_expr(*else_block)),
        },
        Expr::For {
            binding,
            iterable,
            bind,
            body,
        } => Expr::For {
            binding,
            iterable: Box::new(folder.fold_expr(*iterable)),
            bind: bind.map(|bind| Box::new(folder.fold_expr(*bind))),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::Role { role, expr } => Expr::Role {
            role,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::While { condition, body } => Expr::While {
            condition: Box::new(folder.fold_expr(*condition)),
            body: Box::new(folder.fold_expr(*body)),
        },
        Expr::ProcessRef { process } => Expr::ProcessRef { process },
        Expr::HostDescriptorConstructor { type_name, input } => Expr::HostDescriptorConstructor {
            type_name,
            input: Box::new(folder.fold_expr(*input)),
        },
        Expr::ReceiverCall {
            receiver,
            operation,
            args,
        } => Expr::ReceiverCall {
            receiver: Box::new(folder.fold_expr(*receiver)),
            operation,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Await(expr) => Expr::Await(Box::new(folder.fold_expr(*expr))),
        Expr::SleepFor(expr) => Expr::SleepFor(Box::new(folder.fold_expr(*expr))),
        Expr::SleepUntil(expr) => Expr::SleepUntil(Box::new(folder.fold_expr(*expr))),
        Expr::ResultUnwrap(expr) => Expr::ResultUnwrap(Box::new(folder.fold_expr(*expr))),
        Expr::Print(expr) => Expr::Print(Box::new(folder.fold_expr(*expr))),
        Expr::Yield(expr) => Expr::Yield(Box::new(folder.fold_expr(*expr))),
        Expr::Finish(expr) => Expr::Finish(Box::new(folder.fold_expr(*expr))),
        Expr::Fail(expr) => Expr::Fail(Box::new(folder.fold_expr(*expr))),
        Expr::BuiltinCall { name, args } => Expr::BuiltinCall {
            name,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::FunctionCall { function, args } => Expr::FunctionCall {
            function,
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Function(function) => Expr::Function(Box::new(FunctionExpr {
            name: function.name,
            js_name: function.js_name,
            receiver: function.receiver,
            params: function.params,
            captures: function.captures,
            body: Box::new(folder.fold_expr(*function.body)),
        })),
        Expr::ProcessLiteral(literal) => Expr::ProcessLiteral(Box::new(ProcessLiteralExpr {
            params: literal.params,
            hidden_args: literal.hidden_args,
            body: Box::new(folder.fold_expr(*literal.body)),
        })),
        Expr::Call { function, args } => Expr::Call {
            function: Box::new(folder.fold_expr(*function)),
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::MethodCall {
            receiver,
            method,
            args,
        } => Expr::MethodCall {
            receiver: Box::new(folder.fold_expr(*receiver)),
            method: match method {
                MethodKey::Field(field) => MethodKey::Field(field),
                MethodKey::Index(key) => MethodKey::Index(Box::new(folder.fold_expr(*key))),
            },
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::ThisCall {
            this,
            function,
            args,
        } => Expr::ThisCall {
            this: Box::new(folder.fold_expr(*this)),
            function: Box::new(folder.fold_expr(*function)),
            args: args
                .into_iter()
                .map(|expr| folder.fold_expr(expr))
                .collect(),
        },
        Expr::Map { items, function } => Expr::Map {
            items: Box::new(folder.fold_expr(*items)),
            function: Box::new(folder.fold_expr(*function)),
        },
        Expr::Try(scope) => Expr::Try(Box::new(TryExpr {
            body: Box::new(folder.fold_expr(*scope.body)),
            catch: scope.catch.map(|catch| CatchClause {
                binding: catch.binding,
                body: Box::new(folder.fold_expr(*catch.body)),
            }),
            finally: scope
                .finally
                .map(|finally| Box::new(folder.fold_expr(*finally))),
        })),
        Expr::Throw(value) => Expr::Throw(Box::new(folder.fold_expr(*value))),
        Expr::Return(value) => Expr::Return(Box::new(folder.fold_expr(*value))),
        Expr::Field { target, field } => Expr::Field {
            target: Box::new(folder.fold_expr(*target)),
            field,
        },
        Expr::Index { target, index } => Expr::Index {
            target: Box::new(folder.fold_expr(*target)),
            index: Box::new(folder.fold_expr(*index)),
        },
        Expr::Unary { op, expr } => Expr::Unary {
            op,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::Binary { left, op, right } => Expr::Binary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        Expr::JavaScriptUnary { op, expr } => Expr::JavaScriptUnary {
            op,
            expr: Box::new(folder.fold_expr(*expr)),
        },
        Expr::JavaScriptBinary { left, op, right } => Expr::JavaScriptBinary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        Expr::JavaScriptLogical { left, op, right } => Expr::JavaScriptLogical {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
        },
        leaf @ (Expr::Null
        | Expr::Undefined
        | Expr::Bool(_)
        | Expr::Number(_)
        | Expr::String(_)
        | Expr::Variable(_)
        | Expr::Break
        | Expr::Continue
        | Expr::ResourceRef(_)
        | Expr::WaitSignal { .. }
        | Expr::TypeLiteral(_)) => leaf,
    }
}

fn fold_list_comprehension_clause<F>(
    folder: &mut F,
    clause: ListComprehensionClause,
) -> ListComprehensionClause
where
    F: ExprFolder + ?Sized,
{
    match clause {
        ListComprehensionClause::For { binding, iterable } => ListComprehensionClause::For {
            binding,
            iterable: folder.fold_expr(iterable),
        },
        ListComprehensionClause::If { condition } => ListComprehensionClause::If {
            condition: folder.fold_expr(condition),
        },
    }
}

fn fold_assign_target<F>(folder: &mut F, target: AssignTarget) -> AssignTarget
where
    F: ExprFolder + ?Sized,
{
    AssignTarget {
        root: target.root,
        steps: target
            .steps
            .into_iter()
            .map(|step| match step {
                AssignPathStep::Field(field) => AssignPathStep::Field(field),
                AssignPathStep::Index(index) => AssignPathStep::Index(folder.fold_expr(index)),
            })
            .collect(),
    }
}
