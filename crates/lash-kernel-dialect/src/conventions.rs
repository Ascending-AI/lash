//! Dialect-owned construction and recognition recipes used by canvas admission.
use lash_kernel_doc::{
    self as k, ActExpr, ActKind, Callee, Expr, FunctionCatalog, FunctionTarget, Literal, Protocol,
};
use lash_kernel_value::FunctionId;

#[derive(Clone, Copy, Debug)]
pub struct Iteration {
    pub open: &'static str,
    pub next: Option<&'static str>,
    pub item: Option<&'static str>,
    pub check: Option<&'static str>,
}
impl Iteration {
    pub fn resolve<E>(
        &self,
        mut resolve: impl FnMut(&str) -> Result<FunctionTarget, E>,
    ) -> Result<Protocol, E> {
        Ok(Protocol {
            open: resolve(self.open)?,
            next: self.next.map(&mut resolve).transpose()?,
            item: self.item.map(&mut resolve).transpose()?,
            check: self.check.map(&mut resolve).transpose()?,
        })
    }
    pub fn matches(
        &self,
        protocol: &Protocol,
        mut is: impl FnMut(&FunctionTarget, &str) -> bool,
    ) -> bool {
        let mut hook = |target: Option<&FunctionTarget>, name: Option<&str>| match (target, name) {
            (Some(target), Some(name)) => is(target, name),
            (None, None) => true,
            _ => false,
        };
        hook(Some(&protocol.open), Some(self.open))
            && hook(protocol.next.as_ref(), self.next)
            && hook(protocol.item.as_ref(), self.item)
            && hook(protocol.check.as_ref(), self.check)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Boundary {
    Identity,
    /// Call the selected helper on the value, followed by fixed operands.
    Call {
        helper: &'static str,
        role: Option<k::ProjectionRole>,
        tail: &'static [BoundaryOperand],
        literals_passthrough: bool,
    },
}
#[derive(Clone, Copy, Debug)]
pub enum BoundaryOperand {
    Absent,
}
impl BoundaryOperand {
    fn expression(self) -> Expr {
        match self {
            Self::Absent => Expr::Literal(Literal::Absent),
        }
    }
}
impl Boundary {
    pub fn wrap(
        &self,
        value: Expr,
        mut resolve: impl FnMut(&str) -> Option<FunctionId>,
    ) -> Option<Expr> {
        match self {
            Self::Identity => Some(value),
            Self::Call {
                literals_passthrough: true,
                ..
            } if matches!(value, Expr::Literal(_)) => Some(value),
            Self::Call { helper, tail, .. } => {
                let mut args = vec![value];
                args.extend(tail.iter().copied().map(BoundaryOperand::expression));
                Some(Expr::Act(Box::new(ActExpr {
                    kind: ActKind::Call(Callee::Local(resolve(helper)?)),
                    args,
                })))
            }
        }
    }
    pub fn unwrap<'a>(
        &self,
        value: &'a Expr,
        mut is: impl FnMut(&FunctionId, &str) -> bool,
    ) -> Option<&'a Expr> {
        match (self, value) {
            (Self::Identity, value) => Some(value),
            (Self::Call { helper, tail, .. }, Expr::Act(act)) => {
                let ActKind::Call(Callee::Local(id)) = &act.kind else {
                    return None;
                };
                (is(id, helper)
                    && act.args.len() == tail.len() + 1
                    && act.args[1..]
                        .iter()
                        .zip(*tail)
                        .all(|(found, expected)| *found == expected.expression()))
                .then(|| &act.args[0])
            }
            _ => None,
        }
    }
    pub fn resolve(&self, catalog: &dyn FunctionCatalog, name: &str) -> Option<FunctionId> {
        match self {
            Self::Call {
                role: Some(role), ..
            } => catalog.projection_helper(*role),
            _ => catalog.named_helper(name),
        }
    }
    pub fn helper(&self) -> Option<&'static str> {
        match self {
            Self::Identity => None,
            Self::Call { helper, .. } => Some(helper),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Conventions {
    pub language: Option<k::SourceLanguage>,
    pub matches_start: fn(&k::StartConvention) -> bool,
    pub helper_prefix: &'static str,
    pub iteration: Option<Iteration>,
    pub argument: Boundary,
    pub reply: Boundary,
    pub then_yield: bool,
}
impl Conventions {
    pub const KERNEL: Self = Self {
        language: None,
        matches_start: |_| false,
        helper_prefix: "",
        iteration: None,
        argument: Boundary::Identity,
        reply: Boundary::Identity,
        then_yield: false,
    };
    /// Export evidence has priority over helper evidence, in document order.
    pub fn of<'a>(
        document: &k::Document,
        catalog: &dyn FunctionCatalog,
        installed: &'a [Self],
    ) -> &'a Self {
        for export in document.exports.values() {
            if let k::Export::Function(function) = export
                && let Some(conventions) = installed
                    .iter()
                    .find(|entry| (entry.matches_start)(&function.convention))
            {
                return conventions;
            }
        }
        for id in document.library.iter() {
            if let Some(header) = catalog.header(id)
                && let Some(conventions) = installed.iter().find(|entry| {
                    !entry.helper_prefix.is_empty()
                        && header.name.as_str().starts_with(entry.helper_prefix)
                })
            {
                return conventions;
            }
        }
        &Self::KERNEL
    }
}
