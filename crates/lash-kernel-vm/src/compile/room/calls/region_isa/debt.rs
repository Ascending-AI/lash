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
    /// Only the actual already evaluated argument window and returned value
    /// feed the canonical formula. No amount supplied by a caller is trusted.
    pub(crate) fn completion(&self, arguments: &[Value], result: Value) -> Option<u64> {
        if arguments.len() != self.arguments {
            return None;
        }
        let Some(plan) = &self.completion else {
            return Some(0);
        };
        let mut complete = true;
        let units = plan.evaluate(|source, measure| {
            let value = match source {
                compile::Source::Arg(index) => {
                    arguments.get(index).copied().unwrap_or(Value::Absent)
                }
                compile::Source::Result => result,
                compile::Source::Nothing => return 0,
            };
            match crate::data::immediate_measure(&value, measure) {
                Some(amount) => amount,
                None => {
                    complete = false;
                    0
                }
            }
        });
        complete.then_some(units)
    }
}
