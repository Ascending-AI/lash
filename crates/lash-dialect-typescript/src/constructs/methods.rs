//! Primitive method recipes, shared by selection, construction and recognition.
use lash_kernel_dialect::construct::{Atom, Expression as E};
use lash_kernel_doc::{self as k, Expr};
use lash_kernel_value::FunctionId;

pub(crate) const SPLIT_LIMIT: i64 = 4_294_967_295;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Slice,
    Trim,
    Search,
    EndsWith,
    Split,
}
#[derive(Clone, Copy)]
pub(crate) struct Method {
    pub name: &'static str,
    pub array: bool,
    pub recipe: Recipe,
}
#[derive(Clone, Copy)]
pub(crate) enum Recipe {
    Slice,
    Trim(E),
    Search(E),
    EndsWith,
    Split(E),
}
impl Method {
    /// The string recipe's result, even when its fallback helper is not reached.
    pub fn returns_text(&self) -> bool {
        matches!(self.recipe, Recipe::Slice | Recipe::Trim(_))
    }

    pub fn kind(&self) -> Kind {
        match self.recipe {
            Recipe::Slice => Kind::Slice,
            Recipe::Trim(_) => Kind::Trim,
            Recipe::Search(_) => Kind::Search,
            Recipe::EndsWith => Kind::EndsWith,
            Recipe::Split(_) => Kind::Split,
        }
    }
    pub fn expression(&self) -> Option<E> {
        match self.recipe {
            Recipe::Trim(pattern) | Recipe::Search(pattern) | Recipe::Split(pattern) => {
                Some(pattern)
            }
            Recipe::Slice | Recipe::EndsWith => None,
        }
    }
}
pub(crate) const SLICE_UNITS: &str = "ts.string.slice_units";
const FIND: E = E::Call {
    function: "text.utf16_find",
    args: &[E::Input(0), E::Input(1), E::Input(2)],
};
pub(crate) const METHODS: &[Method] = &[
    Method {
        name: "slice",
        array: true,
        recipe: Recipe::Slice,
    },
    Method {
        name: "trim",
        array: false,
        recipe: Recipe::Trim(E::Call {
            function: "text.trim_set",
            args: &[
                E::Input(0),
                E::Input(1),
                E::Atom(Atom::Bool(true)),
                E::Atom(Atom::Bool(true)),
            ],
        }),
    },
    Method {
        name: "trimStart",
        array: false,
        recipe: Recipe::Trim(E::Call {
            function: "text.trim_set",
            args: &[
                E::Input(0),
                E::Input(1),
                E::Atom(Atom::Bool(true)),
                E::Atom(Atom::Bool(false)),
            ],
        }),
    },
    Method {
        name: "trimEnd",
        array: false,
        recipe: Recipe::Trim(E::Call {
            function: "text.trim_set",
            args: &[
                E::Input(0),
                E::Input(1),
                E::Atom(Atom::Bool(false)),
                E::Atom(Atom::Bool(true)),
            ],
        }),
    },
    Method {
        name: "split",
        array: false,
        recipe: Recipe::Split(E::Call {
            function: "text.split",
            args: &[E::Input(0), E::Input(1), E::Atom(Atom::Int(SPLIT_LIMIT))],
        }),
    },
    Method {
        name: "indexOf",
        array: false,
        recipe: Recipe::Search(E::Call {
            function: "num.to_float",
            args: &[FIND],
        }),
    },
    Method {
        name: "includes",
        array: false,
        recipe: Recipe::Search(E::Call {
            function: "num.le",
            args: &[E::Atom(Atom::Int(0)), FIND],
        }),
    },
    Method {
        name: "startsWith",
        array: false,
        recipe: Recipe::Search(E::Call {
            function: "eq",
            args: &[FIND, E::Input(2)],
        }),
    },
    Method {
        name: "endsWith",
        array: false,
        recipe: Recipe::EndsWith,
    },
];
pub(crate) fn method(name: &str, text: bool) -> Option<&'static Method> {
    METHODS
        .iter()
        .find(|method| method.name == name && (text || method.array))
}

/// The helper build projects this literal from its kernel body.
pub(crate) fn trim_set() -> &'static str {
    lash_ext_ts_helpers::STRING_TRIM_SET
}

const CLAMP: E = E::Call {
    function: "float.to_int",
    args: &[E::Call {
        function: "num.to_float",
        args: &[E::Call {
            function: "num.min",
            args: &[
                E::Call {
                    function: "num.max",
                    args: &[E::Input(0), E::Atom(Atom::Int(0))],
                },
                E::Input(1),
            ],
        }],
    }],
};
const RELATIVE: E = E::Call {
    function: "num.add",
    args: &[E::Input(0), E::Input(1)],
};

pub(crate) fn position<E2>(
    receiver: Expr,
    value: f64,
    relative: bool,
    text: bool,
    call: &mut impl FnMut(&str, Vec<Expr>) -> Result<Expr, E2>,
) -> Result<Expr, E2> {
    let value = if value.is_nan() { 0.0 } else { value.trunc() };
    if value == 0.0 || (!relative && value < 0.0) {
        return Ok(Expr::Literal(k::Literal::Int(0_i64.into())));
    }
    let length = if text { TEXT_LENGTH } else { LIST_LENGTH }.emit(&[receiver], call)?;
    if value == f64::INFINITY {
        return Ok(length);
    }
    if value == f64::NEG_INFINITY {
        return Ok(Expr::Literal(k::Literal::Int(0_i64.into())));
    }
    let bound = crate::lower::Operand::number(value).expr();
    let bound = if relative && value < 0.0 {
        RELATIVE.emit(&[length.clone(), bound], call)?
    } else {
        bound
    };
    CLAMP.emit(&[bound, length], call)
}

