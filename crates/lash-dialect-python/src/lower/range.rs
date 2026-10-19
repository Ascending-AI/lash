//! FIG-6160: builtin range iteration uses an integer counter, never a list.

use lash_kernel_doc::{Expr, Integer, Literal, Place, Rhs};
use ruff_python_ast as ast;

use super::{Buf, Lowerer, Lowering, Operand};
use crate::scope::Ty;

impl Lowerer<'_> {
    pub(super) fn range_loop(
        &mut self,
        for_loop: &ast::StmtFor,
        call: &ast::ExprCall,
        span: Option<lash_kernel_doc::Span>,
    ) -> Lowering<()> {
        let args = self.positional("range", call, 1, 3)?;
        // Pin even the last argument: the body may rebind any source name.
        let args: Vec<_> = args.into_iter().map(|arg| self.pin(arg)).collect();
        let zero = Operand::literal(Literal::Int(Integer::from(0)), Ty::Int);
        let one = Operand::literal(Literal::Int(Integer::from(1)), Ty::Int);
        let (start, stop, step) = match args.as_slice() {
            [stop] => (zero.clone(), stop.clone(), one),
            [start, stop] => (start.clone(), stop.clone(), one),
            [start, stop, step] => (start.clone(), stop.clone(), step.clone()),
            _ => unreachable!("positional checks the range arity"),
        };
        // Match py.range's validation order and errors, including on empty
        // ranges. An annotation alone is not proof of the runtime kind.
        for bound in [&start, &stop, &step] {
            let kind = self.native("kind", vec![bound.expr.clone()])?;
            let integer = self.native(
                "same",
                vec![kind, Expr::Literal(Literal::Text("integer".into()))],
            )?;
            let invalid = self.not(integer)?;
            let failure = self.block(|this| {
                let name = this.invoke("py.type", std::slice::from_ref(bound), Ty::Str)?;
                let message = this.native(
                    "text.join",
                    vec![
                        Expr::List(vec![
                            Operand::text("'").expr,
                            name.expr,
                            Operand::text("' object cannot be interpreted as an integer").expr,
                        ]),
                        Operand::text("").expr,
                    ],
                )?;
                this.invoke_do(
                    "py.fail",
                    &[
                        Operand::text("TypeError"),
                        Operand::inline(message, Ty::Str),
                    ],
                )
            })?;
            self.emit_if(invalid, failure, Buf::default());
        }
        let is_zero = self.native("eq", vec![step.expr.clone(), zero.expr.clone()])?;
        let failure = self.block(|this| {
            this.invoke_do(
                "py.fail",
                &[
                    Operand::text("ValueError"),
                    Operand::text("range() arg 3 must not be zero"),
                ],
            )
        })?;
        self.emit_if(is_zero, failure, Buf::default());

        // Admission does not refine a value's type from kind checks. On the
        // successful path these calls are identities, and expose Int results
        // without rejecting a statically non-int argument before py.fail runs.
        let mut integer = |bound: Operand| {
            if matches!(bound.expr, Expr::Literal(Literal::Int(_))) {
                Ok(bound)
            } else {
                self.invoke("py.int", &[bound], Ty::Int)
            }
        };
        let start = integer(start)?;
        let stop = integer(stop)?;
        let step = integer(step)?;

        let counter = self.let_rhs(Rhs::Expr(start.expr), Ty::Int);
        let Expr::Variable(counter_name) = &counter.expr else {
            unreachable!("a temporary is a variable");
        };
        // The usual omitted step needs just one comparison per iteration.
        // For a dynamic step, select the comparison once per loop test.
        let direction = match &step.expr {
            Expr::Literal(Literal::Int(value)) => Some(!value.is_negative()),
            _ => None,
        };
        let positive = if direction.is_none() {
            let expr = self.native("int.gt", vec![step.expr.clone(), zero.expr])?;
            Some(self.let_rhs(Rhs::Expr(expr), Ty::Bool))
        } else {
            None
        };
        let test = |this: &mut Self| match (direction, &positive) {
            (Some(positive), _) => this.native(
                if positive { "int.lt" } else { "int.gt" },
                vec![counter.expr.clone(), stop.expr.clone()],
            ),
            (None, Some(positive)) => {
                let ascending =
                    this.native("int.lt", vec![counter.expr.clone(), stop.expr.clone()])?;
                let more = this.let_rhs(Rhs::Expr(ascending), Ty::Bool);
                let Expr::Variable(more_name) = &more.expr else {
                    unreachable!("a temporary is a variable");
                };
                let descending =
                    this.native("int.gt", vec![counter.expr.clone(), stop.expr.clone()])?;
                let descends = this.block(|this| {
                    this.store(
                        Place::Variable(more_name.clone()),
                        Operand::inline(descending, Ty::Bool),
                    );
                    Ok(())
                })?;
                let negative = this.not(positive.expr.clone())?;
                this.emit_if(negative, descends, Buf::default());
                Ok(more.expr)
            }
            (None, None) => unreachable!("a dynamic step has its direction"),
        };
        let body = |this: &mut Self| {
            this.assign(&for_loop.target, counter.clone())?;
            // Advance before the body so continue (including through finally)
            // cannot skip it, and rebinding the Python target cannot change it.
            let next = this.native("int.add", vec![counter.expr.clone(), step.expr.clone()])?;
            this.store(
                Place::Variable(counter_name.clone()),
                Operand::inline(next, Ty::Int),
            );
            this.statements(&for_loop.body)
        };
        let else_caption = self.loop_else_caption(&for_loop.body, &for_loop.orelse);
        self.python_while(test, body, &for_loop.orelse, else_caption, span)
    }
}
