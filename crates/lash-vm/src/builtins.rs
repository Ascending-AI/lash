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

// Reserved IR intrinsics. They are registered here so the shared linker,
// compiler, runtime arity diagnostics, and profiler agree on the call
// contract; source Lash VM cannot spell or discover the reserved names.
pub(crate) const IR_INTRINSICS: &[Builtin] = &[
    Builtin {
        name: "__lash_vm_split",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_join",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_stdlib",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "__lash_vm_heap_new",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "__lash_vm_heap_instanceof",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_heap_delete_member",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_regexp",
        arity: Arity::AtLeast(1),
    },
    Builtin {
        name: "__lash_vm_global_delete",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_global_get",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_global_has",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_call_dynamic",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_call_method_dynamic",
        arity: Arity::Exact(3),
    },
    Builtin {
        name: "__lash_vm_pending_tool",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_pending_timer",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_await_array",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_await_pending",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_async_map",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_closure",
        arity: Arity::Exact(3),
    },
    Builtin {
        name: "__lash_vm_global_set",
        arity: Arity::Exact(2),
    },
    // Binding cells (FIG-3707): a captured binding that something assigns
    // lives in one cell the owning frame and every closure over it share.
    Builtin {
        name: "__lash_vm_cell_new",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_cell_get",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_cell_set",
        arity: Arity::Exact(2),
    },
    Builtin {
        name: "__lash_vm_encode_uri_component",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_decode_uri_component",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_encode_uri",
        arity: Arity::Exact(1),
    },
    Builtin {
        name: "__lash_vm_decode_uri",
        arity: Arity::Exact(1),
    },
];

/// Looks up a builtin by name.
pub(crate) fn lookup(name: &str) -> Option<Builtin> {
    SOURCE_BUILTINS
        .iter()
        .chain(IR_INTRINSICS)
        .copied()
        .find(|builtin| builtin.name == name)
}

pub(crate) fn is_builtin(name: &str) -> bool {
    lookup(name).is_some()
}

pub(crate) fn names() -> impl ExactSizeIterator<Item = &'static str> + Clone {
    SOURCE_BUILTINS.iter().map(|builtin| builtin.name)
}
