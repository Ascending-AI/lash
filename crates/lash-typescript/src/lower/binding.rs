//! The scope stack's data: what a binding is, and what the lowerer knows
//! about it.
//!
//! Every per-binding fact belongs here rather than in a side table on the
//! lowerer. A table keyed by name outlives the scope that declared the
//! binding, and internal names are unique only where a binding is visibly
//! shadowed — so sibling scopes shared keys and a dead binding's facts
//! changed how a live one lowered.

use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BindingKind {
    Const,
    Let,
    Var,
    Function,
    Parameter,
    Catch,
}

/// The exotic iterables whose `forEach` and `entries`/`keys`/`values` are the
/// collection's own, not the array methods of the same name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum IterableKind {
    Map,
    Set,
    UrlSearchParams,
}

impl IterableKind {
    pub(super) fn from_constructor(constructor: &str) -> Option<Self> {
        match constructor {
            "Map" => Some(Self::Map),
            "Set" => Some(Self::Set),
            "URLSearchParams" => Some(Self::UrlSearchParams),
            _ => None,
        }
    }
}

/// What a binding *is*, beyond the name it holds.
///
/// Every one of these facts is per-binding, so it lives on the binding and
/// dies with the scope that declared it. Held in a side table keyed by name
/// they outlived their binding and leaked across sibling scopes, which is
/// exactly the shadowing case the internal names are only unique under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum BindingRole {
    /// An ordinary value. Everything the lowerer has no extra fact about.
    Plain,
    /// An async function bound to a name, which must be awaited at its call.
    AsyncHelper,
    /// A handle returned by `start`, which `await` resolves to the process
    /// result.
    ProcessHandle,
    /// A `defineProcess` binding, carrying the declared process name that
    /// `start` targets.
    ProcessDefinition(String),
    /// A collection whose iteration protocol is its own, not an array's.
    ExoticIterable(IterableKind),
}

#[derive(Clone, Debug)]
pub(super) struct Binding {
    pub(super) internal: String,
    pub(super) kind: BindingKind,
    pub(super) initialized: bool,
    pub(super) owner_function: usize,
    pub(super) role: BindingRole,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Scope {
    pub(super) bindings: BTreeMap<String, Binding>,
}
