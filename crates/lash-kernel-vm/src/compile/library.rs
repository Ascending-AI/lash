//! One admitted library function and the implementation that runs it.
use super::*;
mod scalar;
use lash_kernel_doc::NativeFunction;

pub(crate) struct Lib {
    pub(crate) shares_params: bool,
    pub(crate) operation: bool,
    /// Its definition's header: what a call reads of it without the body.
    pub(crate) header: Arc<lash_kernel_doc::FunctionHeader>,
    pub(crate) run: LibRun,
    /// How many parameters the definition has.
    pub(crate) arity: usize,
    /// The definition's charge formula, and its guard's limit.
    pub(crate) charge: Plan,
    pub(crate) limit: Option<Plan>,
    /// An unguarded native's prepared entry and its formula-derived charge:
    /// the inline fast path of the operator op that calls it
    /// (`Op::Operator`, or a nested call its entry marks). Odd layouts and
    /// arena-dependent natives use the general call.
    pub(crate) specialization: Specialization,
}

/// The exact admitted function's resolved specialization facts. Register sites
/// project these facts; they never select a native implementation by name.
pub(crate) struct Specialization {
    pub(crate) resource: super::room::CallSource,
    pub(crate) function: FunctionId,
    pub(crate) prepared: Option<(Entry, Fast)>,
    pub(crate) native: lash_kernel_doc::NativeSpecialization,
    pub(crate) deepest: std::sync::OnceLock<u32>,
}

impl Lib {
    pub(crate) fn prepared_depth(&self, exe: &Executable) -> u32 {
        *self.specialization.deepest.get_or_init(|| {
            self.specialization
                .prepared
                .as_ref()
                .map_or(0, |(entry, _)| {
                    entry.deepest(&|id| exe.lib(id), &|id| exe.library_lib(id))
                })
        })
    }

    /// Every prepared, metered, typed, scalar, heap and joined-text entry uses
    /// this exact declaration. Inspecting its source is cold and preserves
    /// ordinary preparation's body laziness.
    #[cfg(test)]
    pub(crate) fn source_room<'e>(&self, exe: &'e Executable) -> Option<&'e super::room::Source> {
        let code = match self.run {
            LibRun::Body(code)
            | LibRun::Native {
                body: Some(code), ..
            } => code,
            _ => return None,
        };
        let source = &exe.code(code).source_room;
        (self.specialization.resource
            == super::room::CallSource::Body(self.specialization.function)
            && source.owner
                == doc::constant_pool::PoolOwner::Function(self.specialization.function))
        .then_some(source.as_ref())
    }

    pub(crate) fn body_arity(&self, given: usize) -> bool {
        given <= self.arity
            && self.header.signature.params[given..]
                .iter()
                .all(|p| p.optional)
    }

    /// A function that may answer without its body reserves that body's
    /// depth unit before offering its native the call (K-BND-002).
    /// Body-only calls reserve their ordinary frame at entry.
    pub(crate) fn operation(&self) -> bool {
        self.operation
    }
}

pub(crate) enum LibRun {
    /// A native-only declaration admitted for foreign-image validation.
    /// Its signature participates in compilation; it has no executable entry.
    Declaration,
    /// The native implementation, and the kernel body that runs a call it
    /// declines (`NativeError::Decline`).
    Native {
        native: Arc<dyn NativeFunction>,
        body: Option<CodeId>,
    },
    Body(CodeId),
    Machine(MachineFunction),
}
