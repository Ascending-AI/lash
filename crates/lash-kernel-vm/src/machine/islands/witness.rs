//! Test-only facts emitted at actual kernel commits and exits.

use std::cell::RefCell;
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// A source-certified grouped kernel selected by packet lowering.
pub enum Family {
    /// An entered source suffix published through SourceAccepted.
    SourcePublication,
    /// A checked comparison retains its real empty-tuple source operand.
    SourceUnit,
    /// A fixed-arity scalar packet and its shared destination.
    Scalar,
    /// A source-certified member guard, read and optional consumer.
    GuardedRead,
    /// An owned append preserving the original text until acceptance.
    TextAppend,
    /// A fixed typed repeat graph.
    Repeated,
    /// A member read, scalar consumer and local write.
    ReadUpdate,
    /// Parts construction, join, append and local write.
    FormatAppend,
    /// A stable merge prefix or its post-callback placement.
    Merge,
    /// A list write followed by its source index update.
    Placement,
    /// A cursor item's source hole/branch selection.
    Visit,
    /// A source cursor's item and membership result.
    ReadSlot,
    /// An ordered scalar reduction over source items.
    Reduction,
    /// An item append retaining the source list and member identity.
    CollectionAppend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
/// An observed kernel commit or recovery edge.
pub enum Exit {
    /// A source operation or group committed.
    Commit,
    /// An input or resident guard missed.
    Guard,
    /// The initial source prefix did not fit.
    Prefix,
    /// A repeated graph exhausted its condition.
    End,
    /// An internal source phase stopped after an earlier phase committed.
    Partial,
    /// A committed operation reached a legal source boundary.
    Boundary,
    /// A committed prefix hands off to its guest callback.
    Callback,
}

/// Actual kernel events, indexed by family and exit.
pub type Counts = BTreeMap<(Family, Exit), u64>;

thread_local! {
    static COUNTS: RefCell<Option<Counts>> = const { RefCell::new(None) };
    static SESSION_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static SCOPE_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CLOSURE_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CELL_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CELL_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static RETURN_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PREPARED_NATIVE_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NATIVE_LIST_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NATIVE_TABLE_PUBLICATIONS: std::cell::Cell<[u64;3]> = const { std::cell::Cell::new([0;3]) };
    static PRECISE_SOURCE_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TUPLE_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TABLE_CONSTRUCTIONS: std::cell::Cell<[u64;3]> = const { std::cell::Cell::new([0;3]) };
    static NATIVE_TEXT_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NATIVE_READ_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CONSTRUCTION_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static MEMBER_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn begin() {
    SESSION_PUBLICATIONS.with(|count| count.set(0));
    SCOPE_PUBLICATIONS.with(|count| count.set(0));
    CONSTRUCTION_PUBLICATIONS.with(|count| count.set(0));
    PREPARED_NATIVE_PUBLICATIONS.with(|count| count.set(0));
    NATIVE_LIST_PUBLICATIONS.with(|count| count.set(0));
    NATIVE_TABLE_PUBLICATIONS.with(|counts| counts.set([0; 3]));
    PRECISE_SOURCE_PUBLICATIONS.with(|count| count.set(0));
    TUPLE_CONSTRUCTIONS.with(|count| count.set(0));
    TABLE_CONSTRUCTIONS.with(|counts| counts.set([0; 3]));
    NATIVE_READ_PUBLICATIONS.with(|count| count.set(0));
    NATIVE_TEXT_PUBLICATIONS.with(|count| count.set(0));
    RETURN_PUBLICATIONS.with(|count| count.set(0));
    CELL_PUBLICATIONS.with(|count| count.set(0));
    CELL_CONSTRUCTIONS.with(|count| count.set(0));
    CLOSURE_CONSTRUCTIONS.with(|count| count.set(0));
    MEMBER_PUBLICATIONS.with(|count| count.set(0));
    COUNTS.with(|counts| *counts.borrow_mut() = Some(Counts::new()));
}

pub(crate) fn take() -> Counts {
    COUNTS.with(|counts| counts.borrow_mut().take().unwrap_or_default())
}

pub(super) fn record(family: Family, exit: Exit) {
    COUNTS.with(|counts| {
        if let Some(counts) = &mut *counts.borrow_mut() {
            *counts.entry((family, exit)).or_default() += 1;
        }
    });
}

/// Generic and specialized packets share this source operation. It is not a
/// grouped-kernel witness, so forced-generic checks still reject every family.
#[cfg(test)]
pub(crate) fn scope_publications() -> u64 {
    SCOPE_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn scope_published() {
    SCOPE_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}
#[cfg(test)]
pub(crate) fn member_publications() -> u64 {
    MEMBER_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn member_published() {
    MEMBER_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn construction_publications() -> u64 {
    CONSTRUCTION_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn construction_published() {
    CONSTRUCTION_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn native_read_publications() -> u64 {
    NATIVE_READ_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn native_read_published() {
    NATIVE_READ_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn native_text_publications() -> u64 {
    NATIVE_TEXT_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn native_text_published() {
    NATIVE_TEXT_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn return_publications() -> u64 {
    RETURN_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn return_published() {
    RETURN_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn cell_publications() -> u64 {
    CELL_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn cell_published() {
    CELL_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn cell_constructions() -> u64 {
    CELL_CONSTRUCTIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn cell_constructed() {
    CELL_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn closure_constructions() -> u64 {
    CLOSURE_CONSTRUCTIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn closure_constructed() {
    CLOSURE_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn session_publications() -> u64 {
    SESSION_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn session_published() {
    SESSION_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn prepared_native_publications() -> u64 {
    PREPARED_NATIVE_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn prepared_native_published() {
    PREPARED_NATIVE_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn native_list_publications() -> u64 {
    NATIVE_LIST_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine::islands) fn native_list_published() {
    NATIVE_LIST_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn native_table_publications() -> [u64; 3] {
    NATIVE_TABLE_PUBLICATIONS.with(|counts| counts.replace([0; 3]))
}
pub(in crate::machine::islands) fn native_collection_published(value: lash_kernel_doc::Value) {
    use lash_kernel_doc::Value;
    let index = match value {
        Value::List(_) => {
            native_list_published();
            return;
        }
        Value::Map(_) => 0,
        Value::Set(_) => 1,
        Value::Record(_) => 2,
        _ => return,
    };
    NATIVE_TABLE_PUBLICATIONS.with(|counts| {
        let mut next = counts.get();
        next[index] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn table_constructions() -> [u64; 3] {
    TABLE_CONSTRUCTIONS.with(|counts| counts.replace([0; 3]))
}

pub(in crate::machine::islands) fn collection_constructed(value: lash_kernel_doc::Value) {
    use lash_kernel_doc::Value;
    let index = match value {
        Value::List(_) => {
            construction_published();
            return;
        }
        Value::Tuple(_) => {
            TUPLE_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
            return;
        }
        Value::Map(_) => 0,
        Value::Set(_) => 1,
        Value::Record(_) => 2,
        _ => return,
    };
    TABLE_CONSTRUCTIONS.with(|counts| {
        let mut next = counts.get();
        next[index] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn tuple_constructions() -> u64 {
    TUPLE_CONSTRUCTIONS.with(|count| count.replace(0))
}

#[cfg(test)]
pub(crate) fn precise_source_publications() -> u64 {
    PRECISE_SOURCE_PUBLICATIONS.with(|count| count.replace(0))
}

pub(in crate::machine) fn precise_source_published() {
    PRECISE_SOURCE_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}
