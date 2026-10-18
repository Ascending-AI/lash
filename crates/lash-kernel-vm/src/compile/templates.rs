//! Generated immediate helper paths, resolved once beside native entries.

use lash_kernel_doc::{FunctionRegistry, PreparedNative, Value, plan::HelperTemplate};

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
        Some(Self::Template { template, natives })
    }

    #[inline]
    pub(crate) fn call(&self, args: &[Value]) -> Option<Value> {
        match self {
            Self::Native(entry) => (entry.entry)(args),
            Self::Template { template, natives } => (template.entry)(args, natives),
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

    #[inline]
    pub(crate) fn depth(&self) -> u32 {
        match self {
            Self::Native(_) => 0,
            Self::Template { template, .. } => template.depth,
        }
    }
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
