//! One admitted library function and the implementation that runs it.
use super::*;
use lash_kernel_doc::NativeFunction;

pub(crate) struct Lib {
    pub(crate) index: LibId,
    pub(crate) shares_params: bool,
    pub(crate) operation: bool,
    pub(crate) id: FunctionId,
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
    pub(crate) prepared: Option<(Entry, Fast)>,
}

impl Lib {
    /// A function that may answer without its body reserves that body's
    /// depth unit before offering its native the call (K-BND-002).
    /// Body-only calls reserve their ordinary frame at entry.
    pub(crate) fn operation(&self) -> bool {
        self.operation
    }

    /// Whether it is the kernel's `same`, whose test of a value against the
    /// empty tuple (`same(x, ())`, TypeScript's record guard and hole
    /// test) is whether the value is a tuple of no members: a plan region
    /// and an island answer that test without building the tuple.
    pub(crate) fn is_same(&self) -> bool {
        self.header.name.as_str() == "same"
            && matches!(self.run, LibRun::Native { .. })
            && self.arity == 2
    }

    /// Whether it is `kind`, whose answer is a function of its operand's
    /// value kind alone.
    pub(crate) fn is_kind(&self) -> bool {
        self.header.name.as_str() == "kind"
            && matches!(self.run, LibRun::Native { .. })
            && self.arity == 1
    }

    /// What a call of `list.len` charges, whose answer on a list is that
    /// list's length: a region reads it from the arena where the list is
    /// resident, as the native's own call reads it, and charges the units
    /// of its formula, which measures nothing. `None` for every other
    /// function.
    pub(crate) fn length_read(&self) -> Option<u64> {
        (self.header.name.as_str() == "list.len"
            && matches!(self.run, LibRun::Native { .. })
            && self.arity == 1
            && self.limit.is_none()
            && self.prepared.is_none()
            && self.charge.measurements().is_empty())
        .then(|| self.charge.evaluate(|_, _| 0))
    }

    /// The table whose membership test it is (FIG-6820): `map.contains`,
    /// `set.contains` and `record.contains`, unguarded, with no prepared
    /// entry. Its answer is a lookup that builds and reserves nothing, so
    /// an island answers it from the arena, as the native's own call looks
    /// the key up, and charges its formula measured on the values. `None`
    /// for every other function.
    pub(crate) fn membership(&self) -> Option<lash_kernel_doc::plan::Table> {
        use lash_kernel_doc::plan::Table;
        if !matches!(self.run, LibRun::Native { .. })
            || self.arity != 2
            || self.limit.is_some()
            || self.prepared.is_some()
        {
            return None;
        }
        match self.header.name.as_str() {
            "map.contains" => Some(Table::Map),
            "set.contains" => Some(Table::Set),
            "record.contains" => Some(Table::Record),
            _ => None,
        }
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
