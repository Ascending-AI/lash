//! Generated immediate helper paths, resolved once beside native entries.

use std::sync::OnceLock;

use lash_kernel_doc::plan::{HeapTemplate, HelperTemplate};
use lash_kernel_doc::{FunctionId, FunctionRegistry, PreparedNative, Value};

impl super::NestedKind {
    /// A statically named library call, whether written as a native
    /// expression or an invoke. Its kind still owns its action semantics.
    pub(crate) fn operator_lib(&self) -> Option<super::LibId> {
        match self {
            Self::Body(lib) | Self::Call(super::NestedCallee::Library(lib)) => Some(*lib),
            _ => None,
        }
    }
}

/// A native entry, or a body-generated entry over resolved native callees.
/// Native entries keep their direct function-pointer ABI.
#[derive(Clone)]
pub(crate) enum Entry {
    Native(PreparedNative),
    Template {
        template: &'static HelperTemplate,
        natives: Box<[PreparedNative]>,
        /// The heap paths' general callees, resolved where a region first
        /// takes them; `None` when the template has no heap paths or a
        /// callee is missing.
        heap: OnceLock<Option<Box<[super::LibId]>>>,
    },
}

impl Entry {
    pub(super) fn template(
        registry: &FunctionRegistry,
        template: &'static HelperTemplate,
    ) -> Option<Self> {
        let natives = template
            .callees
            .iter()
            .map(|function| {
                let registered = registry.get(function)?;
                if registered.definition().guard.is_some() {
                    return None;
                }
                registered.native()?.prepare()
            })
            .collect::<Option<Box<[_]>>>()?;
        if template.entry.is_none() && template.heap.is_none() {
            return None;
        }
        Some(Self::Template {
            template,
            natives,
            heap: OnceLock::new(),
        })
    }

    #[inline]
    pub(crate) fn call(&self, args: &[Value]) -> Option<Value> {
        match self {
            Self::Native(entry) => (entry.entry)(args),
            Self::Template {
                template, natives, ..
            } => (template.entry?)(args, natives),
        }
    }

    #[inline]
    pub(crate) fn answer(
        &self,
        args: &[Value],
        charging: bool,
    ) -> Option<lash_kernel_doc::PreparedAnswer> {
        match self {
            Self::Native(entry) => entry.answer(args, charging),
            Self::Template { .. } => {
                let value = self.call(args)?;
                Some(lash_kernel_doc::PreparedAnswer {
                    value,
                    scalar_sizes: if charging {
                        scalar_sizes(args, value)
                    } else {
                        None
                    },
                })
            }
        }
    }

    /// Whether the entry answers immediates: a native's, or a template
    /// with scalar paths.
    #[inline]
    pub(crate) fn immediate(&self) -> bool {
        match self {
            Self::Native(_) => true,
            Self::Template { template, .. } => template.entry.is_some(),
        }
    }

    /// The template's heap paths, their general callees resolved by
    /// `lib_of` and its resolved prepared natives, when it has heap paths
    /// and every callee resolves.
    #[inline]
    pub(crate) fn heap(
        &self,
        lib_of: &dyn Fn(&FunctionId) -> Option<super::LibId>,
    ) -> Option<(&'static HeapTemplate, &[super::LibId], &[PreparedNative])> {
        match self {
            Self::Template {
                template,
                natives,
                heap,
            } => {
                let paths = template.heap.as_ref()?;
                let callees = heap.get_or_init(|| paths.callees.iter().map(lib_of).collect());
                Some((paths, callees.as_deref()?, natives))
            }
            Self::Native(_) => None,
        }
    }

    /// The call depth its immediate paths need beyond the caller's.
    #[inline]
    pub(crate) fn depth(&self) -> u32 {
        match self {
            Self::Native(_) => 0,
            Self::Template { template, .. } => template.depth,
        }
    }

    /// The call depth any of its paths needs beyond the caller's, heap
    /// paths included: a heap path's call that a callee's entry answers
    /// ([`inner`]) needs that entry's depth below the call's.
    pub(crate) fn deepest<'l>(
        &self,
        lib: &dyn Fn(super::LibId) -> &'l super::Lib,
        lib_of: &dyn Fn(&FunctionId) -> Option<super::LibId>,
    ) -> u32 {
        self.deepest_at(lib, lib_of, 0)
    }

    fn deepest_at<'l>(
        &self,
        lib: &dyn Fn(super::LibId) -> &'l super::Lib,
        lib_of: &dyn Fn(&FunctionId) -> Option<super::LibId>,
        level: u32,
    ) -> u32 {
        let heap = self.heap(lib_of).map_or(0, |(heap, callees, _)| {
            let below = callees
                .iter()
                .filter(|_| level < INNER)
                .filter_map(|&callee| inner(lib(callee)))
                .map(|entry| entry.deepest_at(lib, lib_of, level + 1))
                .max();
            heap.depth + below.unwrap_or(0)
        });
        heap.max(self.depth())
    }
}

/// How many helpers below an operator's heap paths a callee's entry
/// answers a call: below that, only natives answer.
pub(crate) const INNER: u32 = 2;

/// The entry that answers a heap path's call of `function` where no native
/// does: a body-run helper's, which agrees with its body, as the body runs
/// uncharged inside the caller's. An operation's admission makes no cell
/// for a shared parameter; its depth unit is the call's.
pub(crate) fn inner(function: &super::Lib) -> Option<&Entry> {
    if function.shares_params || matches!(function.run, super::LibRun::Native { .. }) {
        return None;
    }
    function.prepared.as_ref().map(|(entry, _)| entry)
}

/// Classify only immediate scalars; all other answers keep formula evaluation.
/// The helper's formula, rather than any callee's, indexes this mask.
#[inline]
fn scalar_sizes(args: &[Value], value: Value) -> Option<u8> {
    #[inline]
    fn bit(value: Value) -> Option<u8> {
        match value {
            Value::Int(word) => Some(u8::from(word != 0)),
            Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_) => Some(0),
            _ => None,
        }
    }
    let result = bit(value)? << 3;
    let args = match args {
        [] => 0,
        [a] => bit(*a)?,
        [a, b] => bit(*a)? | (bit(*b)? << 1),
        [a, b, c] => bit(*a)? | (bit(*b)? << 1) | (bit(*c)? << 2),
        _ => return None,
    };
    Some(args | result)
}
