//! Which bindings a value was certainly given to where the walk stands
//! (see the module): a `var`, or a `let` without an initialiser, read
//! anywhere else is `undefined` first.

use super::Collector;

impl Collector<'_> {
    /// A read of `binding` where the walk stands: early unless its own
    /// function certainly gave it a value first.
    pub(super) fn use_of(&mut self, binding: usize) {
        if self.function_depth > self.bindings[binding].depth || !self.defined.contains(&binding) {
            self.bindings[binding].early = true;
        }
    }

    /// Gives the bindings pending since `mark` their values, when the walk
    /// is not in a conditional part of an expression.
    pub(super) fn settle(&mut self, mark: usize) {
        let given = self.pending.split_off(mark.min(self.pending.len()));
        if self.conditional == 0 {
            self.defined.extend(given);
        }
    }

    /// Runs `walk` as a part of an expression that may not run.
    pub(super) fn conditionally(&mut self, walk: impl FnOnce(&mut Self)) {
        self.conditional += 1;
        walk(self);
        self.conditional -= 1;
    }

    /// Runs `walk` as a statement that gives nothing certain to what
    /// follows it.
    pub(super) fn branch(&mut self, walk: impl FnOnce(&mut Self)) {
        let before = self.defined.clone();
        walk(self);
        self.defined = before;
    }
}
