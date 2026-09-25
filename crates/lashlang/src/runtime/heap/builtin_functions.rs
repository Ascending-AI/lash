//! The heap's one object per built-in function value (FIG-3701).
//!
//! `'a'.includes === 'b'.includes` holds because both reads answer the same
//! reference: the heap allocates a built-in's object on its first read and
//! indexes it, and a wire restores the index from the objects it carries
//! rather than storing it.

use super::*;

impl Heap {
    /// The value of built-in `function`: the heap's one object for it,
    /// allocated on the first read. Every later read, from whichever receiver,
    /// answers the same reference, which is the identity ECMA gives the one
    /// function object on a prototype.
    pub(crate) fn builtin_function(
        &mut self,
        function: BuiltinFunction,
    ) -> Result<Value, RuntimeError> {
        if let Some(id) = self.builtin_functions.get(&function) {
            return Ok(Value::Ref(*id));
        }
        let value = self.allocate_object(HeapObject::BuiltinFunction(function))?;
        if let Value::Ref(id) = value {
            self.builtin_functions.insert(function, id);
        }
        Ok(value)
    }

    /// Rebuilds the built-in index from restored objects, refusing a wire
    /// that holds two objects for one built-in: `===` would then answer false
    /// for one function.
    pub(super) fn index_builtin_functions(&mut self) -> Result<(), String> {
        for (id, entry) in &self.entries {
            if let HeapObject::BuiltinFunction(function) = entry.object
                && self.builtin_functions.insert(function, *id).is_some()
            {
                return Err(format!(
                    "built-in function {}.prototype.{} is held by two heap objects",
                    function.prototype().name(),
                    function.name()
                ));
            }
        }
        Ok(())
    }
}
