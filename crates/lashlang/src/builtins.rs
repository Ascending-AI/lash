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
//! All three consult the registries here instead of re-spelling the name/arity
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
pub(crate) const SOURCE_BUILTINS: &[Builtin] = &[
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

// Dialect-private intrinsics. They are registered here so the shared linker,
// compiler, runtime arity diagnostics, and profiler agree on the call
// contract; source Lashlang cannot spell or discover the reserved names.
pub(crate) const TYPESCRIPT_BUILTINS: &[Builtin] = &[
    Builtin {
        name: "__typescript_split",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_join",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_stdlib",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "__typescript_heap_new",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "__typescript_heap_instanceof",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_global_delete",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__typescript_global_has",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__typescript_call_dynamic",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_async_map",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_closure",
        arity: Arity::Exact(3),
    },
    Builtin {
        name: "__typescript_global_set",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__typescript_encode_uri_component",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__typescript_decode_uri_component",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__typescript_encode_uri",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__typescript_decode_uri",
        arity: Arity::Exact(1),
    },
];

/// Looks up a builtin by name.
pub(crate) fn lookup(name: &str) -> Option<Builtin> {
    SOURCE_BUILTINS
        .iter()
        .chain(TYPESCRIPT_BUILTINS)
        .copied()
        .find(|builtin| builtin.name == name)
}

/// Whether `name` is a known builtin function.
pub(crate) fn is_builtin(name: &str) -> bool {
    lookup(name).is_some()
}

pub(crate) fn names() -> impl ExactSizeIterator<Item = &'static str> + Clone {
    SOURCE_BUILTINS.iter().map(|builtin| builtin.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_intrinsics_are_registered_but_not_advertised() {
        let advertised = names().collect::<Vec<_>>();
        for (name, arity) in [
            ("__typescript_heap_instanceof", Arity::Exact(2)),
            ("__typescript_global_delete", Arity::Exact(1)),
            ("__typescript_global_has", Arity::Exact(1)),
            ("__typescript_call_dynamic", Arity::Exact(2)),
            ("__typescript_async_map", Arity::Exact(2)),
            ("__typescript_closure", Arity::Exact(3)),
            ("__typescript_global_set", Arity::Exact(2)),
            ("__typescript_encode_uri_component", Arity::Exact(1)),
            ("__typescript_decode_uri_component", Arity::Exact(1)),
            ("__typescript_encode_uri", Arity::Exact(1)),
            ("__typescript_decode_uri", Arity::Exact(1)),
        ] {
            assert_eq!(lookup(name).map(|builtin| builtin.arity), Some(arity));
            assert!(!advertised.contains(&name));
        }
        assert!(lookup("__typescript_btoa").is_none());
        assert!(lookup("__typescript_atob").is_none());
    }
}
