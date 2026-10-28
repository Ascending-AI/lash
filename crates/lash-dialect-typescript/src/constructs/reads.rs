//! Guard forms used by authored member reads and their structural inverse.
use lash_kernel_dialect::construct::{Atom, Expression as E};
use lash_kernel_doc::{self as k, Expr};
use lash_kernel_value::FunctionId;

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
