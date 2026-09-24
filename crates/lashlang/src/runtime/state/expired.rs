//! The functions a cell boundary dropped (FIG-3608).
//!
//! Closures do not cross a program boundary: a function's index is
//! program-scoped, and the next cell compiles its own program, so a rooted
//! closure would survive collection only to fail that program's closure
//! validation (ADR 0076). The boundary drops every root that reaches one. The
//! state keeps the dropped names, and persists them, so a later cell's
//! reference is refused by name, live and after a reload alike, instead of
//! degrading to a name nothing ever bound.

use super::*;

impl State {
    /// The globals a cell boundary dropped for holding a function, which no
    /// later cell or host write has bound again.
    pub fn expired_functions(&self) -> &BTreeSet<String> {
        &self.expired_functions
    }

    /// Drops every root of a finished run whose value reaches a function and
    /// remembers its name; a name the run left bound is live again, whatever
    /// it held before. `host_view` omits these globals on the same rule, so
    /// the owner and its projection agree, and each closure is left as
    /// garbage the next collection reclaims.
    pub(super) fn expire_function_roots(&mut self, runtime_globals: &mut Record, heap: &Heap) {
        let closure_reach = heap.closure_reach();
        let mut closure_rooted = Vec::new();
        for entry in runtime_globals.entries.iter() {
            if closure_reach.covers(&entry.value) {
                closure_rooted.push((entry.symbol, entry.name.to_string()));
            }
        }
        for name in runtime_globals.keys() {
            self.expired_functions.remove(name);
        }
        for (symbol, name) in closure_rooted {
            runtime_globals.remove_symbol(symbol);
            self.expired_functions.insert(name);
        }
    }
}

impl Snapshot {
    /// The decoded snapshot with the dropped names its wire carried, refused
    /// when one of them is also a live binding.
    pub(super) fn with_expired_functions(
        self,
        expired_functions: BTreeSet<String>,
    ) -> Result<Self, SnapshotDecodeError> {
        let bound = match &self.mode {
            StateMode::Plain(globals) => globals.as_ref(),
            StateMode::HeapBacked(backed) => &backed.runtime_globals,
        };
        if let Some(name) = expired_functions
            .iter()
            .find(|name| bound.get(name.as_str()).is_some())
        {
            return Err(SnapshotDecodeError::InvalidEncoding(format!(
                "`{name}` is both a live binding and a function the cell boundary dropped"
            )));
        }
        Ok(Self {
            expired_functions,
            ..self
        })
    }
}

/// The wire's dropped-function names: strictly sorted and unique, as the
/// writer emits them.
pub(super) fn expired_functions_from_wire(
    names: Vec<String>,
) -> Result<BTreeSet<String>, SnapshotDecodeError> {
    if names.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(SnapshotDecodeError::NonCanonicalEncoding {
            location: "expired_functions".to_string(),
            reason: "names must be strictly sorted and unique".to_string(),
        });
    }
    Ok(names.into_iter().collect())
}
