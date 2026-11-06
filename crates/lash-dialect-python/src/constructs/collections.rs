//! Collection allocation and the canonical simple-comprehension recipe.
use crate::scope::Ty;
use lash_kernel_dialect::construct::Expression as E;
use lash_kernel_doc::{self as k, Expr, Rhs, Stmt};
use lash_kernel_value::{FunctionId, Name};

#[derive(Clone, Copy)]
pub(crate) enum Collection {
    List,
    Set,
    Dict,
}
impl Collection {
    pub fn allocation(self) -> (Expr, Ty) {
        match self {
            Self::List => (Expr::List(Vec::new()), Ty::List),
            Self::Set => (Expr::Set(Vec::new()), Ty::Set),
            Self::Dict => (Expr::Map(Vec::new()), Ty::Dict),
        }
    }
}
pub(crate) struct ComprehensionRecipe {
    pub append_index: E,
    separator: char,
}
pub(crate) const COMPREHENSION: ComprehensionRecipe = ComprehensionRecipe {
    append_index: E::Call {
        function: "list.len",
        args: &[E::Input(0)],
    },
    separator: '_',
};
pub(crate) struct Comprehension<'a> {
    pub result: &'a Name,
    pub binding: &'a Name,
    pub source_binding: Name,
    pub iterable: &'a Expr,
    pub via: Option<&'a k::Protocol>,
    pub value: &'a Rhs,
    pub key: Option<&'a Expr>,
    pub collection: Collection,
    pub rest: &'a [Stmt],
}
impl ComprehensionRecipe {
    pub fn binding_base(&self, name: &str) -> String {
        format!("{name}{}", self.separator)
    }
    pub fn recognize<'a>(
        &self,
        statements: &'a [Stmt],
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<Comprehension<'a>> {
        let [
            Stmt::Let {
                name,
                value: Rhs::Expr(allocation),
            },
            Stmt::For {
                binding,
                iterable,
                via,
                body,
                else_block,
                metered: 0,
            },
            rest @ ..,
        ] = statements
        else {
            return None;
        };
        let collection = match allocation {
            Expr::List(empty) if empty.is_empty() => Collection::List,
            Expr::Set(empty) if empty.is_empty() => Collection::Set,
            Expr::Map(empty) if empty.is_empty() => Collection::Dict,
            _ => return None,
        };
        if !else_block.is_empty() {
            return None;
        }
        let (key, body) = match (collection, body.as_slice()) {
            (Collection::List, body) => (None, body),
            (
                _,
                [
                    Stmt::Let {
                        name: held,
                        value: Rhs::Expr(key),
                    },
                    assignment @ Stmt::Assign {
                        place:
                            k::Place::Member(k::Member::Index {
                                index: Expr::Variable(index),
                                ..
                            }),
                        ..
                    },
                ],
            ) if held == index => (Some(key), std::slice::from_ref(assignment)),
            _ => return None,
        };
        let [
            Stmt::Assign {
                place:
                    k::Place::Member(k::Member::Index {
                        target: Expr::Variable(target),
                        index,
                    }),
                value,
                order: k::AssignOrder::ValueFirst,
            },
        ] = body
        else {
            return None;
        };
        if target != name
            || (matches!(collection, Collection::List)
                && self.append_index.recognize(index, is)?.as_slice()
                    != [&Expr::Variable(name.clone())])
        {
            return None;
        }
        let (source, suffix) = binding.as_str().rsplit_once(self.separator)?;
        if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        Some(Comprehension {
            result: name,
            binding,
            source_binding: Name::new(source),
            iterable,
            via: via.as_ref(),
            value,
            key,
            collection,
            rest,
        })
    }
}
