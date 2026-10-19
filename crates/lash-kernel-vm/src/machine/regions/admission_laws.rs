//! A region admits a statement's capacity growth before it allocates for
//! it (`docs/design/kernel-regions.md` §3.5, FIG-6791): a construction its
//! nursery cannot hold leaves the statement to the reference before its
//! members are collected, and an operand window is no capacity growth, so
//! that no Rust layout moves where a statement misses.

use std::alloc::{GlobalAlloc, Layout as Allocation, System};
use std::cell::Cell;
use std::sync::Arc;

use lash_kernel_doc::Value;

use super::NURSERY;
use crate::compile::{CompiledStep, Cx, Executable, Meter};
use crate::laws::embedder::Embedder;

thread_local! {
    /// The bytes this thread allocated while a measurement runs on it.
    static ALLOCATED: Cell<Option<u64>> = const { Cell::new(None) };
}

fn count(bytes: usize) {
    // A const thread-local of `Cell<Option<u64>>` allocates nothing, so
    // counting cannot re-enter the allocator.
    let _ = ALLOCATED.try_with(|allocated| {
        if let Some(sum) = allocated.get() {
            allocated.set(Some(sum.saturating_add(bytes as u64)));
        }
    });
}

struct Counting;

#[global_allocator]
static ALLOCATOR: Counting = Counting;

#[expect(
    unsafe_code,
    reason = "test allocator forwards the unchanged GlobalAlloc contract to System"
)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Allocation) -> *mut u8 {
        count(layout.size());
        // SAFETY: The caller supplies a valid layout; System receives it unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Allocation) -> *mut u8 {
        count(layout.size());
        // SAFETY: The caller supplies a valid layout; System receives it unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Allocation) {
        // SAFETY: Allocations came from System with the same layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Allocation, new_size: usize) -> *mut u8 {
        count(new_size);
        // SAFETY: The pointer, layout and new size keep the caller's contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

/// `work`'s answer and the bytes it allocated on this thread.
fn allocated_during<T>(work: impl FnOnce() -> T) -> (T, u64) {
    ALLOCATED.with(|allocated| allocated.set(Some(0)));
    let answer = work();
    let allocated = ALLOCATED.with(|allocated| allocated.take().unwrap_or_default());
    (answer, allocated)
}

/// `main` calling `f`, whose loop's first statement binds `expression`
/// and whose region is the program's one.
fn program(expression: &str) -> String {
    format!(
        "fn f() {{ while true {{ let xs = {expression} break }} }}\nmain {{ do call f() finish null }}"
    )
}

/// The value node of the first statement of `embedder`'s one region, run
/// with `room` bytes of its nursery left and memory metered: its answer,
/// and what it allocated.
fn first_statement(embedder: &mut Embedder, room: u64) -> (Option<Value>, u64) {
    let machine = &mut embedder.machine;
    let exe: Arc<Executable> = Arc::clone(&machine.exe);
    let [plan] = &exe.regions[..] else {
        panic!("one loop region forms, not {}", exe.regions.len());
    };
    assert!(plan.allocates, "the region's plan allocates");
    let compiled = plan.compiled(&exe).expect("the region compiles");
    let value = match compiled.body.steps.first() {
        Some(CompiledStep::Bind { value, .. } | CompiledStep::Write { value, .. }) => value,
        _ => panic!("the loop's first statement binds"),
    };
    let mut arena = machine.arena.borrow_mut();
    let ceiling = arena.held() + room;
    let mut cx = Cx {
        meter: Meter::new(false),
        charging: exe.charging(),
        args: super::super::registers::Arguments::new(),
        exe: &exe,
        arena: &mut arena,
        sites: &mut machine.sites,
        sessions: &mut [],
        limits: machine.limits,
        memory: true,
        ceiling,
        transient: 0,
        fault: None,
        lets: Vec::new(),
        declined: false,
        held: None,
        charged: machine.charged,
        counters: None,
        guests: None,
        called: None,
        accumulator: None,
    };
    let answer = allocated_during(|| value(&[], &mut cx));
    assert!(cx.fault.is_none(), "the statement met no fault");
    answer
}

/// The least nursery room in which the first statement of `text`'s region
/// answers.
fn least_room(text: &str) -> u64 {
    let mut embedder = Embedder::new(text);
    (0..=4096)
        .find(|room| first_statement(&mut embedder, *room).0.is_some())
        .unwrap_or_else(|| panic!("the statement answers in some room: {text}"))
}

/// The most a construction its nursery refuses may allocate before it
/// leaves its statement: the node tree's own bookkeeping, far below one
/// buffer of its members.
const REFUSAL_ALLOCATION: u64 = 1024;

/// FIG-6782 (the oversized-construction witness of FIG-6764), over
/// capacity (FIG-6791): a list, set, tuple, record or map literal whose
/// least capacity exceeds its region's nursery leaves the statement to the
/// reference before it collects its members. Collecting 8,192 members
/// first allocated 128 KiB past the 64 KiB room the region admitted.
#[test]
fn a_construction_its_nursery_cannot_hold_misses_before_it_collects_its_members() {
    let zeros = |count: usize| vec!["0"; count].join(", ");
    let fields = |count: usize| {
        (0..count)
            .map(|at| format!("f{at}: 0"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let entries = |count: usize| {
        (0..count)
            .map(|at| format!("{at}: 0"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for (what, expression) in [
        ("list", format!("[{}]", zeros(8192))),
        ("set", format!("set{{{}}}", zeros(8192))),
        ("tuple", format!("({})", zeros(8192))),
        ("record", format!("{{{}}}", fields(4096))),
        ("map", format!("map{{{}}}", entries(32))),
    ] {
        let mut embedder = Embedder::new(&program(&expression));
        // The map's 32 entries fit the operand window and a whole nursery:
        // it is refused by a room short of its least capacity, 65 slots.
        let room = if what == "map" { 512 } else { NURSERY };
        let (answer, allocated) = first_statement(&mut embedder, room);
        assert!(answer.is_none(), "the {what} is left to the reference");
        assert!(
            allocated <= REFUSAL_ALLOCATION,
            "the {what} allocated {allocated} bytes before its region refused it"
        );
    }
}

/// FIG-6782: an operand window reserves none of the room, since what it
/// holds is held already: a native call's operands need no room their
/// answer does not, so a Rust layout of [`Value`] or of the window moves
/// no statement's miss.
#[test]
fn an_operand_window_takes_no_room() {
    let through_a_call = least_room(&program("[num.add(1, 2)]"));
    let constant = least_room(&program("[3]"));
    assert_eq!(through_a_call, constant);
}
