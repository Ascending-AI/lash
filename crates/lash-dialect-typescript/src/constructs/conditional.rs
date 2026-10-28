//! A statement-valued conditional and its result binding share one recipe.
use lash_kernel_doc::{AssignOrder, Expr, Literal, Place, Rhs, Stmt};
use lash_kernel_value::Name;

pub(crate) const CONDITIONAL: Conditional = Conditional;
pub(crate) struct Conditional;
pub(crate) struct Choice<'a> {
    pub name: &'a Name,
    pub condition: &'a Expr,
    pub then: Arm<'a>,
    pub otherwise: Arm<'a>,
}
pub(crate) struct Arm<'a> {
    pub prelude: &'a [Stmt],
    pub value: &'a Rhs,
}
impl Conditional {
    pub fn binding(&self, name: Name) -> Stmt {
        Stmt::Let {
            name,
            value: Rhs::Expr(Expr::Literal(Literal::Absent)),
        }
    }
    pub fn destination(&self, name: &Name) -> Place {
        Place::Variable(name.clone())
    }
    pub fn recognize<'a>(&self, block: &'a [Stmt]) -> Option<Choice<'a>> {
        let [
            Stmt::Let { name, value },
            Stmt::If {
                condition,
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return None;
        };
        if *value != Rhs::Expr(Expr::Literal(Literal::Absent)) {
            return None;
        }
        let arm = |block: &'a [Stmt]| {
            let [
                prelude @ ..,
                Stmt::Assign {
                    place,
                    value,
                    order: AssignOrder::ValueFirst,
                },
            ] = block
            else {
                return None;
            };
            (*place == self.destination(name)).then_some(Arm { prelude, value })
        };
        Some(Choice {
            name,
            condition,
            then: arm(then_block)?,
            otherwise: arm(else_block)?,
        })
    }
}
