//! A call's own ordered debt excludes already executed argument expressions.
//! Caller closes precede entry; the body executes its reached closes; the
//! completion formula follows Leave and precedes caller Restore/Commit.
use super::*;

#[derive(Clone)]
pub(crate) struct CallDebt {
    before: u64,
    arguments: usize,
    library: Option<LibId>,
    completion: Option<Arc<compile::Plan>>,
}
impl CallDebt {
    pub(super) fn compile(
        exe: &Executable,
        caller: CodeId,
        target: &Resolved,
    ) -> Result<Self, Refused> {
        let code = exe.code(caller);
        let before = if target.rooted && code.charged {
            u64::try_from(target.arguments)
                .ok()
                .and_then(|count| {
                    count.checked_mul(doc::plan::FormCharge::Argument.measurement().0)
                })
                .ok_or(Refused::Facts)?
        } else {
            0
        };
        let completion = target.library.and_then(|lib| {
            code.charging
                .then(|| exe.lib(lib).header.metering.formula())
                .flatten()
                .map(|formula| {
                    Arc::new(compile::Plan::new(
                        formula,
                        &exe.lib(lib).header.signature.params,
                    ))
                })
        });
        Ok(Self {
            before,
            arguments: target.arguments,
            library: target.library,
            completion,
        })
    }
    /// The whole completion program must equal the one the canonical
    /// declaration compiles to: a substituted formula would bill differently.
    pub(super) fn same(&self, other: &Self) -> bool {
        self.before == other.before
            && self.arguments == other.arguments
            && self.library == other.library
            && self.completion.as_deref() == other.completion.as_deref()
    }
    #[cfg(test)]
    pub(super) fn corrupt_completion_for_law(
        &mut self,
        params: &[lash_kernel_value::types::Param],
    ) {
        assert!(self.completion.is_some(), "law needs a charged completion");
        self.completion = Some(Arc::new(compile::Plan::new(
            &doc::Formula::Constant(0),
            params,
        )));
    }
    pub(crate) fn before_entry(&self) -> u64 {
        self.before
    }
    pub(crate) fn needs_arguments(&self) -> bool {
        self.completion.is_some()
    }
    /// Quote from current actual lease values while the source operands are
    /// live. C keeps the scalar debt through Leave and bills it at the canonical
    /// completion phase; quoting adds no source holding or charge.
    pub(crate) fn completion(
        &self,
        lease: &mut StateLease<'_>,
        arguments: &[LeaseValue],
        result: LeaseValue,
    ) -> Result<Option<u64>, LeaseFault> {
        let Some(plan) = &self.completion else {
            return Ok(Some(0));
        };
        if arguments.len() != self.arguments {
            return Ok(None);
        }
        plan.lease(lease, arguments, result).map(Some)
    }
}
