//! Speculation and publication validate scalars with the same admitted kernels.
use super::*;
use lash_kernel_doc::{self as doc, Value};
use lash_kernel_state::arena::Arena;

impl Lib {
    /// None means no applicable kernel; Some(None) is a kernel refusal.
    pub(crate) fn scalar_kernel(&self, arena: &Arena, args: &[Value]) -> Option<Option<Value>> {
        let (entry, _) = self.specialization.prepared.as_ref()?;
        match args {
            [Value::Int(a), Value::Int(b)] => {
                entry.pair()?.ints.map(|op| doc::scalar::ints(op, *a, *b))
            }
            [Value::Float(a), Value::Float(b)] => entry
                .pair()?
                .floats
                .map(|op| doc::scalar::floats(op, *a, *b)),
            [
                a @ (Value::Text(_) | Value::LongText(_)),
                b @ (Value::Text(_) | Value::LongText(_)),
            ] => {
                let op = self.specialization.native.text_relation?;
                let semantics = arena.semantics();
                Some((|| {
                    doc::scalar::texts(op, semantics.text(a).ok()?, semantics.text(b).ok()?)
                })())
            }
            _ => None,
        }
    }

    /// An unavailable arena-dependent entry may use an explicit native contract.
    /// A kernel refusal or an actual disagreement must never do so.
    pub(crate) fn check_known_scalar(
        &self,
        arena: &Arena,
        args: &[Value],
        expected: Value,
    ) -> Result<Option<Value>, ()> {
        let actual = match self.scalar_kernel(arena, args) {
            Some(Some(value)) => value,
            Some(None) => return Err(()),
            None => {
                let Some((entry, _)) = self.specialization.prepared.as_ref() else {
                    return Ok(None);
                };
                let Some(value) = entry.call(args) else {
                    return Ok(None);
                };
                value
            }
        };
        if !actual.same(&expected) {
            return Err(());
        }
        Ok(Some(expected))
    }
}