pub(crate) fn bound(
    value: &Expr,
    receiver: &Expr,
    relative: bool,
    is: &impl Fn(&FunctionId, &str) -> bool,
) -> Option<Option<f64>> {
    let length = |value: &Expr| {
        TEXT_LENGTH
            .recognize(value, is)
            .is_some_and(|args| args.as_slice() == [receiver])
    };
    if *value == Expr::Literal(k::Literal::Int(0_i64.into())) {
        return Some(Some(0.0));
    }
    if length(value) {
        return Some(None);
    }
    let args = CLAMP.recognize(value, is)?;
    let [value, len] = args.as_slice() else {
        return None;
    };
    if !length(len) {
        return None;
    }
    match value {
        Expr::Literal(k::Literal::Float(value)) if value.get() > 0.0 => Some(Some(value.get())),
        value if relative => {
            let args = RELATIVE.recognize(value, is)?;
            let [len, Expr::Literal(k::Literal::Float(value))] = args.as_slice() else {
                return None;
            };
            (length(len) && value.get() < 0.0).then_some(Some(value.get()))
        }
        _ => None,
    }
}

pub(crate) const TEXT_LENGTH: E = E::Call {
    function: "text.utf16_len",
    args: &[E::Input(0)],
};
pub(crate) const LIST_LENGTH: E = E::Call {
    function: "list.len",
    args: &[E::Input(0)],
};
pub(crate) const LIST_SLICE: E = E::Call {
    function: "list.slice",
    args: &[E::Input(0), E::Input(1), E::Input(2)],
};
pub(crate) const SLICE_TOO_LARGE: E = E::Call {
    function: "num.lt",
    args: &[
        E::Atom(Atom::Int(SPLIT_LIMIT)),
        E::Call {
            function: "num.sub",
            args: &[E::Input(1), E::Input(0)],
        },
    ],
};
pub(crate) const SLICE_ERROR: E = E::Call {
    function: "error.new",
    args: &[
        E::Atom(Atom::Text("RangeError")),
        E::Atom(Atom::Text("Invalid array length")),
        E::Atom(Atom::Null),
    ],
};

pub(crate) struct EndsWithRecipe {
    pub start: E,
    pub negative: E,
    pub matched: E,
}
pub(crate) const ENDS_WITH: EndsWithRecipe = EndsWithRecipe {
    start: E::Call {
        function: "num.sub",
        args: &[
            E::Input(0),
            E::Call {
                function: "text.utf16_len",
                args: &[E::Input(1)],
            },
        ],
    },
    negative: E::Call {
        function: "num.lt",
        args: &[E::Input(0), E::Atom(Atom::Int(0))],
    },
    matched: E::Call {
        function: "eq",
        args: &[FIND, E::Input(2)],
    },
};
pub(crate) struct EndsWith<'a> {
    pub start: &'a lash_kernel_value::Name,
    pub result: &'a lash_kernel_value::Name,
    pub receiver: &'a Expr,
    pub needle: &'a Expr,
    pub position: &'a Expr,
}
impl EndsWithRecipe {
    pub fn recognize<'a>(
        &self,
        block: &'a [k::Stmt],
        is: &impl Fn(&FunctionId, &str) -> bool,
    ) -> Option<EndsWith<'a>> {
        let [
            k::Stmt::Let {
                name: start,
                value: k::Rhs::Expr(first),
            },
            k::Stmt::Let {
                name: result,
                value: k::Rhs::Expr(Expr::Literal(k::Literal::Bool(false))),
            },
            k::Stmt::If {
                condition,
                then_block,
                else_block,
            },
            ..,
        ] = block
        else {
            return None;
        };
        let [
            k::Stmt::Assign {
                place: k::Place::Variable(set),
                value: k::Rhs::Expr(matched),
                ..
            },
        ] = else_block.as_slice()
        else {
            return None;
        };
        let values = self.start.recognize(first, is)?;
        let [position, needle @ Expr::Literal(k::Literal::Text(_))] = values.as_slice() else {
            return None;
        };
        let inputs = self.matched.recognize(matched, is)?;
        let [receiver, searched, from] = inputs.as_slice() else {
            return None;
        };
        let starts_here = Expr::Variable(start.clone());
        if set != result
            || !then_block.is_empty()
            || searched != needle
            || **from != starts_here
            || self.negative.recognize(condition, is)?.as_slice() != [&starts_here]
        {
            return None;
        }
        Some(EndsWith {
            start,
            result,
            receiver,
            needle,
            position,
        })
    }
}

pub(crate) const TEXT_RECEIVER_CHECK: E = E::Call {
    function: "text.len",
    args: &[E::Input(0)],
};
pub(crate) fn source_method(kind: Kind) -> &'static str {
    METHODS
        .iter()
        .find(|method| method.kind() == kind)
        .unwrap_or_else(|| unreachable!("a declared method recipe"))
        .name
}
