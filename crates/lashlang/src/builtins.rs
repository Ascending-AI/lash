//! Single source of truth for the language's builtin functions.
//!
//! Three pipeline stages need to agree on the set of builtins and their arity:
//!
//! * the linker rejects calls to unknown builtins (`is_builtin`),
//! * the compiler validates arity before emitting an [`IntrinsicOp`]
//!   (`resolve_intrinsic`), and
//! * the runtime renders arity-mismatch diagnostics
//!   (`invalid_arity_message`).
//!
//! All three consult [`BUILTINS`] here instead of re-spelling the name/arity
//! table, so adding or changing a builtin happens in exactly one place.

/// Accepted argument count(s) for a builtin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arity {
    /// Exactly `n` arguments.
    Exact(usize),
    /// Any count in `min..=max` (inclusive).
    Range(usize, usize),
    /// At least `min` arguments.
    AtLeast(usize),
}

impl Arity {
    pub(crate) fn accepts(self, argc: usize) -> bool {
        match self {
            Arity::Exact(n) => argc == n,
            Arity::Range(min, max) => (min..=max).contains(&argc),
            Arity::AtLeast(min) => argc >= min,
        }
    }
}

/// One builtin's name and accepted arity.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Builtin {
    pub(crate) name: &'static str,
    pub(crate) arity: Arity,
}

/// The canonical builtin registry, ordered for readability only.
pub(crate) const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "len",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "empty",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "keys",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "values",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "trim",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "to_string",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "to_int",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "to_float",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "json_parse",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "contains",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "grep_text",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "starts_with",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "ends_with",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "split",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "join",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "validate",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "ceil_div",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "floor_div",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "push",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "slice",
        arity: Arity::Exact(3),
    },
    Builtin {
        name: "find",
        arity: Arity::Range(2, 3),
    },
    Builtin {
        name: "format",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "range",
        arity: Arity::Range(1, 3),
    },
    Builtin {
        name: "sort",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "sort_by",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "sum",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "min",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "max",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "replace",
        arity: Arity::Exact(3),
    },
    Builtin {
        name: "lower",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "upper",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "unique",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "reverse",
        arity: Arity::Exact(1),
    },
];

/// Looks up a builtin by name.
pub(crate) fn lookup(name: &str) -> Option<Builtin> {
    BUILTINS
        .iter()
        .copied()
        .find(|builtin| builtin.name == name)
}

/// Whether `name` is a known builtin function.
pub(crate) fn is_builtin(name: &str) -> bool {
    lookup(name).is_some()
}
