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
    pub(super) fn same(&self, other: &Self) -> bool {
        self.before == other.before
            && self.arguments == other.arguments
            && self.library == other.library
            && self.completion.is_some() == other.completion.is_some()
    }
    pub(crate) fn before_entry(&self) -> u64 {
        self.before
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
        if arguments.len() != self.arguments {
            return Ok(None);
        }
        let Some(plan) = &self.completion else {
            return Ok(Some(0));
        };
        let absent = lease.import(&Value::Absent).map_err(LeaseFault::Heap)?;
        let mut failure = None;
        let units = plan.evaluate(|source, selector| {
            if failure.is_some() {
                return 0;
            }
            let value = match source {
                compile::Source::Arg(index) => arguments.get(index).copied().unwrap_or(absent),
                compile::Source::Result => result,
                compile::Source::Nothing => return 0,
            };
            match lease.measure(value, selector) {
                Ok(amount) => amount,
                Err(error) => {
                    failure = Some(error);
                    0
                }
            }
        });
        match failure {
            Some(error) => Err(error),
            None => Ok(Some(units)),
        }
    }
}
