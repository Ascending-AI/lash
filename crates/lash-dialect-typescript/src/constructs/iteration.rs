//! Property enumeration emits a key snapshot and visits keys still present.
use lash_kernel_doc::{self as k, Expr, Rhs, Stmt};
use lash_kernel_value::{FunctionId, Name};

pub(crate) const FOR_IN: ForIn = ForIn {
    keys: "ts.keys",
    present: "ts.has",
};
pub(crate) struct ForIn {
    pub keys: &'static str,
    pub present: &'static str,
}
pub(crate) struct Enumeration<'a> {
    pub subject: &'a Expr,
    pub binding: &'a Name,
    pub body: &'a [Stmt],
}
impl ForIn {
    pub fn recognize<'a>(
        &self,
        iterable: &'a Expr,
        item: &Name,
        body: &'a [Stmt],
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<Enumeration<'a>> {
        let arguments = |value: &'a Expr, helper| match value {
            Expr::Act(act) => match &act.kind {
                k::ActKind::Call(k::Callee::Local(id)) if is(id, helper) => {
                    Some(act.args.as_slice())
                }
                _ => None,
            },
            Expr::Call { function, args } if is(function, helper) => Some(args.as_slice()),
            _ => None,
        };
        let [subject] = arguments(iterable, self.keys)? else {
            return None;
        };
        let [
            Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = body
        else {
            return None;
        };
        if !else_block.is_empty()
            || arguments(condition, self.present)?
                != [subject.clone(), Expr::Variable(item.clone())]
        {
            return None;
        }
        let [
            Stmt::Let {
                name,
                value: Rhs::Expr(Expr::Variable(from)),
            },
            rest @ ..,
        ] = then_block.as_slice()
        else {
            return None;
        };
        (from == item).then_some(Enumeration {
            subject,
            binding: name,
            body: rest,
        })
    }
}
