//! A kind-pure pair of prepared calls shared by loop regions and islands.

use std::sync::atomic::{AtomicU64, Ordering};

use lash_kernel_doc::{Value, ValueKind};

use super::expr::{Expr, Kind};
use super::{Executable, LibId};

/// Recognizes `same(kind(x), literal)`. Both answers and both formulas
/// depend only on `x`'s kind. Counted invokes keep their ordinary path.
pub(crate) fn test<'e>(
    exe: &Executable,
    lib: LibId,
    args: &'e [Expr],
) -> Option<(&'e Expr, Value)> {
    if !exe.lib(lib).is_same() || exe.lib(lib).prepared.is_none() {
        return None;
    }
    let [
        inner,
        Expr {
            kind: Kind::Const(literal),
            ..
        },
    ] = args
    else {
        return None;
    };
    let Kind::Operator {
        lib, invoke, args, ..
    } = &inner.kind
    else {
        return None;
    };
    (exe.lib(*lib).is_kind()
        && exe.lib(*lib).prepared.is_some()
        && !invoke.is_some_and(|invoke| invoke.call)
        && args.len() == 1
        && matches!(literal, Value::Text(_)))
    .then_some((inner, *literal))
}

/// First general answer per kind, owned by one compiled test site. The
/// cached units exclude the closes, which each execution form replays in
/// its own charge program. Nothing here is saved with a run.
#[derive(Default)]
pub(crate) struct KindMemo([Memo; ValueKind::Ref as usize + 1]);

impl KindMemo {
    #[inline(always)]
    pub(crate) fn get(&self, kind: ValueKind) -> Option<(bool, u64, u64)> {
        let slot = &self.0[kind as usize];
        let head = slot.head.load(Ordering::Acquire);
        (head & 1 != 0).then(|| (head & 2 != 0, head >> 2, slot.same.load(Ordering::Relaxed)))
    }

    pub(crate) fn set(&self, kind: ValueKind, (answer, first, second): (bool, u64, u64)) {
        // Leave an unrepresentable charge to the general path.
        if first < 1 << 62 {
            let slot = &self.0[kind as usize];
            slot.same.store(second, Ordering::Relaxed);
            slot.head
                .store(first << 2 | u64::from(answer) << 1 | 1, Ordering::Release);
        }
    }
}

/// Concurrent fills write identical values: an acquiring reader sees the
/// second formula before the filled bit, even when another fill follows.
#[derive(Default)]
struct Memo {
    head: AtomicU64,
    same: AtomicU64,
}
