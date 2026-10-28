//! Guard forms used by authored member reads and their structural inverse.
use lash_kernel_dialect::construct::{Atom, Expression as E};
use lash_kernel_doc::{self as k, Expr};
use lash_kernel_value::{FunctionId, Name};

pub(crate) const COMPUTED: Computed = Computed {
    fallback: "ts.get_computed",
};
pub(crate) struct Computed {
    pub fallback: &'static str,
}
pub(crate) struct ComputedAccess<'a> {
    pub target: &'a Expr,
    pub index: &'a Expr,
    pub tests_record: bool,
}
impl Computed {
    /// Recover the direct record read and its optional, ordered guards.
    pub fn recognize<'a>(
        &self,
        name: &Name,
        mut block: &'a [k::Stmt],
        is: &impl Fn(&FunctionId, &str) -> bool,
        fallback: impl Fn(&k::Rhs, &str) -> Option<Vec<Expr>>,
    ) -> Option<ComputedAccess<'a>> {
        let mut guards = Vec::new();
        while let [
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
        ] = block
        {
            guards.push((condition, else_block.as_slice()));
            block = then_block;
        }
        let [
            k::Stmt::Assign {
                place: k::Place::Variable(destination),
                value: k::Rhs::Expr(Expr::Member(member)),
                order: k::AssignOrder::ValueFirst,
            },
        ] = block
        else {
            return None;
        };
        let k::Member::Index { target, index } = &**member else {
            return None;
        };
        if destination != name {
            return None;
        }
        let tests_record = guards
            .first()
            .is_some_and(|(condition, _)| kind(condition, "record", is) == Some(target));
        let mut previous = None;
        for (condition, otherwise) in guards {
            let rank = if kind(condition, "record", is) == Some(target) {
                0
            } else if UNBRANDED
                .recognize(condition, is)
                .is_some_and(|values| values == [target])
            {
                1
            } else if kind(condition, "text", is) == Some(index) {
                2
            } else if OWN
                .recognize(condition, is)
                .is_some_and(|values| values == [target, index])
            {
                3
            } else {
                return None;
            };
            if previous.is_some_and(|previous| previous >= rank) {
                return None;
            }
            previous = Some(rank);
            let [
                k::Stmt::Assign {
                    place: k::Place::Variable(destination),
                    value,
                    order: k::AssignOrder::ValueFirst,
                },
            ] = otherwise
            else {
                return None;
            };
            if destination != name
                || fallback(value, self.fallback)? != [target.clone(), index.clone()]
            {
                return None;
            }
        }
        Some(ComputedAccess {
            target,
            index,
            tests_record,
        })
    }
}

pub(crate) const KIND: E = E::Call {
    function: "same",
    args: &[
        E::Call {
            function: "kind",
            args: &[E::Input(0)],
        },
        E::Input(1),
    ],
};
pub(crate) const UNBRANDED: E = E::Call {
    function: "bool.not",
    args: &[E::Call {
        function: "record.contains",
        args: &[
            E::Input(0),
            E::Atom(Atom::Text(lash_kernel_dialect::BRAND_FIELD)),
        ],
    }],
};
pub(crate) const OWN: E = E::Call {
    function: "record.contains",
    args: &[E::Input(0), E::Input(1)],
};
pub(crate) const POSITION: E = E::Call {
    function: "eq",
    args: &[
        E::Call {
            function: "num.floor",
            args: &[E::Input(0)],
        },
        E::Call {
            function: "num.abs",
            args: &[E::Input(0)],
        },
    ],
};
pub(crate) const BOUNDS: E = E::Call {
    function: "num.lt",
    args: &[
        E::Input(0),
        E::Call {
            function: "list.len",
            args: &[E::Input(1)],
        },
    ],
};
pub(crate) const APPEND_BOUNDS: E = E::Call {
    function: "num.le",
    args: &[
        E::Input(0),
        E::Call {
            function: "list.len",
            args: &[E::Input(1)],
        },
    ],
};
pub(crate) const LENGTH: E = E::Call {
    function: "num.to_float",
    args: &[E::Call {
        function: "list.len",
        args: &[E::Input(0)],
    }],
};
pub(crate) const HOLE: E = E::Call {
    function: "same",
    args: &[E::Input(0), E::Tuple(&[])],
};

pub(crate) fn kind<'a>(
    expression: &'a Expr,
    expected: &str,
    is: &impl Fn(&FunctionId, &str) -> bool,
) -> Option<&'a Expr> {
    let values = KIND.recognize(expression, is)?;
    let [value, Expr::Literal(k::Literal::Text(kind))] = values.as_slice() else {
        return None;
    };
    (kind == expected).then_some(*value)
}
pub(crate) fn position<'a>(
    expression: &'a Expr,
    is: &impl Fn(&FunctionId, &str) -> bool,
) -> Option<&'a Expr> {
    POSITION.recognize(expression, is)?.first().copied()
}
