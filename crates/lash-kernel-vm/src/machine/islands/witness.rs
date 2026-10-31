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
    static PRECISE_TEST_ITERATIONS: std::cell::Cell<[Option<u64>; 2]> = const { std::cell::Cell::new([None; 2]) };
    static PRECISE_TESTS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
    static SOURCE_LOOP_PUBLICATIONS: std::cell::RefCell<std::collections::BTreeMap<lash_kernel_value::sites::Site, std::collections::BTreeSet<u64>>> = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
    static PRECISE_JOINS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
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
    static EMPTY_TUPLE_CONSTRUCTIONS: std::cell::Cell<[u64;2]> = const { std::cell::Cell::new([0;2]) };
    static PRECISE_CONSTRUCTIONS: std::cell::Cell<[u64;5]> = const { std::cell::Cell::new([0;5]) };
    static SOURCE_REPLAY_PUBLICATIONS: std::cell::Cell<[u64; 4]> = const { std::cell::Cell::new([0; 4]) };
    static SCOPED_BINDINGS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
    static PRECISE_WRITES: std::cell::Cell<[u64;2]> = const { std::cell::Cell::new([0;2]) };
    static PRECISE_SESSION_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static PRECISE_BINDINGS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
    static PRECISE_SOURCE_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TUPLE_CONSTRUCTIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static TABLE_CONSTRUCTIONS: std::cell::Cell<[u64;3]> = const { std::cell::Cell::new([0;3]) };
    static NATIVE_TEXT_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static NATIVE_READ_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static CONSTRUCTION_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static MEMBER_PUBLICATIONS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn begin() {
    PRECISE_TEST_ITERATIONS.with(|counts| counts.set([None; 2]));
    PRECISE_TESTS.with(|counts| counts.set([0; 2]));
    SESSION_PUBLICATIONS.with(|count| count.set(0));
    SCOPE_PUBLICATIONS.with(|count| count.set(0));
    CONSTRUCTION_PUBLICATIONS.with(|count| count.set(0));
    PREPARED_NATIVE_PUBLICATIONS.with(|count| count.set(0));
    NATIVE_LIST_PUBLICATIONS.with(|count| count.set(0));
    NATIVE_TABLE_PUBLICATIONS.with(|counts| counts.set([0; 3]));
    EMPTY_TUPLE_CONSTRUCTIONS.with(|counts| counts.set([0; 2]));
    PRECISE_CONSTRUCTIONS.with(|counts| counts.set([0; 5]));
    PRECISE_BINDINGS.with(|counts| counts.set([0; 2]));
    PRECISE_WRITES.with(|counts| counts.set([0; 2]));
    PRECISE_SESSION_READS.with(|count| count.set(0));
    SCOPED_BINDINGS.with(|counts| counts.set([0; 2]));
    SOURCE_REPLAY_PUBLICATIONS.with(|total| total.set([0; 4]));
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
pub(in crate::machine) fn cell_published() {
    CELL_PUBLICATIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn cell_constructions() -> u64 {
    CELL_CONSTRUCTIONS.with(|count| count.replace(0))
}
pub(in crate::machine) fn cell_constructed() {
    CELL_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn closure_constructions() -> u64 {
    CLOSURE_CONSTRUCTIONS.with(|count| count.replace(0))
}
pub(in crate::machine) fn closure_constructed() {
    CLOSURE_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn session_publications() -> u64 {
    SESSION_PUBLICATIONS.with(|count| count.replace(0))
}
pub(in crate::machine) fn session_published() {
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

#[cfg(test)]
pub(crate) fn precise_constructions() -> [u64; 5] {
    PRECISE_CONSTRUCTIONS.with(|counts| counts.replace([0; 5]))
}

pub(in crate::machine) fn precise_constructed(value: lash_kernel_doc::Value) {
    use lash_kernel_doc::Value;
    let index = match value {
        Value::List(_) => 0,
        Value::Tuple(_) => 1,
        Value::Map(_) => 2,
        Value::Set(_) => 3,
        Value::Record(_) => 4,
        _ => return,
    };
    PRECISE_CONSTRUCTIONS.with(|counts| {
        let mut next = counts.get();
        next[index] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn empty_tuple_constructions() -> [u64; 2] {
    EMPTY_TUPLE_CONSTRUCTIONS.with(|counts| counts.replace([0; 2]))
}

pub(in crate::machine) fn empty_tuple_constructed(precise: bool) {
    EMPTY_TUPLE_CONSTRUCTIONS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(precise)] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn precise_bindings() -> [u64; 2] {
    PRECISE_BINDINGS.with(|counts| counts.replace([0; 2]))
}

pub(in crate::machine) fn precise_binding_published(value: lash_kernel_doc::Value) {
    PRECISE_BINDINGS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(value.handle().is_some())] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn scoped_bindings() -> [u64; 2] {
    SCOPED_BINDINGS.with(|counts| counts.replace([0; 2]))
}

pub(in crate::machine) fn scoped_binding_published(shared: bool) {
    SCOPED_BINDINGS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(shared)] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn source_replay_publications() -> [u64; 4] {
    SOURCE_REPLAY_PUBLICATIONS.with(|total| total.replace([0; 4]))
}

pub(in crate::machine) fn source_replay_published(op: lash_kernel_exec::Op) {
    use lash_kernel_exec::Op;
    let index = match op {
        Op::Const { .. } | Op::Local { .. } | Op::Session { .. } => 0,
        Op::LetBind { .. } => 1,
        Op::LetBindShared { .. } => 2,
        Op::MakeTuple { .. }
        | Op::MakeList { .. }
        | Op::MakeSet { .. }
        | Op::MakeMap { .. }
        | Op::MakeRecord { .. } => 3,
        Op::Closure { .. } => return,
        _ => unreachable!("a replay publication has a prepared source operation"),
    };
    SOURCE_REPLAY_PUBLICATIONS.with(|total| {
        let mut counts = total.get();
        counts[index] += 1;
        total.set(counts);
    });
}

#[cfg(test)]
pub(crate) fn precise_writes() -> [u64; 2] {
    PRECISE_WRITES.with(|counts| counts.replace([0; 2]))
}
#[cfg(test)]
pub(crate) fn precise_session_reads() -> u64 {
    PRECISE_SESSION_READS.with(|count| count.replace(0))
}

pub(in crate::machine) fn precise_write_published(value: lash_kernel_doc::Value) {
    PRECISE_WRITES.with(|counts| {
        let mut next = counts.get();
        next[usize::from(value.handle().is_some())] += 1;
        counts.set(next);
    });
}
pub(in crate::machine) fn precise_session_read_published() {
    PRECISE_SESSION_READS.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
pub(crate) fn precise_joins() -> [u64; 2] {
    PRECISE_JOINS.with(|counts| counts.replace([0; 2]))
}

pub(in crate::machine) fn precise_join_published(scoped: bool) {
    PRECISE_JOINS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(scoped)] += 1;
        counts.set(next);
    });
}

#[cfg(test)]
pub(crate) fn source_loop_publications()
-> std::collections::BTreeMap<lash_kernel_value::sites::Site, std::collections::BTreeSet<u64>> {
    SOURCE_LOOP_PUBLICATIONS.with(|counts| std::mem::take(&mut *counts.borrow_mut()))
}

pub(in crate::machine) fn source_loop_published(loops: &[(lash_kernel_value::sites::Site, u64)]) {
    if !loops.iter().any(|(_, iteration)| *iteration < 2) {
        return;
    }
    SOURCE_LOOP_PUBLICATIONS.with(|counts| {
        let mut counts = counts.borrow_mut();
        for (site, iteration) in loops.iter().filter(|(_, iteration)| *iteration < 2) {
            match counts.get_mut(site) {
                Some(iterations) => {
                    iterations.insert(*iteration);
                }
                None => {
                    counts.insert(site.clone(), [*iteration].into());
                }
            }
        }
    });
}

#[cfg(test)]
pub(crate) fn precise_tests() -> [u64; 2] {
    PRECISE_TESTS.with(|counts| counts.replace([0; 2]))
}

#[cfg(test)]
pub(crate) fn precise_test_iterations() -> [Option<u64>; 2] {
    PRECISE_TEST_ITERATIONS.with(|counts| counts.replace([None; 2]))
}

pub(in crate::machine) fn precise_test_published(taken: bool, iteration: u64) {
    PRECISE_TEST_ITERATIONS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(taken)] = Some(iteration);
        counts.set(next);
    });
    PRECISE_TESTS.with(|counts| {
        let mut next = counts.get();
        next[usize::from(taken)] += 1;
        counts.set(next);
    });
}
