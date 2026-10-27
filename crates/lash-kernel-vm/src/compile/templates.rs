//! Generated immediate helper paths, resolved once beside native entries.

use std::sync::OnceLock;

use lash_kernel_doc::plan::{HeapTemplate, HelperTemplate};
use lash_kernel_doc::{FunctionRegistry, PreparedNative, Value};

use lash_kernel_value::names::FunctionId;

impl super::NestedKind {
    /// A statically named library call, whether written as a native
    /// expression or an invoke. Its kind still owns its action semantics.
    pub(crate) fn operator_lib(&self) -> Option<super::LibId> {
        match self {
            Self::Body(lib)
            | Self::Call(
                super::NestedCallee::Library(lib)
                | super::NestedCallee::CheckedHelper(lib)
                | super::NestedCallee::Import(_, Some(lib)),
            ) => Some(*lib),
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
        element: Option<&'static lash_kernel_doc::plan::ElementPath>,
    },
}

impl Entry {
    pub(crate) fn element(&self) -> Option<&'static lash_kernel_doc::plan::ElementPath> {
        match self {
            Self::Template { element, .. } => *element,
            Self::Native(_) => None,
        }
    }

    /// Resolve the certificate's predicate against the actual registered native.
    pub(crate) fn record_write(&self) -> Option<(&'static str, [bool; 2], u32)> {
        let Self::Template {
            template, natives, ..
        } = self
        else {
            return None;
        };
        let path = template.heap.as_ref()?.record_write.as_ref()?;
        let entry = natives.get(usize::from(path.test))?;
        let accepted = |present| match (entry.entry)(&[Value::Bool(present)])? {
            Value::Bool(answer) => Some(answer == path.holds),
            _ => None,
        };
        Some((path.key, [accepted(false)?, accepted(true)?], path.values))
    }

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
                registered.native()?.specialization().immediate
            })
            .collect::<Option<Box<[_]>>>()?;
        if template.entry.is_none() && template.heap.is_none() {
            return None;
        }
        Some(Self::Template {
            template,
            natives,
            heap: OnceLock::new(),
            element: template
                .heap
                .as_ref()
                .and_then(|heap| heap.element.as_ref())
                .and_then(|path| {
                    path.requires
                        .iter()
                        .all(|requirement| {
                            registry
                                .get(&requirement.function)
                                .is_some_and(|registered| {
                                    registered.definition().guard.is_none()
                                        && registered.native().is_some_and(|native| {
                                            requirement.fact.accepts(&native.specialization())
                                        })
                                })
                        })
                        .then_some(path)
                }),
        })
    }

    #[inline]
    pub(crate) fn call(&self, args: &[Value]) -> Option<Value> {
        match self {
            Self::Native(entry) => (entry.entry)(args),
            Self::Template {
                template, natives, ..
            } => {
                if template.metered.is_some() {
                    return None;
                }
                (template.entry?)(args, natives)
            }
        }
    }

    #[inline(always)]
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

    /// The entry's typed paths over two floats and over two word
    /// integers: a native's own, or those of the native a template's path
    /// forwards both arguments to unchanged.
    #[inline]
    pub(crate) fn pair(&self) -> Option<lash_kernel_doc::scalar::Pair> {
        match self {
            Self::Native(entry) => entry.pair,
            Self::Template {
                template, natives, ..
            } => {
                if template.metered.is_some() {
                    return None;
                }
                let of = |native: Option<u8>| natives.get(usize::from(native?))?.pair;
                let floats = of(template.forwards.floats).and_then(|pair| pair.floats);
                let ints = of(template.forwards.ints).and_then(|pair| pair.ints);
                (floats.is_some() || ints.is_some())
                    .then_some(lash_kernel_doc::scalar::Pair { floats, ints })
            }
        }
    }

    /// Whether the entry answers immediates: a native's, or a template
    /// with scalar paths.
    #[inline]
    pub(crate) fn immediate(&self) -> bool {
        match self {
            Self::Native(_) => true,
            Self::Template { template, .. } => {
                template.entry.is_some() && template.metered.is_none()
            }
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
                ..
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
            .max(self.element().map_or(0, |path| path.depth))
    }

    /// Whether a heap path may write a guest object: its own, or one of a
    /// callee's entry below it ([`inner`]).
    pub(crate) fn writes<'l>(
        &self,
        lib: &dyn Fn(super::LibId) -> &'l super::Lib,
        lib_of: &dyn Fn(&FunctionId) -> Option<super::LibId>,
    ) -> bool {
        self.writes_at(lib, lib_of, 0)
    }

    fn writes_at<'l>(
        &self,
        lib: &dyn Fn(super::LibId) -> &'l super::Lib,
        lib_of: &dyn Fn(&FunctionId) -> Option<super::LibId>,
        level: u32,
    ) -> bool {
        self.heap(lib_of).is_some_and(|(heap, callees, _)| {
            heap.writes
                || level < INNER
                    && callees
                        .iter()
                        .filter_map(|&callee| inner(lib(callee)))
                        .any(|entry| entry.writes_at(lib, lib_of, level + 1))
        })
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
    function
        .specialization
        .prepared
        .as_ref()
        .map(|(entry, _)| entry)
}

/// Classify only immediate scalars; all other answers keep formula evaluation.
/// The helper's formula, rather than any callee's, indexes this mask.
#[inline]
pub(super) fn scalar_sizes(args: &[Value], value: Value) -> Option<u8> {
    use lash_kernel_doc::scalar::size_bit as bit;
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
